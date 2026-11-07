//! Run: copy to crates/vessel-node/tests/m5_verify_proxy.rs, then
//! `cargo test -p vessel-node --test m5_verify_proxy -- --nocapture`
//! Uses only fake-acp and `FakeUpstream`, temporary homes, and loopback HTTP.

#![cfg(test)]

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use vessel_client::NodeClient;
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

const KEY: &str = "sk-review-FAKE-ONLY-1b37a825";
const CAP: usize = 32 * 1024 * 1024;

struct Rig {
    _fixture: support::Fixture,
    node: Node,
    client: NodeClient,
    session: SessionId,
    addr: SocketAddr,
    token: String,
    upstream: FakeUpstream,
}

impl Rig {
    async fn start(api: &str) -> Self {
        let upstream = FakeUpstream::start().await.unwrap();
        let fixture = support::Fixture::new();
        let models = format!(
            "[providers.local]\napi = \"{api}\"\nbase_url = \"{}\"\n\
             key = {{ secret = \"REVIEW_KEY\" }}\n\
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
        set_secret(
            &FleetFiles::new(&fixture.data_dir()),
            &"REVIEW_KEY".parse().unwrap(),
            KEY,
        )
        .unwrap();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        let mut spec = support::structured_spec(&fixture, None);
        spec.profile = Some("routed".parse().unwrap());
        let session = client.create_session(spec).await.unwrap().id;
        let mut stream = client.follow(session, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        support::read_until(&mut stream, &mut seen, |record| {
            support::status_of(record) == Some(&SessionStatus::Running)
        })
        .await;
        let base = support::ask(
            &client,
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
            &client,
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
        Self {
            _fixture: fixture,
            node,
            client,
            session,
            addr,
            token,
            upstream,
        }
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
        self.client.kill(self.session).await.unwrap();
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
fn decoded_data_has_key(body: &str) -> bool {
    let mut decoder = vessel_llm::sse::SseDecoder::default();
    let mut frames = Vec::new();
    decoder.feed(body.as_bytes(), &mut frames).unwrap();
    decoder.finish(&mut frames).unwrap();
    frames.into_iter().any(|frame| match frame {
        vessel_llm::sse::SseFrame::Event(event) => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&event.data);
            parsed.is_ok_and(|value| value.to_string().contains(KEY))
                || event.event.is_some_and(|name| name.contains(KEY))
        }
        vessel_llm::sse::SseFrame::Comment => false,
    })
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_redaction_cannot_depend_on_raw_error_spelling_or_sse_field() {
    support::within(async {
        let rig = Rig::start("openai-responses").await;
        let escaped = escaped_key();
        let cases = [
            ("escaped-error-discriminator", format!(r#"data: {{"type":"\u0065rror","code":"server_error","message":"{escaped}","sequence_number":1}}"#)),
            ("event-name", format!("event: error-{KEY}\ndata: {{\"type\":\"error\",\"code\":\"server_error\",\"message\":\"bad\",\"sequence_number\":1}}")),
            ("escaped-success-text", format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{escaped}\",\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"sequence_number\":1}}")),
        ];
        let mut leaks = Vec::new();
        for (name, event) in cases {
            for piece in [1, 2, 3, 7, 31, 1024] {
                rig.upstream.reply(Reply::Stream { body: Bytes::from(format!("{event}\n\n")), piece });
                let got = rig.call("/v1/responses", &input("/v1/responses", true)).await;
                if decoded_data_has_key(&got.body) { leaks.push(format!("{name}, piece={piece}")); }
            }
        }
        rig.stop().await;
        assert!(leaks.is_empty(), "provider key reaches harness after SSE/JSON decoding: {leaks:?}");
    }).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_escaped_json_member_names_are_redacted() {
    support::within(async {
        let rig = Rig::start("openai-responses").await;
        let body = format!(r#"{{"status":"failed","error":{{"message":"bad"}},"metadata":{{"{}":"echoed credential"}}}}"#, escaped_key());
        rig.upstream.reply(Reply::Status { status: 200, body: Bytes::from(body), retry_after: None });
        let got = rig.call("/v1/responses", &input("/v1/responses", false)).await;
        let decoded: serde_json::Value = serde_json::from_str(&got.body).unwrap();
        rig.stop().await;
        assert!(!decoded.to_string().contains(KEY), "escaped provider key survives as a JSON member name");
    }).await;
}
/// Read one response without waiting for a keepalive connection to close.
async fn read_one(socket: &mut TcpStream) -> Answer {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        raw.push(socket.read_u8().await.unwrap());
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
    let _read = socket.read_exact(&mut body).await.unwrap();
    raw.extend(body);
    parse(&raw)
}
async fn get_models(socket: &mut TcpStream, rig: &Rig, authorized: bool) -> Answer {
    let auth = if authorized {
        format!("Authorization: Bearer {}\r\n", rig.token)
    } else {
        String::new()
    };
    socket
        .write_all(
            format!(
                "GET /v1/models HTTP/1.1\r\nHost: {}\r\n{auth}\r\n",
                rig.addr
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    read_one(socket).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_unauthenticated_keepalives_cannot_starve_a_real_harness() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let mut held = Vec::new();
        for _ in 0..256 {
            let mut socket = TcpStream::connect(rig.addr).await.unwrap();
            assert_eq!(get_models(&mut socket, &rig, false).await.status, 401);
            held.push(socket);
        }
        // Adjusted when ported (the verification round, item 4): a refusal
        // now closes its connection (`Connection: close`), so the renewal
        // this loop made, a second 401 on each socket, cannot happen; each
        // held socket reads its end instead. The sockets stay open, so the
        // proxy may still hold their places while it lingers on them: the
        // extra connection below must get past them all the same.
        for socket in &mut held {
            let mut rest = Vec::new();
            assert_eq!(socket.read_to_end(&mut rest).await.unwrap(), 0);
        }
        let mut byte = [0_u8; 1];
        let mut extra = TcpStream::connect(rig.addr).await.unwrap();
        let early = tokio::time::timeout(Duration::from_millis(100), extra.peek(&mut byte)).await;
        let status = if matches!(early, Ok(Ok(n)) if n > 0) {
            read_one(&mut extra).await.status
        } else { get_models(&mut extra, &rig, true).await.status };
        drop(extra);
        drop(held);
        rig.stop().await;
        assert_eq!(status, 200, "256 unauthorized keepalives (each renewed with another 401) block real harness; status={status}");
    }).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_same_api_eof_without_terminal_event_is_reported_to_harness() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let prefix = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"allowed\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n"
        );
        rig.upstream.reply(Reply::Stream { body: Bytes::from_static(prefix.as_bytes()), piece: 17 });
        let got = rig.call("/v1/messages", &input("/v1/messages", true)).await;
        rig.stop().await;
        assert!(got.body.contains("event: error"), "HTTP {} closes cleanly after partial completion with no error event: {}", got.status, got.body);
    }).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_bom_split_does_not_remove_same_api_event_name() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let stream = "\u{feff}event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut broken = Vec::new();
        for piece in [1, 2, 3, 4, 64] {
            rig.upstream.reply(Reply::Stream {
                body: Bytes::from(stream),
                piece,
            });
            let got = rig.call("/v1/messages", &input("/v1/messages", true)).await;
            if !got.body.contains("event: message_stop") {
                broken.push(piece);
            }
        }
        rig.stop().await;
        assert!(
            broken.is_empty(),
            "valid BOM-prefixed SSE loses event name at piece sizes {broken:?}"
        );
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_host_and_admission_corner_cases() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        for (host, expected) in [
            (Some(format!("LOCALHOST:{}", rig.addr.port())), 200),
            (Some(format!("localhost.:{}", rig.addr.port())), 403),
            (Some(format!("[::1]:{}", rig.addr.port())), 403),
            (None, 403),
            (Some(format!("127.0.0.1:{}", rig.addr.port())), 200),
        ] {
            for authorized in [false, true] {
                let mut socket = TcpStream::connect(rig.addr).await.unwrap();
                let host = host
                    .as_ref()
                    .map_or_else(String::new, |host| format!("Host: {host}\r\n"));
                let auth = if authorized {
                    format!("Authorization: Bearer {}\r\n", rig.token)
                } else {
                    String::new()
                };
                socket
                    .write_all(
                        format!("GET /v1/models HTTP/1.1\r\n{host}{auth}Connection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                let got = read_one(&mut socket).await;
                assert_eq!(
                    got.status,
                    if authorized { expected } else { 401 },
                    "host={host:?}, authorized={authorized}"
                );
            }
        }
        rig.stop().await;
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_parse_failures_and_cancellation_release_exactly_one_permit() {
    support::within(async {
        let rig=Rig::start("anthropic-messages").await;
        for _ in 0..12 {assert_eq!(rig.call("/v1/messages","{").await.status,400);}
        let mut bodies=Vec::new();
        for _ in 0..4 {
            let mut socket=TcpStream::connect(rig.addr).await.unwrap();
            socket.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nExpect: 100-continue\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",rig.addr,rig.token).as_bytes()).await.unwrap();
            assert_eq!(read_one(&mut socket).await.status,100);
            bodies.push(socket);
        }
        assert_eq!(rig.call("/v1/messages","{}").await.status,429);
        let mut cancelled=bodies.pop().unwrap();
        cancelled.shutdown().await.unwrap();
        assert_eq!(read_one(&mut cancelled).await.status,400);
        for _ in 0..8 {assert_eq!(rig.call("/v1/messages","{").await.status,400);}
        let mut replacement=TcpStream::connect(rig.addr).await.unwrap();
        replacement.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nExpect: 100-continue\r\nContent-Length: 100\r\n\r\n",rig.addr,rig.token).as_bytes()).await.unwrap();
        assert_eq!(read_one(&mut replacement).await.status,100);
        assert_eq!(rig.call("/v1/messages","{}").await.status,429);
        drop(replacement);drop(bodies);drop(cancelled);
        rig.stop().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tool_arguments_and_signatures_share_the_total_answer_cap() {
    support::within(async {
        let rig=Rig::start("anthropic-messages").await;
        for signature in [false,true] {
            let block=if signature {r#"{"type":"thinking","thinking":"","signature":""}"#}else{r#"{"type":"tool_use","id":"t1","name":"f","input":{}}"#};
            let delta=if signature {format!(r#"{{"type":"signature_delta","signature":"{}"}}"#,"A".repeat(1024*1024))}
                else {format!(r#"{{"type":"input_json_delta","partial_json":"{}"}}"#,"a".repeat(1024*1024))};
            let mut stream=format!("event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{block}}}\n\n");
            let delta=format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{delta}}}\n\n");
            for _ in 0..33 {stream.push_str(&delta);}
            rig.upstream.reply(Reply::Stream{body:Bytes::from(stream),piece:65536});
            let got=rig.call("/v1/responses",&input("/v1/responses",false)).await;
            assert!(got.body.contains(&format!("grew past {CAP} bytes")),"no cap failure, signature={signature}, bytes={}",got.body.len());
            assert!(got.body.len()<8192);
        }
        rig.stop().await;
    }).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_canonical_errors_escaped_keys_id_comments_and_multiline_data() {
    support::within(async {
        for api in ["anthropic-messages","openai-responses","openai-chat-completions"] {
            let rig=Rig::start(api).await;
            for front in ["/v1/messages","/v1/responses"] {
                for piece in [1,2,7,257] {
                    let escaped=escaped_key();
                    let data=match api {
                        "anthropic-messages"=>format!(r#"{{"type":"error","error":{{"type":"api_error","message":"{escaped}"}}}}"#),
                        "openai-responses"=>format!(r#"{{"type":"error","code":"server_error","message":"{escaped}","sequence_number":1}}"#),
                        _=>format!(r#"{{"error":{{"message":"{escaped}","code":500}}}}"#),
                    };
                    let multiline=data.replacen(',',",\ndata: ",1);
                    let stream=format!("id: {KEY}\n: {KEY}\nretry: 100\nevent: error\ndata: {multiline}\n\n");
                    rig.upstream.reply(Reply::Stream{body:Bytes::from(stream),piece});
                    let got=rig.call(front,&input(front,true)).await;
                    assert!(got.body.contains("[redacted]"),"api={api}, front={front}, piece={piece}");
                    assert!(!decoded_data_has_key(&got.body));
                    assert!(!got.body.contains(KEY));
                }
            }
            rig.stop().await;
        }
    }).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_header_and_body_deadlines_recover_capacity() {
    support::within(async {
        let rig=Rig::start("anthropic-messages").await;
        let mut headers=TcpStream::connect(rig.addr).await.unwrap();
        headers.write_all(b"GET /v1/models HTTP/1.1\r\nHost:").await.unwrap();
        let ended=tokio::time::timeout(Duration::from_secs(12),read_one(&mut headers)).await.unwrap();
        assert_eq!(ended.status,408);
        drop(headers);
        let mut body=TcpStream::connect(rig.addr).await.unwrap();
        body.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nExpect: 100-continue\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",rig.addr,rig.token).as_bytes()).await.unwrap();
        assert_eq!(read_one(&mut body).await.status,100);
        let ended=tokio::time::timeout(Duration::from_secs(32),read_one(&mut body)).await.unwrap();
        assert_eq!(ended.status,408);
        drop(body);
        assert_eq!(rig.call("/v1/messages","{").await.status,400);
        rig.stop().await;
    }).await;
}
