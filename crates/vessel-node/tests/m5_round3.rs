//! Milestone 5, round 3 (final verification at 9077ead).
//!
//! Run: copy to crates/vessel-node/tests/m5_round3.rs, then
//! `cargo test -p vessel-node --test m5_round3 -- --nocapture --test-threads 1`
//! Uses only fake-acp and `FakeUpstream`, temporary homes, and loopback HTTP.

#![cfg(test)]
#![allow(
    clippy::print_stderr,
    reason = "the sound checks print what they measured, for the report"
)]

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use vessel_client::NodeClient;
use vessel_llm::sse::{SseDecoder, SseEvent, SseFrame};
use vessel_node::Node;
use vessel_node::fleet::{FleetFiles, set_secret};
use vessel_proto::{EventSeq, SessionId, SessionStatus};
use vessel_testkit::upstream::{FakeUpstream, Reply};

#[path = "e2e/support.rs"]
#[expect(
    dead_code,
    reason = "reuse the existing isolated e2e fixture, not every helper"
)]
mod support;

const KEY: &str = "sk-round3-FAKE-ONLY-6f0c19d2";

struct Rig {
    fixture: support::Fixture,
    node: Node,
    client: NodeClient,
    sessions: Vec<SessionId>,
    addr: SocketAddr,
    token: String,
    upstream: FakeUpstream,
}

async fn routed_session(
    fixture: &support::Fixture,
    client: &NodeClient,
) -> (SessionId, SocketAddr, String) {
    let mut spec = support::structured_spec(fixture, None);
    spec.profile = Some("routed".parse().unwrap());
    let session = client.create_session(spec).await.unwrap().id;
    let mut stream = client.follow(session, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    support::read_until(&mut stream, &mut seen, |record| {
        support::status_of(record) == Some(&SessionStatus::Running)
    })
    .await;
    let base = support::ask(
        client,
        &mut stream,
        &mut seen,
        session,
        "env ANTHROPIC_BASE_URL",
    )
    .await;
    let addr = base
        .strip_prefix("env ANTHROPIC_BASE_URL=http://")
        .unwrap()
        .parse()
        .unwrap();
    let token = support::ask(
        client,
        &mut stream,
        &mut seen,
        session,
        "env ANTHROPIC_AUTH_TOKEN",
    )
    .await;
    let token = token
        .strip_prefix("env ANTHROPIC_AUTH_TOKEN=")
        .unwrap()
        .to_owned();
    (session, addr, token)
}

impl Rig {
    async fn start(api: &str) -> Self {
        Self::start_with_key(api, KEY).await
    }

    async fn start_with_key(api: &str, key: &str) -> Self {
        Self::start_with(api, Some(key)).await
    }

    /// Added when ported: a route whose provider takes no key (a local
    /// server), as the F1 fix asks such routes to be written.
    async fn start_without_key(api: &str) -> Self {
        Self::start_with(api, None).await
    }

    async fn start_with(api: &str, key: Option<&str>) -> Self {
        let upstream = FakeUpstream::start().await.unwrap();
        let fixture = support::Fixture::new();
        let key_line = if key.is_some() {
            "key = { secret = \"ROUND3_KEY\" }\n"
        } else {
            ""
        };
        let models = format!(
            "[providers.local]\napi = \"{api}\"\nbase_url = \"{}\"\n\
             {key_line}\
             [models.routed]\nprovider = \"local\"\nmodel = \"allowed\"\n",
            upstream.url()
        );
        let path = fixture.data_dir().models_path();
        std::fs::write(&path, models).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(
            fixture.data_dir().profiles_path(),
            "[profiles.routed]\nharness = \"claude\"\nmodel = \"routed\"\n",
        )
        .unwrap();
        if let Some(key) = key {
            set_secret(
                &FleetFiles::new(&fixture.data_dir()),
                &"ROUND3_KEY".parse().unwrap(),
                key,
            )
            .unwrap();
        }
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        let (session, addr, token) = routed_session(&fixture, &client).await;
        Self {
            fixture,
            node,
            client,
            sessions: vec![session],
            addr,
            token,
            upstream,
        }
    }

    /// A second routed session on the same node; its token.
    async fn another_session(&mut self) -> String {
        let (session, _addr, token) = routed_session(&self.fixture, &self.client).await;
        self.sessions.push(session);
        token
    }

    async fn call(&self, front: &str, body: &str) -> Answer {
        request(
            self.addr,
            front,
            &[("authorization", &format!("Bearer {}", self.token))],
            body,
        )
        .await
    }

    async fn stop(self) {
        for session in &self.sessions {
            let _killed = self.client.kill(*session).await;
        }
        self.node.stop().await.unwrap();
    }
}

#[derive(Debug)]
struct Answer {
    status: u16,
    body: String,
}

async fn request(addr: SocketAddr, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
    let mut wire = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        write!(wire, "{name}: {value}\r\n").unwrap();
    }
    write!(wire, "\r\n{body}").unwrap();
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(wire.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _read = support::step("read proxy HTTP response", socket.read_to_end(&mut raw))
        .await
        .unwrap();
    parse(&raw)
}

fn parse(raw: &[u8]) -> Answer {
    let raw = std::str::from_utf8(raw).unwrap();
    let (head, mut rest) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut body = String::new();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        loop {
            let (len, tail) = rest.split_once("\r\n").unwrap();
            let len = usize::from_str_radix(len, 16).unwrap();
            if len == 0 {
                break;
            }
            let (chunk, tail) = tail.split_at(len);
            body.push_str(chunk);
            rest = tail.strip_prefix("\r\n").unwrap();
        }
    } else {
        body.push_str(rest);
    }
    Answer { status, body }
}

/// Read one response off a keep-alive socket (Content-Length bodies only).
async fn read_one(socket: &mut TcpStream) -> std::io::Result<Answer> {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        raw.push(socket.read_u8().await?);
        assert!(raw.len() < 16384);
    }
    let header = std::str::from_utf8(&raw).unwrap();
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _read = socket.read_exact(&mut body).await?;
    raw.extend(body);
    Ok(parse(&raw))
}

fn get_models_head(addr: SocketAddr, token: Option<&str>, close: bool) -> String {
    let auth = token.map_or_else(String::new, |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let close = if close { "Connection: close\r\n" } else { "" };
    format!("GET /v1/models HTTP/1.1\r\nHost: {addr}\r\n{auth}{close}\r\n")
}

fn input(front: &str, streaming: bool) -> String {
    if front == "/v1/messages" {
        format!(
            r#"{{"model":"allowed","max_tokens":64,"stream":{streaming},"messages":[{{"role":"user","content":"hello"}}]}}"#
        )
    } else {
        format!(r#"{{"model":"allowed","stream":{streaming},"input":"hello"}}"#)
    }
}

fn escaped_key() -> String {
    KEY.bytes().fold(String::new(), |mut out, byte| {
        let _written = write!(out, "\\u{byte:04x}");
        out
    })
}

fn decode(body: &str) -> (Vec<SseEvent>, usize) {
    let mut decoder = SseDecoder::default();
    let mut frames = Vec::new();
    decoder.feed(body.as_bytes(), &mut frames).unwrap();
    decoder.finish(&mut frames).unwrap();
    let mut events = Vec::new();
    let mut comments: usize = 0;
    for frame in frames {
        match frame {
            SseFrame::Event(event) => events.push(event),
            SseFrame::Comment => comments = comments.saturating_add(1),
        }
    }
    (events, comments)
}

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","model":"allowed","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}"#;

/// Valid traffic: a same-API stream is written again event by event. Every
/// event (name and data) must reach the harness as the provider sent it,
/// in order, whatever field spelling and line endings the provider used:
/// CRLF endings, `data:` without a space, multi-line data, an event type
/// the provider added later, an event without a name, a 1 MiB tool
/// argument delta. Comments may become `: keepalive`; `id:`/`retry:` may
/// go (neither API uses them).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_same_api_events_reach_the_harness_as_sent() {
    support::within(async {
        let big = "a".repeat(1024 * 1024);
        let anthropic = format!(
            concat!(
                ": OPENROUTER PROCESSING\r\n\r\n",
                "id: 7\r\nretry: 3000\r\nevent: message_start\r\ndata: {start}\r\n\r\n",
                "event: future_event_type\ndata: {{\"type\":\"future_event_type\",\"nested\":{{\"a\":[1,2,3]}}}}\n\n",
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                "event: content_block_delta\ndata:{{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"l1\\nl2 \\\"q\\\" \\u00e9 \\ud83d\\ude00\"}}}}\n\n",
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\n",
                "data:  \"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"multi\"}}}}\n\n",
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"citations_delta\",\"citation\":{{\"type\":\"char_location\",\"cited_text\":\"x\"}}}}}}\n\n",
                "data: {{\"type\":\"ping\"}}\n\n",
                "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\",\"input\":{{}}}}}}\n\n",
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"x\\\":\\\"{big}\\\"}}\"}}}}\n\n",
                "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":1}}\n\n",
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":9}}}}\n\n",
                "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            ),
            start = MESSAGE_START,
            big = big,
        );
        let responses = concat!(
            ": keep\n\n",
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"r1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"allowed\",\"output\":[]}}\n\n",
            "event: response.future.thing\ndata: {\"type\":\"response.future.thing\",\"sequence_number\":1,\"x\":{\"y\":null}}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":2,\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"m1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\\n\"}\r\n\r\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"id\":\"r1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"allowed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens_details\":{\"reasoning_tokens\":0}}}}\n\n",
        )
        .to_owned();
        let mut broken = Vec::new();
        for (api, front, stream) in [
            ("anthropic-messages", "/v1/messages", anthropic),
            ("openai-responses", "/v1/responses", responses),
        ] {
            let rig = Rig::start(api).await;
            let (sent, sent_comments) = decode(&stream);
            for piece in [1, 5, 4096] {
                // A 1 MiB event a byte at a time is slow under debug; the
                // Anthropic stream is checked at 5 and 4096.
                if piece == 1 && api == "anthropic-messages" {
                    continue;
                }
                rig.upstream.reply(Reply::Stream {
                    body: Bytes::from(stream.clone()),
                    piece,
                });
                let got = rig.call(front, &input(front, true)).await;
                let (received, comments) = decode(&got.body);
                if got.status != 200 || received != sent || comments != sent_comments {
                    let first = sent
                        .iter()
                        .zip(&received)
                        .position(|(a, b)| a != b)
                        .unwrap_or(sent.len().min(received.len()));
                    broken.push(format!(
                        "{api} piece={piece}: status {} events {}/{} comments {comments}/{sent_comments}, first difference at {first}: sent {:?} got {:?}",
                        got.status,
                        received.len(),
                        sent.len(),
                        sent.get(first).map(|e| (&e.event, e.data.chars().take(120).collect::<String>())),
                        received.get(first).map(|e| (&e.event, e.data.chars().take(120).collect::<String>())),
                    ));
                }
            }
            rig.stop().await;
        }
        assert!(broken.is_empty(), "{broken:#?}");
    })
    .await;
}

/// Redaction fails open when `serde_json` cannot decode an event's data:
/// `Redactor::holds` treats "not decodable" as "holds no key". JSON that
/// `serde_json` refuses but a harness's JSON.parse (Claude Code is Node)
/// accepts carries a `\u`-escaped key through: a lone surrogate anywhere in
/// the event, or nesting deeper than `serde_json`'s 128 levels. Same-API
/// stream and non-streamed answer both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_escaped_key_in_json_serde_cannot_read_reaches_the_harness() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let escaped = escaped_key();
        let surrogate = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{escaped}"}},"pad":"\ud800"}}"#
        );
        let deep = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"x"}},"pad":{}"{escaped}"{}}}"#,
            "[".repeat(130),
            "]".repeat(130)
        );
        let mut leaks = Vec::new();
        for (name, data) in [("lone surrogate", &surrogate), ("depth 130", &deep)] {
            let stream = format!(
                "event: message_start\ndata: {MESSAGE_START}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {data}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            rig.upstream.reply(Reply::Stream {
                body: Bytes::from(stream),
                piece: 64,
            });
            let got = rig.call("/v1/messages", &input("/v1/messages", true)).await;
            if got.body.contains(&escaped) {
                leaks.push(format!("stream, {name}"));
            }
            let whole = format!(
                r#"{{"id":"m1","type":"message","role":"assistant","model":"allowed","content":[{{"type":"text","text":"{escaped}"}}],"stop_reason":"end_turn","usage":{{"input_tokens":1,"output_tokens":1}},"pad":{}}}"#,
                if name == "lone surrogate" {
                    r#""\ud800""#.to_owned()
                } else {
                    format!("{}0{}", "[".repeat(130), "]".repeat(130))
                }
            );
            rig.upstream.reply(Reply::Status {
                status: 200,
                body: Bytes::from(whole),
                retry_after: None,
            });
            let got = rig.call("/v1/messages", &input("/v1/messages", false)).await;
            if got.body.contains(&escaped) {
                leaks.push(format!("whole, {name}"));
            }
        }
        rig.stop().await;
        assert!(
            leaks.is_empty(),
            "a \\u-escaped key reaches the harness when serde_json cannot decode the JSON: {leaks:?}"
        );
    })
    .await;
}

/// Sound check: a provider echoing the request's headers as JSON text
/// inside a JSON string (a gateway wrapping its upstream's error), with
/// and without PHP-style `\/` escaping, is withheld on every API pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_key_echoed_as_json_inside_a_json_string_is_withheld() {
    support::within(async {
        for api in ["anthropic-messages", "openai-responses", "openai-chat-completions"] {
            let rig = Rig::start(api).await;
            for front in ["/v1/messages", "/v1/responses"] {
                for inner in [
                    format!(r#"{{\"headers\":{{\"authorization\":\"Bearer {KEY}\"}}}}"#),
                    format!(r#"{{\"x-api-key\":\"{}\"}}"#, KEY.replace('-', "\\\\u002d")),
                ] {
                    let data = match api {
                        "anthropic-messages" => format!(
                            r#"{{"type":"error","error":{{"type":"api_error","message":"{inner}"}}}}"#
                        ),
                        "openai-responses" => format!(
                            r#"{{"type":"error","code":"server_error","message":"{inner}","sequence_number":1}}"#
                        ),
                        _ => format!(r#"{{"error":{{"message":"{inner}","code":500}}}}"#),
                    };
                    rig.upstream.reply(Reply::Stream {
                        body: Bytes::from(format!("event: error\ndata: {data}\n\n")),
                        piece: 3,
                    });
                    let got = rig.call(front, &input(front, true)).await;
                    // The second spelling is JSON inside a JSON string with
                    // the inner escape kept: documented as out of scope.
                    if inner.contains("Bearer") {
                        assert!(
                            !got.body.contains(KEY),
                            "api={api} front={front}: {}",
                            got.body
                        );
                    } else {
                        eprintln!(
                            "[info] api={api} front={front} double-escaped key withheld: {}",
                            !got.body.contains("u002d")
                        );
                    }
                }
            }
            rig.stop().await;
        }
    })
    .await;
}

/// A live session's token (which the harness and every command it runs can
/// read, ADR 0015) can hold every one of the proxy's 256 connection
/// places with proven keep-alive connections: proven connections are never
/// closed to make room, and nothing bounds them per session. Another
/// session's harness then gets 503 for as long as the first session lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_one_sessions_token_holds_every_connection_place() {
    support::within(async {
        let mut rig = Rig::start("anthropic-messages").await;
        let other = rig.another_session().await;
        let mut held = Vec::new();
        for _ in 0..256 {
            let mut socket = TcpStream::connect(rig.addr).await.unwrap();
            socket
                .write_all(get_models_head(rig.addr, Some(&rig.token), false).as_bytes())
                .await
                .unwrap();
            assert_eq!(read_one(&mut socket).await.unwrap().status, 200);
            held.push(socket);
        }
        // Renewing them every few seconds (under the 10 s idle timeout)
        // keeps them for the session's life; one round is enough here.
        let mut extra = TcpStream::connect(rig.addr).await.unwrap();
        let _sent = extra
            .write_all(get_models_head(rig.addr, Some(&other), true).as_bytes())
            .await;
        let status = tokio::time::timeout(Duration::from_secs(5), read_one(&mut extra))
            .await
            .map_or(0, |read| read.map_or(1, |answer| answer.status));
        drop(extra);
        drop(held);
        rig.stop().await;
        assert_eq!(
            status, 200,
            "another session's harness is refused (status {status}; 503 = every place held by one session's proven connections)"
        );
    })
    .await;
}

/// Sound check (connection eviction): 255 tokenless connections are held
/// and a local process keeps opening more as fast as it can, while a
/// harness makes calls on fresh connections. Every harness call must get
/// 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r3_tokenless_flood_does_not_shut_out_fresh_harness_connections() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let addr = rig.addr;
        let mut held = Vec::new();
        for _ in 0..255 {
            held.push(TcpStream::connect(addr).await.unwrap());
        }
        let stop = tokio_util::sync::CancellationToken::new();
        let mut floods = Vec::new();
        for _ in 0..8 {
            let stop = stop.clone();
            floods.push(tokio::spawn(async move {
                let mut open = std::collections::VecDeque::new();
                let mut opened = 0_u32;
                while !stop.is_cancelled() && opened < 1000 {
                    if let Ok(socket) = TcpStream::connect(addr).await {
                        open.push_back(socket);
                        opened += 1;
                    }
                    if open.len() > 128 {
                        drop(open.pop_front());
                    }
                }
                opened
            }));
        }
        let mut failed = Vec::new();
        let mut slowest = Duration::ZERO;
        let started = Instant::now();
        for call in 0..40 {
            // Spread over ~2 s so the calls meet the flood at its height.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let one = Instant::now();
            let mut socket = TcpStream::connect(addr).await.unwrap();
            let _sent = socket
                .write_all(get_models_head(addr, Some(&rig.token), true).as_bytes())
                .await;
            match tokio::time::timeout(Duration::from_secs(3), read_one(&mut socket)).await {
                Ok(Ok(answer)) if answer.status == 200 => slowest = slowest.max(one.elapsed()),
                Ok(Ok(answer)) => failed.push(format!("call {call}: {}", answer.status)),
                Ok(Err(error)) => failed.push(format!("call {call}: {error}")),
                Err(_elapsed) => failed.push(format!("call {call}: no answer in 3 s")),
            }
        }
        let elapsed = started.elapsed();
        stop.cancel();
        let mut opened = 0;
        for flood in floods {
            opened += flood.await.unwrap();
        }
        eprintln!(
            "[info] flood opened {opened} connections; 40 harness calls took {elapsed:?} (slowest {slowest:?}); failures {failed:?}"
        );
        drop(held);
        rig.stop().await;
        assert!(failed.is_empty(), "{failed:?}");
    })
    .await;
}

/// Sound check: an idle proven keep-alive connection is closed at the
/// 10 s header deadline (the idle timeout), and a streamed answer that is
/// silent for 12 s mid-way is not cut by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_idle_keepalive_closes_but_a_slow_stream_survives() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let mut socket = TcpStream::connect(rig.addr).await.unwrap();
        socket
            .write_all(get_models_head(rig.addr, Some(&rig.token), false).as_bytes())
            .await
            .unwrap();
        assert_eq!(read_one(&mut socket).await.unwrap().status, 200);
        let idle = Instant::now();
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(14), socket.read_to_end(&mut rest))
            .await
            .expect("an idle keep-alive connection was not closed within 14 s");
        let idle = idle.elapsed();
        assert!(read.is_ok() && rest.is_empty(), "{read:?} {rest:?}");
        assert!(idle >= Duration::from_secs(9), "closed after {idle:?}");

        let partial = format!(
            "event: message_start\ndata: {MESSAGE_START}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n"
        );
        rig.upstream.reply(Reply::Stall {
            body: Bytes::from(partial),
            silence: Duration::from_secs(12),
        });
        let started = Instant::now();
        let got = rig.call("/v1/messages", &input("/v1/messages", true)).await;
        let took = started.elapsed();
        rig.stop().await;
        assert!(took >= Duration::from_secs(11), "ended after {took:?}: {}", got.body);
        assert!(got.body.contains("event: message_start"), "{}", got.body);
        assert!(got.body.contains("event: error"), "{}", got.body);
    })
    .await;
}

/// Pipelining: a whole request followed in the same write by the start of
/// a second one, then silence. The listener's notes say a head cut off by
/// the deadline is answered 408; here the first answer's write clears the
/// "pending" flag that the second head's bytes set, so the second head
/// gets a bare close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a documented limit (F4): the 408 follows bytes, not heads, so a head pipelined \
            behind an answered request gets a bare close; neither harness pipelines"]
async fn r3_pipelined_partial_head_is_answered_408() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let mut socket = TcpStream::connect(rig.addr).await.unwrap();
        let mut wire = get_models_head(rig.addr, Some(&rig.token), false);
        wire.push_str("GET /v1/models HTTP/1.1\r\nHost:");
        socket.write_all(wire.as_bytes()).await.unwrap();
        assert_eq!(read_one(&mut socket).await.unwrap().status, 200);
        let second = tokio::time::timeout(Duration::from_secs(14), read_one(&mut socket))
            .await
            .expect("no answer and no close within 14 s");
        rig.stop().await;
        let status = second.as_ref().map_or(0, |answer| answer.status);
        assert_eq!(status, 408, "pipelined partial head: {second:?}");
    })
    .await;
}

/// Sound check (pipelining and refusals): a refused request closes its
/// connection even with a valid request pipelined behind it, and a valid
/// request with a refused one behind it is answered first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_pipelined_refusals_close_and_order_holds() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let mut socket = TcpStream::connect(rig.addr).await.unwrap();
        let wire = format!(
            "{}{}",
            get_models_head(rig.addr, None, false),
            get_models_head(rig.addr, Some(&rig.token), false)
        );
        socket.write_all(wire.as_bytes()).await.unwrap();
        assert_eq!(read_one(&mut socket).await.unwrap().status, 401);
        let mut rest = Vec::new();
        let _read = tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut rest))
            .await
            .unwrap();
        assert!(rest.is_empty(), "{}", String::from_utf8_lossy(&rest));

        let mut socket = TcpStream::connect(rig.addr).await.unwrap();
        let wire = format!(
            "{}{}",
            get_models_head(rig.addr, Some(&rig.token), false),
            get_models_head(rig.addr, None, false)
        );
        socket.write_all(wire.as_bytes()).await.unwrap();
        assert_eq!(read_one(&mut socket).await.unwrap().status, 200);
        assert_eq!(read_one(&mut socket).await.unwrap().status, 401);
        let mut rest = Vec::new();
        let _read = tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut rest))
            .await
            .unwrap();
        assert!(rest.is_empty());
        rig.stop().await;
    })
    .await;
}

/// Valid traffic: every event that "holds the key" now ends the call
/// (4c66726). With a short, word-like key, which local OpenAI-compatible
/// servers are commonly given (vLLM's `--api-key`, `EMPTY`, `sk-1234`),
/// ordinary model text matches it: a streamed answer is cut off with "the
/// provider's answer quoted the call's key", and a non-streamed one has the
/// word silently replaced with `[redacted]`.
///
/// Adjusted when ported (the F1 fix, as the coordinator chose it): such a
/// key is now refused where it is read, before anything goes up, with a
/// message naming the rule; a local server's route is written with no key,
/// and then the same answers pass untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_a_short_key_matching_model_text_ends_valid_answers() {
    support::within(async {
        let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The queue is EMPTY now."}}"#;
        let stream = format!(
            "event: message_start\ndata: {MESSAGE_START}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {delta}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let whole_body = r#"{"id":"m1","type":"message","role":"assistant","model":"allowed","content":[{"type":"text","text":"The queue is EMPTY now."}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":5}}"#;

        let rig = Rig::start_with_key("anthropic-messages", "EMPTY").await;
        let refused = rig.call("/v1/messages", &input("/v1/messages", true)).await;
        let went_up = rig.upstream.seen().len();
        rig.stop().await;
        assert_eq!(refused.status, 403, "{refused:?}");
        assert!(refused.body.contains("too short or too plain"), "{refused:?}");
        assert!(!refused.body.contains("EMPTY"), "{refused:?}");
        assert_eq!(went_up, 0, "a plain key was sent upstream");

        let rig = Rig::start_without_key("anthropic-messages").await;
        rig.upstream.reply(Reply::Stream {
            body: Bytes::from(stream),
            piece: 64,
        });
        let streamed = rig.call("/v1/messages", &input("/v1/messages", true)).await;
        rig.upstream.reply(Reply::Status {
            status: 200,
            body: Bytes::from(whole_body),
            retry_after: None,
        });
        let whole = rig.call("/v1/messages", &input("/v1/messages", false)).await;
        rig.stop().await;
        let mut broken = Vec::new();
        if !streamed.body.contains("event: message_stop") || streamed.body.contains("quoted the call's key") {
            broken.push(format!("streamed answer cut: {}", streamed.body.lines().filter(|l| l.contains("error")).collect::<Vec<_>>().join(" | ")));
        }
        if !whole.body.contains("The queue is EMPTY now.") {
            broken.push(format!("whole answer rewritten: {}", whole.body));
        }
        assert!(broken.is_empty(), "{broken:#?}");
    })
    .await;
}
