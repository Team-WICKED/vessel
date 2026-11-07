//! Routed profiles end to end (milestone 5, ADR 0015): a profile with a
//! `model` route launches its harness pointed at the node's proxy, with a
//! token of the session's own, pinned to the route's models; a session
//! keeps its route when it moves, and its restarted agent gets a new token;
//! and the fake harnesses' calls (`model <text>`) go through the proxy to a
//! fake provider, in each harness's own API, and land in the session's log.

use std::path::Path;

use bytes::Bytes;
use vessel_client::{NodeClient, SessionStream};
use vessel_harness::fakes::claude_project;
use vessel_node::Node;
use vessel_node::fleet::{FleetFiles, set_secret};
use vessel_proto::{
    ErrorCode, EventRecord, EventSeq, HarnessKind, ModelApi, ModelCall, ModelId, ModelStop,
    ProfileName, SessionEvent, SessionId, SessionInfo, SessionModel, SessionStatus,
};
use vessel_rpc::CallError;
use vessel_testkit::upstream::{FakeUpstream, Reply};

use crate::support::{
    Fixture, ask, collect_until, keys, read_all, read_until, status_of, structured_spec,
    terminal_spec, within,
};

fn name(text: &str) -> ProfileName {
    text.parse().unwrap()
}

/// One route, `qwen`, at a provider that is never called.
const MODELS: &str = r#"
[providers.local]
api = "openai-chat-completions"
base_url = "http://127.0.0.1:9/v1"

[models.qwen]
provider = "local"
model = "qwen/qwen3-coder"
small_model = "qwen/qwen3-coder-flash"
"#;

/// Two Claude profiles on `qwen`, one on its harness's own provider, one
/// on a route `models.toml` does not have, and a Codex one on `qwen`.
fn profiles_toml(homes: &Path) -> String {
    format!(
        r#"
[profiles.claude-qwen]
harness = "claude"
model = "qwen"
env = {{ CLAUDE_CONFIG_DIR = '{alt}' }}

[profiles.claude-qwen-work]
harness = "claude"
model = "qwen"
env = {{ CLAUDE_CONFIG_DIR = '{work}' }}

[profiles.claude-own]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = '{work}' }}

[profiles.claude-ghost]
harness = "claude"
model = "ghost"

[profiles.codex-qwen]
harness = "codex"
model = "qwen"
"#,
        alt = homes.join("alt").display(),
        work = homes.join("work").display(),
    )
}

fn write_models(fixture: &Fixture, models: &str) {
    let path = fixture.data_dir().models_path();
    std::fs::write(&path, models).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// The value the agent reports for `variable`, or `None` when unset.
async fn env(
    client: &NodeClient,
    stream: &mut SessionStream,
    seen: &mut Vec<EventRecord>,
    session: SessionId,
    variable: &str,
) -> Option<String> {
    let reply = ask(client, stream, seen, session, &format!("env {variable}")).await;
    reply
        .strip_prefix(&format!("env {variable}="))
        .map(str::to_owned)
}

/// A node with `models` and the profiles of [`profiles_toml`] (homes
/// under `homes`).
async fn start(fixture: &Fixture, homes: &Path, models: &str) -> (Node, NodeClient) {
    for home in ["alt", "work"] {
        std::fs::create_dir_all(homes.join(home)).unwrap();
    }
    write_models(fixture, models);
    std::fs::write(fixture.data_dir().profiles_path(), profiles_toml(homes)).unwrap();
    let node = fixture.start().await;
    let client = fixture.connect(&node).await;
    (node, client)
}

/// A new structured session of `harness` under `profile`, running.
async fn running(
    fixture: &Fixture,
    client: &NodeClient,
    harness: HarnessKind,
    profile: &str,
) -> (SessionInfo, SessionStream, Vec<EventRecord>) {
    let mut spec = structured_spec(fixture, None);
    spec.harness = harness;
    spec.profile = Some(name(profile));
    let info = client.create_session(spec).await.unwrap();
    let mut stream = client.follow(info.id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    read_until(&mut stream, &mut seen, |record| {
        status_of(record) == Some(&SessionStatus::Running)
    })
    .await;
    (info, stream, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_are_listed_and_an_unknown_one_is_refused_by_name() {
    within(async {
        let fixture = Fixture::new();
        let homes = tempfile::tempdir().unwrap();
        let (node, client) = start(&fixture, homes.path(), MODELS).await;

        let routes = client.models().await.unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].name.as_str(), "qwen");
        let profiles = client.profiles(None).await.unwrap();
        let route_of = |profile: &str| {
            profiles
                .iter()
                .find(|info| info.name.as_str() == profile)
                .and_then(|info| info.model.as_ref())
                .map(ToString::to_string)
        };
        assert_eq!(route_of("claude-qwen").as_deref(), Some("qwen"));
        assert_eq!(route_of("claude-own"), None);

        // A route the node does not have: refused, and named.
        let mut ghost = structured_spec(&fixture, None);
        ghost.profile = Some(name("claude-ghost"));
        let Err(CallError::Remote(error)) = client.create_session(ghost).await else {
            panic!("a profile on an unknown route must be refused");
        };
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{error:?}");
        assert!(error.message.contains("ghost"), "{error:?}");
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routed_session_is_pointed_at_the_proxy_and_keeps_its_route() {
    within(async {
        let fixture = Fixture::new();
        let homes = tempfile::tempdir().unwrap();
        let (node, client) = start(&fixture, homes.path(), MODELS).await;
        let (info, mut stream, mut seen) =
            running(&fixture, &client, HarnessKind::ClaudeCode, "claude-qwen").await;
        let id = info.id;
        let recorded = Some(SessionModel {
            route: "qwen".parse().unwrap(),
            model: ModelId::new("qwen/qwen3-coder"),
        });
        assert_eq!(info.model, recorded);

        let base = env(&client, &mut stream, &mut seen, id, "ANTHROPIC_BASE_URL")
            .await
            .unwrap();
        assert!(base.starts_with("http://127.0.0.1:"), "{base}");
        let first = env(&client, &mut stream, &mut seen, id, "ANTHROPIC_AUTH_TOKEN")
            .await
            .unwrap();
        assert!(first.starts_with("vslm-"), "a token, not {first:.5}");
        for (variable, value) in [
            ("ANTHROPIC_MODEL", Some("qwen/qwen3-coder")),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                Some("qwen/qwen3-coder-flash"),
            ),
            ("ANTHROPIC_API_KEY", None),
        ] {
            assert_eq!(
                env(&client, &mut stream, &mut seen, id, variable)
                    .await
                    .as_deref(),
                value,
                "{variable}"
            );
        }

        // A move keeps the route: to a profile on the harness's own
        // provider it is refused, before anything is linked.
        let session = client.session(id).await.unwrap().harness_session.unwrap();
        let project = claude_project(&homes.path().join("alt"), &fixture.cwd()).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{session}.jsonl")), "{}\n").unwrap();
        let Err(CallError::Remote(error)) = client
            .move_profile(id, Some(name("claude-own")), false)
            .await
        else {
            panic!("a move off the session's route must be refused");
        };
        assert_eq!(error.code, ErrorCode::Unsupported, "{error:?}");
        assert!(error.message.contains("qwen"), "{error:?}");
        assert!(!homes.path().join("work").join("projects").exists());

        // On the same route it moves, and the new agent has a token of its
        // own.
        let moved = client
            .move_profile(id, Some(name("claude-qwen-work")), false)
            .await
            .unwrap();
        assert_eq!(moved.profile, Some(name("claude-qwen-work")));
        assert_eq!(moved.model, recorded);
        let second = env(&client, &mut stream, &mut seen, id, "ANTHROPIC_AUTH_TOKEN")
            .await
            .unwrap();
        assert!(second.starts_with("vslm-"));
        assert_ne!(second, first);
        client.kill(id).await.unwrap();
        node.stop().await.unwrap();
    })
    .await;
}

/// Codex, structured through its adapter: a `vessel` provider in `-c`
/// overrides, the token in its environment only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routed_codex_gets_a_vessel_provider() {
    within(async {
        let fixture = Fixture::with_codex();
        let homes = tempfile::tempdir().unwrap();
        let (node, client) = start(&fixture, homes.path(), MODELS).await;
        let (info, mut stream, mut seen) =
            running(&fixture, &client, HarnessKind::Codex, "codex-qwen").await;
        let codex = info.id;
        let base = "http://127.0.0.1:";
        let args = ask(&client, &mut stream, &mut seen, codex, "args").await;
        assert!(args.contains(r#"model_provider=\"vessel\""#), "{args}");
        assert!(args.contains(r#"model=\"qwen/qwen3-coder\""#), "{args}");
        assert!(args.contains(base) && args.contains("/v1"), "{args}");
        assert!(!args.contains("vslm-"), "{args}");
        let secret = env(
            &client,
            &mut stream,
            &mut seen,
            codex,
            "VESSEL_MODEL_SECRET",
        )
        .await
        .unwrap();
        assert!(secret.starts_with("vslm-"));
        client.kill(codex).await.unwrap();
        node.stop().await.unwrap();
    })
    .await;
}

/// The provider's key in these tests.
const KEY: &str = "sk-e2e-provider-key";

/// A short Chat Completions stream: `Hel`, `lo`, a stop, usage.
const CHAT_STREAM: &str = concat!(
    r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen/qwen3-coder","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen/qwen3-coder","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
    "\n\n",
    r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen/qwen3-coder","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":2,"total_tokens":22}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// The route `qwen` at `upstream`, its key a secret of the node's.
fn upstream_models(upstream: &FakeUpstream) -> String {
    format!(
        "[providers.local]\napi = \"openai-chat-completions\"\nbase_url = \"{}/v1\"\n\
         key = {{ secret = \"LOCAL_KEY\" }}\n\n\
         [models.qwen]\nprovider = \"local\"\nmodel = \"qwen/qwen3-coder\"\n",
        upstream.url()
    )
}

/// The model calls in session `id`'s log.
async fn calls(client: &NodeClient, id: SessionId) -> Vec<ModelCall> {
    read_all(client, id)
        .await
        .into_iter()
        .filter_map(|record| match record.event {
            SessionEvent::ModelCall(call) => Some(call),
            SessionEvent::Status { .. }
            | SessionEvent::Terminal(_)
            | SessionEvent::Agent { .. }
            | SessionEvent::ProfileChanged { .. } => None,
        })
        .collect()
}

/// Claude Code (structured and terminal) and Codex (structured), all on a
/// Chat Completions route: each call goes up with the provider's key and
/// comes back in the harness's own API, and the session's log records it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routed_sessions_calls_reach_the_provider_and_are_recorded() {
    within(async {
        let upstream = FakeUpstream::start().await.unwrap();
        let fixture = Fixture::with_codex();
        set_secret(
            &FleetFiles::new(&fixture.data_dir()),
            &"LOCAL_KEY".parse().unwrap(),
            KEY,
        )
        .unwrap();
        let homes = tempfile::tempdir().unwrap();
        let (node, client) = start(&fixture, homes.path(), &upstream_models(&upstream)).await;
        let stream = || Reply::Stream {
            body: Bytes::from_static(CHAT_STREAM.as_bytes()),
            piece: 23,
        };

        let mut sessions = Vec::new();
        for (harness, profile) in [
            (HarnessKind::ClaudeCode, "claude-qwen"),
            (HarnessKind::Codex, "codex-qwen"),
        ] {
            upstream.reply(stream());
            let (info, mut stream, mut seen) = running(&fixture, &client, harness, profile).await;
            let reply = ask(&client, &mut stream, &mut seen, info.id, "model hi").await;
            assert_eq!(reply, "model Hello", "{harness}");
            sessions.push(info.id);
        }

        upstream.reply(stream());
        let mut terminal = terminal_spec(&fixture, HarnessKind::ClaudeCode);
        terminal.profile = Some(name("claude-qwen"));
        let terminal = client.create_session(terminal).await.unwrap().id;
        let mut output = client.follow(terminal, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut output, &mut seen, "ready").await;
        client.send(terminal, keys("model hi\r")).await.unwrap();
        collect_until(&mut output, &mut seen, "model Hello").await;
        sessions.push(terminal);

        let seen = upstream.seen();
        assert_eq!(seen.len(), 3);
        for request in &seen {
            assert_eq!(request.path, "/v1/chat/completions");
            assert_eq!(
                request.header("authorization"),
                Some(format!("Bearer {KEY}").as_str())
            );
            assert!(
                request
                    .headers
                    .iter()
                    .all(|(_, value)| !value.contains("vslm-")),
                "{:?}",
                request.headers
            );
        }
        for id in sessions {
            client.kill(id).await.unwrap();
            let calls = calls(&client, id).await;
            assert_eq!(calls.len(), 1, "{calls:?}");
            let call = &calls[0];
            assert_eq!(call.route.as_str(), "qwen");
            assert_eq!(call.model.as_str(), "qwen/qwen3-coder");
            assert_eq!(call.api, ModelApi::OpenAiChatCompletions);
            assert_eq!(call.usage.input.get(), 20);
            assert_eq!(call.usage.output.get(), 2);
            assert_eq!(call.stop, ModelStop::EndTurn);
            assert_eq!(call.failure, None);
        }
        node.stop().await.unwrap();
    })
    .await;
}
