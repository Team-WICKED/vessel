//! Run: copy to crates/vessel-node/tests/m5_trust_boundaries.rs, then
//! `cargo test -p vessel-node --test m5_trust_boundaries -- --nocapture`
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_model_fields_must_not_bypass_route_pinning() {
    support::within(async {
        let mut forwarded = Vec::new();
        for (api, front) in [
            ("anthropic-messages", "/v1/messages"),
            ("openai-responses", "/v1/responses"),
        ] {
            let rig = Rig::start(api).await;
            rig.upstream.reply(Reply::Status {
                status: 200,
                body: Bytes::from_static(b"{}"),
                retry_after: None,
            });
            let body = input(front, false).replacen(
                "\"model\":\"allowed\"",
                "\"model\":\"allowed\",\"model\":\"forbidden-expensive\"",
                1,
            );
            let got = rig.call(front, &body).await;
            for seen in rig.upstream.seen() {
                // A standard last-value-wins JSON object, as used by many upstreams.
                let members: std::collections::BTreeMap<String, vessel_proto::RawJson> =
                    serde_json::from_slice(&seen.body).unwrap();
                let actual: String = serde_json::from_str(members["model"].as_text()).unwrap();
                if actual != "allowed" {
                    forwarded.push(format!(
                        "{api}: HTTP {}, upstream model={actual}",
                        got.status
                    ));
                }
            }
            rig.stop().await;
        }
        assert!(
            forwarded.is_empty(),
            "forbidden models reached FakeUpstream: {forwarded:?}"
        );
    })
    .await;
}

fn error_event(api: &str) -> String {
    match api {
        "anthropic-messages" => format!(
            "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"echo {KEY}\"}}}}\n\n"
        ),
        "openai-responses" => format!(
            "event: error\ndata: {{\"type\":\"error\",\"sequence_number\":1,\"code\":\"server_error\",\"message\":\"echo {KEY}\",\"param\":null}}\n\n"
        ),
        "openai-chat-completions" => {
            format!("data: {{\"error\":{{\"message\":\"echo {KEY}\",\"code\":500}}}}\n\n")
        }
        _ => panic!("unsupported fake API"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_errors_must_redact_provider_key_on_every_api_pair() {
    support::within(async {
        let mut leaks = Vec::new();
        for api in [
            "anthropic-messages",
            "openai-responses",
            "openai-chat-completions",
        ] {
            let rig = Rig::start(api).await;
            for front in ["/v1/messages", "/v1/responses"] {
                rig.upstream.reply(Reply::Stream {
                    body: Bytes::from(format!(": first byte\n\n{}", error_event(api))),
                    piece: 7,
                });
                let got = rig.call(front, &input(front, true)).await;
                if got.body.contains(KEY) {
                    leaks.push(format!(
                        "{api} -> {front}: HTTP {}, provider key in harness response",
                        got.status
                    ));
                }
            }
            rig.stop().await;
        }
        assert!(leaks.is_empty(), "key leaks: {leaks:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_must_run_before_body_size_rejection() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let mut wrong = Vec::new();
        let oversized = "x".repeat(CAP + 1);
        for (headers, expected) in [
            (
                vec![
                    ("origin", "https://example.invalid"),
                    ("content-encoding", "gzip"),
                ],
                403,
            ),
            (vec![("content-encoding", "gzip")], 415),
            (vec![], 401),
        ] {
            let got = request(rig.addr, "/v1/messages", &headers, &oversized).await;
            if got.status != expected {
                wrong.push((expected, got.status));
            }
        }
        rig.stop().await;
        assert!(
            wrong.is_empty(),
            "expected admission status / actual status: {wrong:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_partial_body_must_be_refused_without_waiting() {
    support::within(async {
        let rig = Rig::start("anthropic-messages").await;
        let mut socket = TcpStream::connect(rig.addr).await.unwrap();
        let wire = format!("POST /v1/messages HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {CAP}\r\n\r\nx", rig.addr);
        socket.write_all(wire.as_bytes()).await.unwrap();
        let mut first = [0_u8; 128];
        // A response deadline is the behavior under test, not a coordination sleep.
        let response = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut first)).await;
        drop(socket);
        rig.stop().await;
        assert!(response.is_ok(), "no 401 within 1 s: an unauthenticated incomplete body holds a connection before admission, outside both call limits");
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translated_nonstream_response_must_obey_body_cap() {
    support::within(async {
        let rig = Rig::start("openai-chat-completions").await;
        let delta = format!("data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}]}}\n\n", "a".repeat(1024 * 1024));
        let mut stream = delta.repeat(33);
        stream.push_str("data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
        rig.upstream.reply(Reply::Stream { body: Bytes::from(stream), piece: 65536 });
        let got = rig.call("/v1/messages", &input("/v1/messages", false)).await;
        let length = got.body.len();
        rig.stop().await;
        assert!(length <= CAP, "translated nonstream response retained and returned {length} bytes (cap {CAP}), HTTP {}", got.status);
    }).await;
}

/// Positive controls distinguish the proven bypasses from ordinary auth
/// and HTTP-status redaction, which must continue to work after fixes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_admission_and_typed_http_error_redaction_controls() {
    support::within(async {
        for api in ["anthropic-messages", "openai-responses", "openai-chat-completions"] {
            let rig = Rig::start(api).await;
            let small = input("/v1/messages", false);
            assert_eq!(request(rig.addr, "/v1/messages", &[], &small).await.status, 401);
            assert_eq!(request(rig.addr, "/v1/messages", &[("origin", "null"), ("content-encoding", "gzip")], &small).await.status, 403);
            assert_eq!(request(rig.addr, "/v1/messages", &[("content-encoding", "gzip")], &small).await.status, 415);
            assert_eq!(rig.call("/v1/messages", &small.replace("allowed", "forbidden")).await.status, 400);
            assert!(rig.upstream.seen().is_empty());

            // An arbitrary Host without an Origin does not obtain model access.
            let mut socket = TcpStream::connect(rig.addr).await.unwrap();
            socket.write_all(b"GET /v1/models HTTP/1.1\r\nHost: rebind.example.invalid\r\nConnection: close\r\n\r\n").await.unwrap();
            let mut raw = Vec::new();
            let _read = support::step("Host-only request", socket.read_to_end(&mut raw)).await.unwrap();
            assert_eq!(parse(&raw).status, 401);

            for front in ["/v1/messages", "/v1/responses"] {
                let error = if api == "anthropic-messages" {
                    format!(r#"{{"type":"error","error":{{"type":"authentication_error","message":"echo {KEY}"}}}}"#)
                } else {
                    format!(r#"{{"error":{{"type":"authentication_error","message":"echo {KEY}"}}}}"#)
                };
                rig.upstream.reply(Reply::Status { status: 401, body: Bytes::from(error), retry_after: None });
                let got = rig.call(front, &input(front, true)).await;
                assert!(!got.body.contains(KEY));
                assert!(got.body.contains("[redacted]"));
            }
            let listed = serde_json::to_string(&rig.client.models().await.unwrap()).unwrap();
            for forbidden in [KEY, "REVIEW_KEY", "base_url", "http://", "has_key"] {
                assert!(!listed.contains(forbidden), "model.list contains {forbidden}");
            }
            let mut stream = rig.client.follow(rig.session, EventSeq::ZERO).await.unwrap();
            rig.client.kill(rig.session).await.unwrap();
            let mut seen = Vec::new();
            support::read_until(&mut stream, &mut seen, |record| {
                support::status_of(record).is_some_and(|status| !status.is_live())
            }).await;
            assert_eq!(rig.call("/v1/messages", &small).await.status, 401);
            rig.node.stop().await.unwrap();
        }
    }).await;
}
