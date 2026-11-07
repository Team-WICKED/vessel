//! The model proxy between a live (fake) session and a fake provider: who
//! may call, what goes up, what comes back, and what the session's log
//! records. Requests are written by hand, so each test controls exactly
//! which headers a "harness" sends.

use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vessel_harness::Lookup;
use vessel_harness::fakes::{Role, install};
use vessel_llm::client::Limits;
use vessel_proto::{
    EventRecord, HarnessKind, ModelApi, ModelCall, ModelFailure, ModelStop, SessionEvent,
    SessionMode, SessionSpec, TerminalSize,
};
use vessel_session::{
    DEFAULT_DURABLE_CAPACITY, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RING_CAPACITY, RuntimeConfig,
    SessionHandle, SessionRuntime,
};
use vessel_store::DataDir;
use vessel_testkit::Fake;
use vessel_testkit::upstream::{FakeUpstream, Reply};

use super::{Models, PER_SESSION};
use crate::fleet::{FleetFiles, set_secret};

/// The provider key every route's provider uses here.
const KEY: &str = "sk-upstream-test-key";

/// What a session's records come to once it has ended.
type Records = JoinHandle<Vec<Arc<EventRecord>>>;

struct Fixture {
    data: TempDir,
    _bin: TempDir,
    runtime: Arc<SessionRuntime>,
    upstream: FakeUpstream,
    models: Arc<Models>,
    proxy: SocketAddr,
    stop: CancellationToken,
}

impl Fixture {
    async fn new() -> Self {
        Self::with(Limits::default()).await
    }

    /// A fixture whose upstream client has `limits` (the proxy's answer
    /// bound and write deadline come from them).
    async fn with(limits: Limits) -> Self {
        let data = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        let runtime = Arc::new(SessionRuntime::new(
            RuntimeConfig {
                harness_lookup: Lookup::new([bin.path().to_path_buf()]),
                harness_env: vessel_harness::HarnessEnv::new(data.path().to_owned()),
                ring_capacity: DEFAULT_RING_CAPACITY,
                durable_capacity: DEFAULT_DURABLE_CAPACITY,
                agent_handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            },
            CancellationToken::new(),
        ));
        let upstream = FakeUpstream::start().await.unwrap();
        let dir = DataDir::new(data.path());
        write_models(&dir, &upstream.url());
        set_secret(&FleetFiles::new(&dir), &"TEST_KEY".parse().unwrap(), KEY).unwrap();
        let models = Arc::new(Models::for_tests_with(&dir, limits));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let proxy = listener.local_addr().unwrap();
        models.listening(proxy);
        let stop = CancellationToken::new();
        drop(tokio::spawn(super::proxy::serve(
            Arc::clone(&models),
            listener,
            stop.clone(),
        )));
        Self {
            data,
            _bin: bin,
            runtime,
            upstream,
            models,
            proxy,
            stop,
        }
    }

    /// A running session on route `route`, its token, and its records.
    async fn session(&self, route: &str) -> (SessionHandle, String, Records) {
        let spec = SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        )
        .with_cwd(self.data.path().to_path_buf());
        let created = self.runtime.create(spec).await.unwrap();
        let id = created.handle.id();
        let profile = routed_profile(route);
        let (model, proxy) = self.models.launch(Some(&profile), id).unwrap().unwrap();
        assert_eq!(model.route.as_str(), route);
        assert_eq!(proxy.base_url, format!("http://{}", self.proxy));
        let token = proxy.token;
        // Minted for a session not started yet, it opens nothing.
        let early = self
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                MESSAGE,
            )
            .await;
        assert_eq!(early.status, 401, "{early:?}");
        self.models.started(id, created.handle.clone());
        let mut durable = created.durable;
        let records = tokio::spawn(async move {
            let mut records = Vec::new();
            while let Some(record) = durable.recv().await {
                records.push(record);
            }
            records
        });
        (created.handle, token, records)
    }

    async fn call(&self, request_line: &str, headers: &[(&str, &str)], body: &str) -> Answer {
        call(self.proxy, request_line, headers, body).await
    }

    async fn stop(self) {
        self.stop.cancel();
        self.runtime.shutdown().await;
    }
}

/// A Claude Code profile on `route`.
fn routed_profile(route: &str) -> vessel_harness::Profile {
    let text = format!("[profiles.routed]\nharness = \"claude\"\nmodel = \"{route}\"\n");
    vessel_harness::parse_profiles(&text, None)
        .unwrap()
        .get(&"routed".parse().unwrap())
        .cloned()
        .unwrap()
}

/// `models.toml`, owner-only, with one provider of each API at the fake.
fn write_models(dir: &DataDir, url: &str) {
    let text = format!(
        r#"
[providers.anthropic]
api = "anthropic-messages"
base_url = "{url}"
key = {{ secret = "TEST_KEY" }}

[providers.chat]
api = "openai-chat-completions"
base_url = "{url}/v1"
key = {{ secret = "TEST_KEY" }}

[models.sonnet]
provider = "anthropic"
model = "claude-sonnet-4-5"
small_model = "claude-haiku-4-5"

[models.qwen]
provider = "chat"
model = "qwen/qwen3-coder"
"#
    );
    let path = dir.models_path();
    std::fs::write(&path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// An HTTP answer, read to its end.
#[derive(Debug)]
struct Answer {
    status: u16,
    /// Names lowercase.
    headers: Vec<(String, String)>,
    /// De-chunked.
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(have, _)| have == name)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// One HTTP/1.1 request on its own connection, the answer read until the
/// proxy closes it.
async fn call(
    proxy: SocketAddr,
    request_line: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Answer {
    call_as(proxy, &proxy.to_string(), request_line, headers, body).await
}

/// [`call`] naming `host` as its `Host`.
async fn call_as(
    proxy: SocketAddr,
    host: &str,
    request_line: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Answer {
    let request = request_text(host, request_line, headers, body);
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _read = stream.read_to_end(&mut response).await.unwrap();
    parse(&response)
}

/// A request as the bytes a harness sends.
fn request_text(host: &str, request_line: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut request = format!(
        "{request_line} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        write!(request, "{name}: {value}\r\n").unwrap();
    }
    request.push_str("\r\n");
    request.push_str(body);
    request
}

fn parse(response: &[u8]) -> Answer {
    let text = String::from_utf8(response.to_vec()).unwrap();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value == "chunked");
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_owned()
    };
    Answer {
        status,
        headers,
        body,
    }
}

/// A chunked body's content (the proxy's answers are ASCII-sized chunks of
/// UTF-8 text, so splitting by bytes stays on character boundaries).
fn dechunk(mut rest: &str) -> String {
    let mut body = String::new();
    loop {
        let (size, after) = rest.split_once("\r\n").unwrap();
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        if size == 0 {
            return body;
        }
        let (chunk, after) = after.split_at(size);
        body.push_str(chunk);
        rest = after.strip_prefix("\r\n").unwrap();
    }
}

/// The model calls among a session's records.
async fn model_calls(handle: &SessionHandle, records: Records) -> Vec<ModelCall> {
    handle.kill();
    records
        .await
        .unwrap()
        .iter()
        .filter_map(|record| {
            if let SessionEvent::ModelCall(call) = &record.event {
                Some(call.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Fail a test that hangs rather than hang the suite.
async fn within(test: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(60), Box::pin(test))
        .await
        .unwrap();
}

const MESSAGE: &str =
    r#"{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;

/// Everything refused is refused before the provider is called, in the
/// harness's error shape, without echoing what was presented.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn who_may_call() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("sonnet").await;
        let bearer = format!("Bearer {token}");

        let none = fixture.call("POST /v1/messages", &[], MESSAGE).await;
        assert_eq!(none.status, 401, "{none:?}");
        assert_eq!(none.json()["error"]["type"], "authentication_error");

        let forged = format!("vslm-{}", "ab".repeat(32));
        for presented in [
            vec![("x-api-key", forged.as_str())],
            vec![("authorization", "Bearer not-a-token")],
        ] {
            let wrong = fixture.call("POST /v1/messages", &presented, MESSAGE).await;
            assert_eq!(wrong.status, 401, "{wrong:?}");
            assert!(!wrong.body.contains(presented[0].1), "{wrong:?}");
        }
        let codex = fixture
            .call(
                "POST /v1/responses",
                &[("authorization", "Bearer nope")],
                "{}",
            )
            .await;
        assert_eq!(codex.status, 401, "{codex:?}");
        assert_eq!(codex.json()["error"]["type"], "authentication_error");

        // A browser page aimed at the port, even with a good token.
        let page = fixture
            .call(
                "POST /v1/messages",
                &[
                    ("authorization", &bearer),
                    ("origin", "https://example.com"),
                ],
                MESSAGE,
            )
            .await;
        assert_eq!(page.status, 403, "{page:?}");

        let compressed = fixture
            .call(
                "POST /v1/responses",
                &[("authorization", &bearer), ("content-encoding", "zstd")],
                "{}",
            )
            .await;
        assert_eq!(compressed.status, 415, "{compressed:?}");

        // Codex's WebSocket transport, on the path it posts to as well.
        let socket = fixture
            .call("GET /v1/responses", &[("upgrade", "websocket")], "")
            .await;
        assert_eq!(socket.status, 426, "{socket:?}");
        let elsewhere = fixture.call("GET /v2/anything", &[], "").await;
        assert_eq!(elsewhere.status, 404, "{elsewhere:?}");
        assert!(elsewhere.json()["error"].is_object(), "{elsewhere:?}");

        assert!(fixture.upstream.seen().is_empty());
        assert!(model_calls(&handle, records).await.is_empty());

        // Once the session has ended its token opens nothing.
        let _ended = handle.ended().await;
        let late = fixture
            .call("POST /v1/messages", &[("authorization", &bearer)], MESSAGE)
            .await;
        assert_eq!(late.status, 401, "{late:?}");
        fixture.stop().await;
    })
    .await;
}

/// Same API on both sides: the harness's bytes go up with the provider's
/// key in place of the session's token, the provider's come back, and the
/// call is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_api_call_goes_up_with_the_provider_key_and_is_recorded() {
    within(async {
        let fixture = Fixture::new().await;
        assert_eq!(fixture.models.base_url(), Some(format!("http://{}", fixture.proxy)));
        let (handle, token, records) = fixture.session("sonnet").await;
        let bearer = format!("Bearer {token}");
        let answer = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":3}}"#;
        fixture.upstream.reply(Reply::Status {
            status: 200,
            body: Bytes::from_static(answer.as_bytes()),
            retry_after: None,
        });

        let harness = [
            ("authorization", bearer.as_str()),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "a-beta"),
            ("x-stainless-os", "Linux"),
        ];
        let got = fixture.call("POST /v1/messages", &harness, MESSAGE).await;
        assert_eq!(got.status, 200, "{got:?}");
        assert_eq!(got.body, answer);

        let seen = fixture.upstream.seen();
        assert_eq!(seen.len(), 1);
        let up = &seen[0];
        assert_eq!(up.path, "/v1/messages");
        assert_eq!(up.body, MESSAGE.as_bytes());
        assert_eq!(up.header("x-api-key"), Some(KEY));
        assert_eq!(up.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(up.header("anthropic-beta"), Some("a-beta"));
        assert_eq!(up.header("authorization"), None);
        assert_eq!(up.header("x-stainless-os"), None);
        assert!(
            up.headers.iter().all(|(_, value)| !value.contains("vslm-")),
            "{:?}",
            up.headers
        );

        // Another model than the route's is a mistake, named, not sent.
        let other = MESSAGE.replace("claude-sonnet-4-5", "claude-opus-4-1");
        let refused = fixture.call("POST /v1/messages", &harness, &other).await;
        assert_eq!(refused.status, 400, "{refused:?}");
        let says = refused.json()["error"]["message"].as_str().unwrap().to_owned();
        for name in ["claude-opus-4-1", "claude-sonnet-4-5", "claude-haiku-4-5", "sonnet"] {
            assert!(says.contains(name), "{says}");
        }
        assert_eq!(fixture.upstream.seen().len(), 1);

        // What Claude Code asks besides calls, answered here.
        let counted = fixture
            .call("POST /v1/messages/count_tokens", &[("x-api-key", &token)], &"x".repeat(40))
            .await;
        assert_eq!(counted.json()["input_tokens"], 10);
        let listed = fixture
            .call("GET /v1/models", &[("x-api-key", &token), ("anthropic-version", "2023-06-01")], "")
            .await;
        let ids: Vec<String> = listed.json()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, ["claude-sonnet-4-5", "claude-haiku-4-5"]);
        let openai = fixture
            .call("GET /v1/models", &[("authorization", &bearer)], "")
            .await;
        assert_eq!(openai.json()["object"], "list");
        assert_eq!(fixture.upstream.seen().len(), 1);

        let calls = model_calls(&handle, records).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        let call = &calls[0];
        assert_eq!(call.route.as_str(), "sonnet");
        assert_eq!(call.model.as_str(), "claude-sonnet-4-5");
        assert_eq!(call.api, ModelApi::AnthropicMessages);
        assert_eq!(call.usage.input.get(), 12);
        assert_eq!(call.usage.output.get(), 3);
        assert_eq!(call.stop, ModelStop::EndTurn);
        assert_eq!(call.failure, None);
        fixture.stop().await;
    })
    .await;
}

const CHAT_STREAM: &str = concat!(
    ": PROCESSING\n\n",
    r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen/qwen3-coder","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen/qwen3-coder","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
    "\n\n",
    r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen/qwen3-coder","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":2,"total_tokens":22}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// Different APIs: Claude Code's request goes up as Chat Completions, and
/// the provider's stream comes back as Anthropic events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_translated_call_streams_back_in_the_harness_api_and_is_recorded() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("qwen").await;
        fixture.upstream.reply(Reply::Stream {
            body: Bytes::from_static(CHAT_STREAM.as_bytes()),
            piece: 37,
        });
        let request = r#"{"model":"qwen/qwen3-coder","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                request,
            )
            .await;
        assert_eq!(got.status, 200, "{got:?}");
        assert_eq!(got.header("content-type"), Some("text/event-stream"));
        let starts = got.body.find("event: message_start").unwrap();
        let hel = got.body.find(r#""text":"Hel""#).unwrap();
        let lo = got.body.find(r#""text":"lo""#).unwrap();
        let stops = got.body.find("event: message_stop").unwrap();
        assert!(starts < hel && hel < lo && lo < stops, "{}", got.body);

        let seen = fixture.upstream.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "/v1/chat/completions");
        assert_eq!(seen[0].header("authorization"), Some(&*format!("Bearer {KEY}")));
        let up: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(up["model"], "qwen/qwen3-coder");
        assert_eq!(up["stream"], true);

        let calls = model_calls(&handle, records).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        let call = &calls[0];
        assert_eq!(call.route.as_str(), "qwen");
        assert_eq!(call.api, ModelApi::OpenAiChatCompletions);
        assert_eq!(call.usage.input.get(), 20);
        assert_eq!(call.usage.output.get(), 2);
        assert_eq!(call.stop, ModelStop::EndTurn);
        assert_eq!(call.failure, None);
        fixture.stop().await;
    })
    .await;
}

/// A session's calls past its limit are refused as a rate limit, with a
/// wait, while its first calls are still open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_past_the_session_limit_are_refused_as_rate_limited() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("sonnet").await;
        let bearer = format!("Bearer {token}");
        let mut open = Vec::new();
        for _ in 0..PER_SESSION {
            fixture.upstream.reply(Reply::Hang {
                wait: Duration::from_secs(120),
            });
            let proxy = fixture.proxy;
            let bearer = bearer.clone();
            open.push(tokio::spawn(async move {
                call(
                    proxy,
                    "POST /v1/messages",
                    &[("authorization", &bearer)],
                    MESSAGE,
                )
                .await
            }));
        }
        while fixture.upstream.seen().len() < PER_SESSION {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let busy = fixture
            .call("POST /v1/messages", &[("authorization", &bearer)], MESSAGE)
            .await;
        assert_eq!(busy.status, 429, "{busy:?}");
        assert_eq!(busy.header("retry-after"), Some("1"));
        assert_eq!(busy.json()["error"]["type"], "rate_limit_error");
        assert_eq!(fixture.upstream.seen().len(), PER_SESSION);

        for task in open {
            task.abort();
        }
        drop(model_calls(&handle, records).await);
        fixture.stop().await;
    })
    .await;
}

/// `models.toml` names the provider keys' secrets and the providers' URLs:
/// a file other users can read is refused, at start and on reload, and a
/// reload that fails keeps the routes in effect.
#[cfg(unix)]
#[test]
fn a_models_file_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let data = tempfile::tempdir().unwrap();
    let dir = DataDir::new(data.path());
    let client = || {
        vessel_llm::client::Client::with_roots(rustls::RootCertStore::empty(), Limits::default())
            .unwrap()
    };
    write_models(&dir, "http://127.0.0.1:9");
    let open = std::fs::Permissions::from_mode(0o644);
    std::fs::set_permissions(dir.models_path(), open.clone()).unwrap();
    assert!(matches!(
        Models::load(dir.clone(), FleetFiles::new(&dir), client()),
        Err(super::ModelsError::NotOwnerOnly)
    ));

    write_models(&dir, "http://127.0.0.1:9");
    let models = Models::load(dir.clone(), FleetFiles::new(&dir), client()).unwrap();
    assert_eq!(models.list().len(), 2);
    std::fs::set_permissions(dir.models_path(), open).unwrap();
    assert!(matches!(
        models.reload(),
        Err(super::ModelsError::NotOwnerOnly)
    ));
    assert_eq!(models.list().len(), 2);
}

/// The next model call recorded in `handle`'s log, as it happens.
async fn next_call(handle: &SessionHandle) -> ModelCall {
    let follow = handle.follow(vessel_proto::EventSeq::ZERO).unwrap();
    for record in &follow.cached {
        if let SessionEvent::ModelCall(call) = &record.event {
            return call.clone();
        }
    }
    let mut live = follow.live;
    loop {
        let record = live.recv().await.unwrap();
        if let SessionEvent::ModelCall(call) = &record.event {
            return call.clone();
        }
    }
}

/// Anthropic stream events: `count` text deltas of `size` bytes each.
fn deltas(count: usize, size: usize) -> Bytes {
    let delta = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\
         \"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
        "a".repeat(size)
    );
    Bytes::from(delta.repeat(count))
}

/// [`MESSAGE`], streamed.
fn streamed() -> String {
    MESSAGE.replacen(
        "\"max_tokens\":64,",
        "\"max_tokens\":64,\"stream\":true,",
        1,
    )
}

/// `Host` must name the proxy itself (defence in depth against DNS
/// rebinding), checked after the token: without one, 401 whatever the
/// host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_proxy_s_own_host_is_answered() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("sonnet").await;
        let bearer = format!("Bearer {token}");
        let port = fixture.proxy.port();
        let ask = |host: String, headers: Vec<(&'static str, String)>| {
            let proxy = fixture.proxy;
            async move {
                let headers: Vec<(&str, &str)> = headers
                    .iter()
                    .map(|(name, value)| (*name, value.as_str()))
                    .collect();
                call_as(proxy, &host, "GET /v1/models", &headers, "")
                    .await
                    .status
            }
        };
        let with = || vec![("authorization", bearer.clone())];
        for host in [
            format!("rebind.example:{port}"),
            format!("localhost.rebind.example:{port}"),
            "127.0.0.1:1".to_owned(),
            "127.0.0.1".to_owned(),
        ] {
            assert_eq!(ask(host.clone(), with()).await, 403, "{host}");
            assert_eq!(ask(host.clone(), Vec::new()).await, 401, "{host}");
        }
        for host in [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("LocalHost:{port}"),
        ] {
            assert_eq!(ask(host.clone(), with()).await, 200, "{host}");
        }
        assert!(fixture.upstream.seen().is_empty());
        drop(model_calls(&handle, records).await);
        fixture.stop().await;
    })
    .await;
}

/// A harness that stops reading its streamed answer: the write deadline
/// (the client's idle timeout, a second here) ends the call, recorded as
/// cut off, instead of holding the session's place for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_that_stops_reading_ends_its_call() {
    within(async {
        let limits = Limits {
            idle: Duration::from_secs(1),
            max_body: 64 * 1024 * 1024,
            ..Limits::default()
        };
        let fixture = Fixture::with(limits).await;
        let (handle, token, records) = fixture.session("sonnet").await;
        // About 48 MiB, under the answer's bound: more than the sockets
        // between can hold on any system, with the harness's own receive
        // buffer kept small.
        fixture.upstream.reply(Reply::Stream {
            body: deltas(12 * 1024, 4096),
            piece: 64 * 1024,
        });
        let request = request_text(
            &fixture.proxy.to_string(),
            "POST /v1/messages",
            &[("authorization", &format!("Bearer {token}"))],
            &streamed(),
        );
        let harness = tokio::net::TcpSocket::new_v4().unwrap();
        harness.set_recv_buffer_size(8 * 1024).unwrap();
        let mut socket = harness.connect(fixture.proxy).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        // Never read.
        let call = next_call(&handle).await;
        assert_eq!(call.failure, Some(ModelFailure::Truncated), "{call:?}");
        drop(socket);
        drop(model_calls(&handle, records).await);
        fixture.stop().await;
    })
    .await;
}

/// A same-API stream Vessel cannot decode is not passed on unread: the
/// harness gets an error in its own form, and the call is a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_vessel_cannot_read_is_a_failure() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("sonnet").await;
        fixture.upstream.reply(Reply::Stream {
            body: Bytes::from_static(b"event: message_start\ndata: \xff\xfe\n\n"),
            piece: 64,
        });
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                &streamed(),
            )
            .await;
        assert_eq!(got.status, 200, "{got:?}");
        assert!(got.body.contains("event: error"), "{got:?}");
        assert!(got.body.contains("not Server-Sent Events"), "{got:?}");
        let calls = model_calls(&handle, records).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].failure, Some(ModelFailure::UpstreamError));
        fixture.stop().await;
    })
    .await;
}

/// An answer that grows past the bound is cut off, same API or
/// translated, streamed or collected (finding 5, with a 64 KiB bound).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_past_the_bound_is_cut_off() {
    within(async {
        let limits = Limits {
            max_body: 64 * 1024,
            ..Limits::default()
        };
        let fixture = Fixture::with(limits).await;
        let (sonnet, token, sonnet_records) = fixture.session("sonnet").await;
        fixture.upstream.reply(Reply::Stream {
            body: deltas(200, 1024),
            piece: 4096,
        });
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                &streamed(),
            )
            .await;
        assert!(got.body.len() < 80 * 1024, "{}", got.body.len());
        assert!(got.body.contains("event: error"), "{}", got.body.len());
        assert!(got.body.contains("grew past 65536 bytes"));

        let (qwen, token, qwen_records) = fixture.session("qwen").await;
        let delta = format!(
            "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{}\"}},\
             \"finish_reason\":null}}]}}\n\n",
            "a".repeat(1024)
        );
        fixture.upstream.reply(Reply::Stream {
            body: Bytes::from(delta.repeat(200)),
            piece: 4096,
        });
        let request = r#"{"model":"qwen/qwen3-coder","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                request,
            )
            .await;
        assert!(got.status >= 500, "{got:?}");
        assert!(got.body.contains("grew past 65536 bytes"), "{got:?}");

        for (handle, records) in [(sonnet, sonnet_records), (qwen, qwen_records)] {
            let calls = model_calls(&handle, records).await;
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(calls[0].failure, Some(ModelFailure::UpstreamError));
        }
        fixture.stop().await;
    })
    .await;
}

/// A successful status does not mean the answer holds no key: a whole
/// same-API answer quoting it reaches the harness redacted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whole_answer_quoting_the_key_is_redacted() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("sonnet").await;
        let answer = format!(
            r#"{{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"text","text":"the key was {}"}}],"stop_reason":"end_turn","stop_sequence":null,"usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
            KEY.replace('-', "\\u002d")
        );
        fixture.upstream.reply(Reply::Status {
            status: 200,
            body: Bytes::from(answer),
            retry_after: None,
        });
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                MESSAGE,
            )
            .await;
        assert_eq!(got.status, 200, "{got:?}");
        let text = got.json()["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(text, "the key was [redacted]");
        drop(model_calls(&handle, records).await);
        fixture.stop().await;
    })
    .await;
}

/// The verification round: an event that holds the key once decoded (an
/// escaped key in a text delta, the key in an event name) is never
/// written, same API or translated; the call ends there with an error in
/// the front's form.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_quoting_the_key_ends_the_call() {
    within(async {
        let fixture = Fixture::new().await;
        let escaped = KEY.replace('-', "\\u002d");
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\
                     \"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\
                     \"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
        let same_api = [
            format!(
                "{start}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\
                 \"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{escaped}\"}}}}\n\n"
            ),
            format!("{start}event: ping-{KEY}\ndata: {{\"type\":\"ping\"}}\n\n"),
        ];
        let (sonnet, token, records) = fixture.session("sonnet").await;
        for body in same_api {
            fixture.upstream.reply(Reply::Stream {
                body: Bytes::from(body),
                piece: 3,
            });
            let got = fixture
                .call(
                    "POST /v1/messages",
                    &[("authorization", &format!("Bearer {token}"))],
                    &streamed(),
                )
                .await;
            assert!(got.body.contains("event: message_start"), "{got:?}");
            assert!(got.body.contains("event: error"), "{got:?}");
            assert!(got.body.contains("quoted the call's key"), "{got:?}");
            assert!(!got.body.contains("u002d"), "{got:?}");
            assert!(!got.body.contains(KEY), "{got:?}");
        }
        let calls = model_calls(&sonnet, records).await;
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls
                .iter()
                .all(|call| call.failure == Some(ModelFailure::UpstreamError)),
            "{calls:?}"
        );

        let (qwen, token, records) = fixture.session("qwen").await;
        fixture.upstream.reply(Reply::Stream {
            body: Bytes::from(format!(
                "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{escaped}\"}},\
                 \"finish_reason\":null}}]}}\n\n"
            )),
            piece: 3,
        });
        let request = r#"{"model":"qwen/qwen3-coder","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                request,
            )
            .await;
        assert!(got.body.contains("event: error"), "{got:?}");
        assert!(got.body.contains("quoted the call's key"), "{got:?}");
        assert!(!got.body.contains(KEY), "{got:?}");
        let calls = model_calls(&qwen, records).await;
        assert_eq!(calls[0].failure, Some(ModelFailure::UpstreamError));
        fixture.stop().await;
    })
    .await;
}

/// The verification round: a same-API stream that ends cleanly before its
/// answer did (no `message_stop`) is reported to the harness as an error
/// event, and recorded as truncated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_cut_short_is_reported_to_the_harness() {
    within(async {
        let fixture = Fixture::new().await;
        let (handle, token, records) = fixture.session("sonnet").await;
        fixture.upstream.reply(Reply::Stream {
            body: deltas(2, 8),
            piece: 17,
        });
        let got = fixture
            .call(
                "POST /v1/messages",
                &[("authorization", &format!("Bearer {token}"))],
                &streamed(),
            )
            .await;
        assert_eq!(got.status, 200, "{got:?}");
        assert!(got.body.contains("event: content_block_delta"), "{got:?}");
        assert!(got.body.contains("event: error"), "{got:?}");
        assert!(got.body.contains("ended before the message did"), "{got:?}");
        let calls = model_calls(&handle, records).await;
        assert_eq!(calls[0].failure, Some(ModelFailure::Truncated));
        fixture.stop().await;
    })
    .await;
}

/// Past [`super::listen::MAX_CONNECTIONS`] a connection is answered 503
/// and closed before any byte is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_past_the_limit_are_refused() {
    within(async {
        let fixture = Fixture::new().await;
        let mut held = Vec::new();
        for _ in 0..super::listen::MAX_CONNECTIONS {
            held.push(TcpStream::connect(fixture.proxy).await.unwrap());
        }
        let mut extra = TcpStream::connect(fixture.proxy).await.unwrap();
        let mut answer = Vec::new();
        let _read = extra.read_to_end(&mut answer).await.unwrap();
        assert!(
            answer.starts_with(b"HTTP/1.1 503 "),
            "{}",
            String::from_utf8_lossy(&answer)
        );
        drop(held);
        fixture.stop().await;
    })
    .await;
}
