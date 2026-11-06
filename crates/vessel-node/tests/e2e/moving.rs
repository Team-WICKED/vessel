//! Moving a structured session to another profile end to end (milestone 4
//! Track H2): `session.move_profile` refuses what cannot move and changes
//! nothing then; a move continues the conversation under the new profile,
//! and the session keeps that profile across a restart. The homes are
//! temporary directories laid out as Claude Code would have written them;
//! `fake-tui`, installed as `claude`, answers the login check.

use std::path::Path;

use vessel_client::NodeClient;
use vessel_harness::fakes::claude_project;
use vessel_proto::{
    ErrorCode, EventSeq, HarnessKind, ProfileName, SessionEvent, SessionId, SessionStatus,
};
use vessel_rpc::CallError;

use crate::support::{Fixture, ask, read_until, status_of, structured_spec, terminal_spec, within};

fn name(text: &str) -> ProfileName {
    text.parse().unwrap()
}

/// Two logged-in Claude profiles, one that is not, and a Codex one, with
/// their homes under `homes`.
fn profiles_toml(homes: &Path) -> String {
    format!(
        r#"
[profiles.claude-alt]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = '{alt}' }}

[profiles.claude-work]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = '{work}' }}

[profiles.claude-out]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = '{out}', FAKE_TUI_LOGGED_OUT = "1" }}

[profiles.codex-work]
harness = "codex"
"#,
        alt = homes.join("alt").display(),
        work = homes.join("work").display(),
        out = homes.join("out").display(),
    )
}

/// The node refused a move with `code`.
async fn refused(client: &NodeClient, session: SessionId, profile: &str, code: ErrorCode) {
    let Err(CallError::Remote(error)) = client
        .move_profile(session, Some(name(profile)), false)
        .await
    else {
        panic!("a move to {profile} must be refused");
    };
    assert_eq!(error.code, code, "{profile}: {error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_moves_to_a_logged_in_profile_and_stays_moved() {
    within(async {
        let fixture = Fixture::new();
        let homes = tempfile::tempdir().unwrap();
        for home in ["alt", "work", "out"] {
            std::fs::create_dir_all(homes.path().join(home)).unwrap();
        }
        std::fs::write(
            fixture.data_dir().profiles_path(),
            profiles_toml(homes.path()),
        )
        .unwrap();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;

        let mut spec = structured_spec(&fixture, None);
        spec.profile = Some(name("claude-alt"));
        let id = client.create_session(spec).await.unwrap().id;
        let mut stream = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        read_until(&mut stream, &mut seen, |record| {
            status_of(record) == Some(&SessionStatus::Running)
        })
        .await;
        let session = client.session(id).await.unwrap().harness_session.unwrap();
        let transcript = format!("{session}.jsonl");
        let project = claude_project(&homes.path().join("alt"), &fixture.cwd()).unwrap();
        std::fs::create_dir_all(project.join("memory")).unwrap();
        std::fs::write(project.join(&transcript), "{}\n").unwrap();
        std::fs::write(project.join("memory").join("MEMORY.md"), "kept\n").unwrap();

        // Refused, before anything is linked: a profile that is not logged
        // in, an unknown one, another harness's.
        refused(&client, id, "claude-out", ErrorCode::Conflict).await;
        refused(&client, id, "nope", ErrorCode::InvalidRequest).await;
        refused(&client, id, "codex-work", ErrorCode::InvalidRequest).await;
        assert!(!homes.path().join("out").join("projects").exists());
        assert_eq!(
            client.session(id).await.unwrap().profile,
            Some(name("claude-alt"))
        );

        let moved = client
            .move_profile(id, Some(name("claude-work")), false)
            .await
            .unwrap();
        assert_eq!(moved.profile, Some(name("claude-work")));
        assert_eq!(moved.status, SessionStatus::Running);
        read_until(&mut stream, &mut seen, |record| {
            matches!(record.event, SessionEvent::ProfileChanged { .. })
        })
        .await;
        assert_eq!(
            seen.last().unwrap().event,
            SessionEvent::ProfileChanged {
                from: Some(name("claude-alt")),
                to: Some(name("claude-work")),
            }
        );
        assert_eq!(
            ask(&client, &mut stream, &mut seen, id, "loaded").await,
            format!("loaded {session}")
        );
        assert_eq!(
            ask(&client, &mut stream, &mut seen, id, "env CLAUDE_CONFIG_DIR").await,
            format!(
                "env CLAUDE_CONFIG_DIR={}",
                homes.path().join("work").display()
            )
        );
        let linked = claude_project(&homes.path().join("work"), &fixture.cwd()).unwrap();
        assert!(linked.join(&transcript).is_file());
        assert!(!linked.join("memory").exists(), "memory/ was linked");

        // A terminal session cannot move.
        let mut terminal = terminal_spec(&fixture, HarnessKind::ClaudeCode);
        terminal.profile = Some(name("claude-alt"));
        let terminal = client.create_session(terminal).await.unwrap().id;
        refused(&client, terminal, "claude-work", ErrorCode::Unsupported).await;
        client.kill(terminal).await.unwrap();

        // The session keeps the profile it moved to; ended, it cannot move.
        client.kill(id).await.unwrap();
        read_until(&mut stream, &mut seen, |record| {
            status_of(record).is_some_and(|status| !status.is_live())
        })
        .await;
        node.stop().await.unwrap();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        let stored = client.session(id).await.unwrap();
        assert_eq!(stored.profile, Some(name("claude-work")));
        assert_eq!(stored.spec.profile, Some(name("claude-alt")));
        refused(&client, id, "claude-alt", ErrorCode::Conflict).await;
        node.stop().await.unwrap();
    })
    .await;
}
