//! Harness profiles end to end (milestone 4 Track H1): `profiles.toml` in
//! the data directory, `profile.list` and `profile.reload`, `session.create`
//! with a profile, and what a session records (its profile and its
//! creator), across a restart. `fake-tui` reports the environment it got.

use bytes::Bytes;
use vessel_client::secure::{RouteTimeouts, SecureConnector, WsDialer, pair_over};
use vessel_client::{Connector as _, NodeClient, Pinned, SecureTarget, SessionStream};
use vessel_node::{Config, Node, NodeError};
use vessel_proto::{
    Creator, ErrorCode, EventRecord, EventSeq, HarnessKind, ProfileName, RouteUrl, SessionId,
    SessionInput, SessionStatus, TerminalBytes, TerminalInput,
};
use vessel_relay::TlsConfig;
use vessel_rpc::CallError;

use crate::support::{Fixture, client, output_of, terminal_spec, within};

/// A value that must never leave the node.
const SECRET: &str = "sk-e2e-profile-secret-31";

/// An inherited variable the fake does not need, for `unset`.
fn inherited() -> &'static str {
    if cfg!(windows) { "USERPROFILE" } else { "HOME" }
}

fn profiles_toml() -> String {
    format!(
        r#"
[profiles.claude-alt]
harness = "claude"
label = "second account"
env = {{ CLAUDE_CONFIG_DIR = "/homes/alt", ANTHROPIC_AUTH_TOKEN = "{SECRET}" }}
unset = ["{unset}"]

[profiles.claude-work]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = "/homes/work" }}

[profiles.codex-work]
harness = "codex"
env = {{ CODEX_HOME = "/homes/codex" }}
"#,
        unset = inherited()
    )
}

fn write_profiles(fixture: &Fixture, text: &str) {
    std::fs::write(fixture.data_dir().profiles_path(), text).unwrap();
}

fn name(text: &str) -> ProfileName {
    text.parse().unwrap()
}

fn typed(text: &str) -> SessionInput {
    SessionInput::Terminal {
        input: TerminalInput::Data {
            data: TerminalBytes::new(Bytes::from(format!("{text}\r"))),
        },
    }
}

/// Read until the output holds one of `needles`; returns the first found.
async fn until_any<'n>(
    stream: &mut SessionStream,
    seen: &mut Vec<EventRecord>,
    needles: &[&'n str],
) -> &'n str {
    loop {
        let output = output_of(seen);
        if let Some(found) = needles.iter().find(|needle| output.contains(**needle)) {
            return found;
        }
        let record = stream
            .next()
            .await
            .unwrap_or_else(|| panic!("stream ended before {needles:?}: {output:?}"))
            .unwrap();
        seen.push(record);
    }
}

/// A terminal Claude session under `profile`; returns its id, after
/// checking what `fake-tui` reports for the home variable and for
/// [`inherited`].
async fn run_under(
    client: &NodeClient,
    fixture: &Fixture,
    profile: Option<&str>,
    home: Option<&str>,
    unset: bool,
) -> SessionId {
    let mut spec = terminal_spec(fixture, HarnessKind::ClaudeCode);
    spec.profile = profile.map(name);
    let info = client.create_session(spec).await.unwrap();
    assert_eq!(info.profile, profile.map(name));
    assert_eq!(info.created_by, Creator::Owner);
    let mut stream = client.follow(info.id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    let _prompt = until_any(&mut stream, &mut seen, &["> "]).await;

    client
        .send(info.id, typed("env CLAUDE_CONFIG_DIR"))
        .await
        .unwrap();
    // A profile's own home, or else the fixture's account's, which has no
    // home variable: never the test process's own (`HarnessEnv`).
    let expected = home.map_or_else(
        || "env CLAUDE_CONFIG_DIR unset".to_owned(),
        |home| format!("env CLAUDE_CONFIG_DIR={home}"),
    );
    let found = until_any(
        &mut stream,
        &mut seen,
        &[&expected, "env CLAUDE_CONFIG_DIR"],
    )
    .await;
    // The echoed command line holds the prefix too; read on to the answer.
    if found != expected {
        let _answer = until_any(&mut stream, &mut seen, &[&expected]).await;
    }

    client
        .send(info.id, typed(&format!("env {}", inherited())))
        .await
        .unwrap();
    let removed = format!("env {} unset", inherited());
    let kept = format!("env {}=", inherited());
    let answer = until_any(&mut stream, &mut seen, &[&removed, &kept]).await;
    assert_eq!(answer == removed, unset, "{answer}");
    client.kill(info.id).await.unwrap();
    info.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profiles_are_listed_without_values_and_sessions_run_in_their_homes() {
    within(async {
        let fixture = Fixture::new();
        write_profiles(&fixture, &profiles_toml());
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;

        let listed = client.profiles(None).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|profile| profile.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude",
                "codex",
                "gemini",
                "opencode",
                "claude-alt",
                "claude-work",
                "codex-work"
            ]
        );
        let alt = listed
            .iter()
            .find(|profile| profile.name.as_str() == "claude-alt")
            .unwrap();
        assert_eq!(alt.label.as_deref(), Some("second account"));
        let sets: Vec<&str> = alt.sets.iter().map(vessel_proto::EnvName::as_str).collect();
        assert_eq!(sets, ["ANTHROPIC_AUTH_TOKEN", "CLAUDE_CONFIG_DIR"]);
        let json = serde_json::to_string(&listed).unwrap();
        assert!(
            !json.contains(SECRET) && !json.contains("/homes/"),
            "{json}"
        );
        let codex: Vec<String> = client
            .profiles(Some(HarnessKind::Codex))
            .await
            .unwrap()
            .into_iter()
            .map(|profile| profile.name.to_string())
            .collect();
        assert_eq!(codex, ["codex", "codex-work"]);

        // Two profiles, two homes; `unset` removes; the default changes
        // nothing.
        let alt_session = run_under(
            &client,
            &fixture,
            Some("claude-alt"),
            Some("/homes/alt"),
            true,
        )
        .await;
        let _work = run_under(
            &client,
            &fixture,
            Some("claude-work"),
            Some("/homes/work"),
            false,
        )
        .await;
        let _default = run_under(&client, &fixture, None, None, false).await;
        let _named_default = run_under(&client, &fixture, Some("claude"), None, false).await;

        // Refused before anything starts: an unknown profile, and one of
        // another harness.
        for wrong in ["nope", "codex-work"] {
            let mut spec = terminal_spec(&fixture, HarnessKind::ClaudeCode);
            spec.profile = Some(name(wrong));
            let Err(CallError::Remote(error)) = client.create_session(spec).await else {
                panic!("{wrong} must be refused");
            };
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{wrong}: {error:?}");
            assert!(error.message.contains(wrong), "{error:?}");
        }

        // The profile and the creator are stored with the session.
        let ended = client.session(alt_session).await.unwrap();
        assert_eq!(ended.profile, Some(name("claude-alt")));
        node.stop().await.unwrap();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        let stored = client.session(alt_session).await.unwrap();
        assert_eq!(stored.profile, Some(name("claude-alt")));
        assert_eq!(stored.spec.profile, Some(name("claude-alt")));
        assert_eq!(stored.created_by, Creator::Owner);
        assert!(!stored.status.is_live());
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_applies_a_good_file_and_keeps_the_profiles_on_a_bad_one() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        assert_eq!(client.profiles(None).await.unwrap().len(), 4);

        write_profiles(&fixture, &profiles_toml());
        let reloaded = client.reload_profiles().await.unwrap();
        assert_eq!(reloaded.len(), 7);
        assert_eq!(client.profiles(None).await.unwrap(), reloaded);

        // A broken file: refused with where it is wrong, never a value, and
        // the profiles in effect stay.
        let broken = format!(
            "[profiles.claude-alt]\nharness = \"claude\"\nenv = {{ TOKEN = \"{SECRET}\" }}\nunset = [\"TOKEN\"]\n"
        );
        write_profiles(&fixture, &broken);
        let Err(CallError::Remote(error)) = client.reload_profiles().await else {
            panic!("a broken file must be refused");
        };
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(
            error.message.contains("claude-alt") && error.message.contains("TOKEN"),
            "{error:?}"
        );
        assert!(!error.message.contains(SECRET), "{error:?}");
        let syntax = format!("[profiles.x]\nharness = \"claude\"\nenv = {{ TOKEN = \"{SECRET}\" \n");
        write_profiles(&fixture, &syntax);
        let Err(CallError::Remote(error)) = client.reload_profiles().await else {
            panic!("a broken file must be refused");
        };
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(error.message.contains("line 3"), "{error:?}");
        assert!(!error.message.contains(SECRET), "{error:?}");
        assert_eq!(client.profiles(None).await.unwrap(), reloaded);

        // A session still runs under a profile from the set in effect.
        let mut spec = terminal_spec(&fixture, HarnessKind::ClaudeCode);
        spec.profile = Some(name("claude-work"));
        let info = client.create_session(spec).await.unwrap();
        client.kill(info.id).await.unwrap();
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_broken_profiles_file_stops_the_node_from_starting() {
    within(async {
        let fixture = Fixture::new();
        write_profiles(&fixture, "[profiles.claude]\nharness = \"codex\"\n");
        let mut config = Config::new(fixture.data_dir(), fixture.harness_env());
        config.bind = Some(std::net::SocketAddr::from((
            std::net::Ipv4Addr::LOCALHOST,
            0,
        )));
        let Err(error) = Node::start(config).await else {
            panic!("the node must not start");
        };
        assert!(matches!(error, NodeError::Profiles(_)), "{error:?}");
        // The data directory is free again for a node with a good file.
        write_profiles(&fixture, &profiles_toml());
        let node = fixture.start().await;
        node.stop().await.unwrap();
    })
    .await;
}

/// A paired device's session records the device as its creator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_devices_session_records_the_device() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let owner = fixture.connect(&node).await;
        let direct: RouteUrl = format!("ws://{}", node.address().unwrap()).parse().unwrap();
        let dialer = || WsDialer::new(TlsConfig::with_roots(rustls::RootCertStore::empty()).unwrap());
        let mut offer = owner
            .invite_device(None, 600)
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        offer.direct = vec![direct.clone()];
        let (paired, _route) = pair_over(&dialer(), &offer, "laptop", RouteTimeouts::default())
            .await
            .unwrap();
        let target = SecureTarget {
            key: paired.credential.key,
            direct: vec![direct],
            relays: Vec::new(),
        };
        let connector = SecureConnector::new(
            dialer(),
            target,
            paired.node.id,
            paired.credential.clone(),
        )
        .unwrap();
        let device = Pinned::new(connector, Some(paired.node.id))
            .connect(client())
            .await
            .unwrap();

        let info = device
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let expected = Creator::Device {
            id: paired.credential.device,
        };
        assert_eq!(info.created_by, expected);
        assert_eq!(owner.session(info.id).await.unwrap().created_by, expected);
        owner.kill(info.id).await.unwrap();
        let mut stream = owner.follow(info.id, EventSeq::ZERO).await.unwrap();
        while let Some(record) = stream.next().await {
            let record = record.unwrap();
            if matches!(&record.event, vessel_proto::SessionEvent::Status { status } if !status.is_live())
            {
                break;
            }
        }
        node.stop().await.unwrap();
        let node = fixture.start().await;
        let owner = fixture.connect(&node).await;
        assert_eq!(owner.session(info.id).await.unwrap().created_by, expected);
        assert!(matches!(
            owner.session(info.id).await.unwrap().status,
            SessionStatus::Exited { .. } | SessionStatus::Interrupted
        ));
        node.stop().await.unwrap();
    })
    .await;
}
