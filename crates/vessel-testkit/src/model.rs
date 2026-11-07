//! A harness's model call, as the fakes make it (milestone 5): the fakes
//! read where a routed session's harness is pointed (Claude Code's gateway
//! variables, or Codex's `vessel` provider in `-c` overrides and its token
//! variable), make one streamed call there, and say what came back, so
//! end-to-end tests check the node's real wiring without a real harness.
//!
//! [Decision Log]
//! - Purpose: `model <text>` in `fake-acp` (a prompt) and `fake-tui` (a
//!   line) is a model call through the node's proxy.
//! - Constraint: the fakes are small binaries without an HTTP stack (the
//!   `upstream` feature's server is for tests only), and the proxy is on
//!   plain-HTTP loopback.
//! - Choice: HTTP/1.1 over `std::net::TcpStream`, `Connection: close`,
//!   the answer read to its end, de-chunked, and its Server-Sent Events
//!   read for the text deltas of the harness's API. Enough for a fake; a
//!   harness's real client is not what is under test.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};

/// How long a call may take before the fake gives up.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Make one model call with `text` as the user's message, as the harness
/// this process stands in for would, and say what came back: `model
/// <text>`, or `model error: <why>`.
#[must_use]
pub fn call(text: &str) -> String {
    match target().and_then(|target| send(&target, text)) {
        Ok(reply) => format!("model {reply}"),
        Err(error) => format!("model error: {error}"),
    }
}

/// Which API, where, with what.
struct Target {
    anthropic: bool,
    /// `host:port`.
    authority: String,
    path: String,
    token: String,
    model: String,
}

/// Claude Code's variables, else Codex's provider.
fn target() -> Result<Target, String> {
    if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL") {
        let (authority, prefix) = split_url(&base)?;
        return Ok(Target {
            anthropic: true,
            authority,
            path: format!("{prefix}/v1/messages"),
            token: std::env::var("ANTHROPIC_AUTH_TOKEN")
                .map_err(|_unset| "ANTHROPIC_AUTH_TOKEN is not set".to_owned())?,
            model: std::env::var("ANTHROPIC_MODEL").unwrap_or_default(),
        });
    }
    let token = std::env::var("VESSEL_MODEL_SECRET")
        .map_err(|_unset| "not pointed at a model proxy".to_owned())?;
    let args: Vec<String> = std::env::args().collect();
    let provider = args
        .iter()
        .find_map(|arg| arg.strip_prefix("model_providers.vessel="))
        .ok_or("no `vessel` provider in the arguments")?;
    let base = quoted_after(provider, "base_url").ok_or("the provider has no base_url")?;
    let model = args
        .iter()
        .find_map(|arg| arg.strip_prefix("model="))
        .map(|value| value.trim_matches('"').to_owned())
        .unwrap_or_default();
    let (authority, prefix) = split_url(&base)?;
    Ok(Target {
        anthropic: false,
        authority,
        path: format!("{prefix}/responses"),
        token,
        model,
    })
}

/// `http://host:port/prefix` as `(host:port, /prefix)`.
fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("not a plain-HTTP URL: {url}"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.trim_end_matches('/');
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    Ok((authority.to_owned(), prefix))
}

/// The string after `key = "` in a TOML inline table.
fn quoted_after(table: &str, key: &str) -> Option<String> {
    let start = table.find(&format!("{key} = \""))?;
    let rest = table.get(start..)?.split_once('"')?.1;
    Some(rest.split_once('"')?.0.to_owned())
}

fn send(target: &Target, text: &str) -> Result<String, String> {
    let body = if target.anthropic {
        json!({
            "model": target.model,
            "max_tokens": 256,
            "stream": true,
            "messages": [{"role": "user", "content": text}],
        })
    } else {
        json!({
            "model": target.model,
            "stream": true,
            "input": [{"role": "user", "content": text}],
        })
    }
    .to_string();
    let mut request = format!(
        "POST {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nauthorization: Bearer {}\r\n",
        target.path,
        target.authority,
        body.len(),
        target.token
    );
    if target.anthropic {
        request.push_str("anthropic-version: 2023-06-01\r\n");
    }
    request.push_str("\r\n");
    request.push_str(&body);
    let mut stream = TcpStream::connect(&target.authority).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    let _read = stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("no end of headers")?;
    let (head, rest) = response.split_at(split);
    let head = String::from_utf8_lossy(head);
    let rest = rest.get(4..).unwrap_or_default();
    let status = head.split(' ').nth(1).unwrap_or("?");
    let chunked = head.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });
    let body = if chunked {
        dechunk(rest)?
    } else {
        rest.to_vec()
    };
    let body = String::from_utf8_lossy(&body);
    if status != "200" {
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| body.into_owned());
        return Err(format!("{status} {message}"));
    }
    read_events(&body, target.anthropic)
}

/// A chunked body's bytes.
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let end = rest
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .ok_or("a cut chunk")?;
        let (size, after) = rest.split_at(end);
        let size = std::str::from_utf8(size).map_err(|error| error.to_string())?;
        let size = usize::from_str_radix(size.trim(), 16).map_err(|error| error.to_string())?;
        if size == 0 {
            return Ok(body);
        }
        let after = after.get(2..).ok_or("a cut chunk")?;
        body.extend_from_slice(after.get(..size).ok_or("a short chunk")?);
        rest = after
            .get(size..)
            .and_then(|after| after.strip_prefix(b"\r\n"))
            .ok_or("a chunk without its end")?;
    }
}

/// The text the stream carries, or the error it ends with.
fn read_events(body: &str, anthropic: bool) -> Result<String, String> {
    let mut text = String::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match (anthropic, kind) {
            (true, "content_block_delta") => {
                text.push_str(
                    event
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            (true, "error") => {
                return Err(event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("an error event")
                    .to_owned());
            }
            (false, "response.output_text.delta") => {
                text.push_str(
                    event
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            (false, "response.failed") => {
                return Err(event
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response.failed")
                    .to_owned());
            }
            _ => {}
        }
    }
    Ok(text)
}
