//! `fake-provider`: a stand-in for an OpenAI-compatible model provider, for
//! the model routing smoke's `--fake` run (`docs/smoke/model-route.sh`).
//!
//! It listens on a loopback port the system picks, prints `listening
//! <port>`, and answers every `POST .../chat/completions` with a short
//! Chat Completions stream: the text `fake reply to: <the last user
//! message>`, a stop and usage. When `FAKE_PROVIDER_KEY` is set, a request
//! whose `Authorization` is not `Bearer <that key>` gets 401 instead, so the
//! smoke sees the node send the key it was given. Anything else is a 404.
//! It never prints a request's headers or body.
//!
//! Tests use the `upstream` feature's `FakeUpstream`, which scripts
//! failures too; this binary exists so a shell script can start a provider
//! without a test harness. Standard library only.

use std::fmt::Write as _;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;

use serde_json::{Value, json};

fn main() -> ExitCode {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            complain(&format!("fake-provider: cannot listen: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            complain(&format!("fake-provider: no address: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let said = writeln!(io::stdout().lock(), "listening {port}");
    drop(said);
    let key = std::env::var("FAKE_PROVIDER_KEY").ok();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let key = key.clone();
        drop(std::thread::spawn(move || {
            if let Err(error) = answer(stream, key.as_deref()) {
                complain(&format!("fake-provider: {error}"));
            }
        }));
    }
    ExitCode::SUCCESS
}

fn complain(line: &str) {
    let written = writeln!(io::stderr().lock(), "{line}");
    drop(written);
}

fn answer(stream: TcpStream, key: Option<&str>) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    let _read = reader.read_line(&mut request_line)?;
    let mut length = 0_usize;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        let _read = reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_owned());
            }
        }
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    let mut stream = stream;
    let path = request_line.split(' ').nth(1).unwrap_or_default();
    if !request_line.starts_with("POST ") || !path.ends_with("/chat/completions") {
        return respond(
            &mut stream,
            "404 Not Found",
            "application/json",
            &error("no such endpoint"),
        );
    }
    if let Some(key) = key
        && authorization.as_deref() != Some(format!("Bearer {key}").as_str())
    {
        return respond(
            &mut stream,
            "401 Unauthorized",
            "application/json",
            &error("wrong key"),
        );
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("fake")
        .to_owned();
    let asked = request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .map(|message| message.get("content").map(text_of).unwrap_or_default())
        .unwrap_or_default();
    let reply = format!("fake reply to: {asked}");
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "fake-1", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let usage = json!({
        "id": "fake-1", "object": "chat.completion.chunk", "created": 1, "model": model,
        "choices": [],
        "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17},
    });
    let events = [
        chunk(json!({"role": "assistant", "content": reply}), Value::Null),
        chunk(json!({}), json!("stop")),
        usage,
    ];
    let mut text = String::new();
    for event in events {
        let _infallible = write!(text, "data: {event}\n\n");
    }
    text.push_str("data: [DONE]\n\n");
    respond(&mut stream, "200 OK", "text/event-stream", &text)
}

/// A message's text: a string, or the text parts of a content array.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => String::new(),
    }
}

fn error(message: &str) -> String {
    json!({"error": {"message": message, "type": "invalid_request_error"}}).to_string()
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}
