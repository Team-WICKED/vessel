//! `vessel session copy|move` and `vessel fleet memory copy` (protocol 6,
//! plan F4) against a node in this process with two Claude profiles whose
//! homes are temporary directories laid out as Claude Code would have
//! written them. Only named profiles are used, so nothing reads the
//! machine's own `~/.claude`.

use std::path::{Path, PathBuf};

use vessel_client::NodeClient;
use vessel_harness::fakes::claude_project;
use vessel_proto::{
    ClientInfo, EventSeq, HarnessKind, HarnessSessionId, SessionEvent, SessionId, SessionMode,
    SessionSpec, SessionStatus,
};
use vessel_store::DataDir;

use crate::support::{Fixture, within};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `profiles.toml` naming `claude-alt` and `claude-work`, with their homes
/// under `<data>/homes`.
fn profiles(data: &Path) {
    let homes = data.join("homes");
    for home in ["alt", "work"] {
        std::fs::create_dir_all(homes.join(home)).unwrap();
    }
    let text = format!(
        "[profiles.claude-alt]\nharness = \"claude\"\nenv = {{ CLAUDE_CONFIG_DIR = '{}' }}\n\n\
         [profiles.claude-work]\nharness = \"claude\"\nenv = {{ CLAUDE_CONFIG_DIR = '{}' }}\n",
        homes.join("alt").display(),
        homes.join("work").display(),
    );
    std::fs::write(DataDir::new(data).profiles_path(), text).unwrap();
}

async fn owner(fixture: &Fixture) -> NodeClient {
    let token = DataDir::new(fixture.data())
        .read_token()
        .unwrap()
        .expose()
        .parse()
        .unwrap();
    let url = fixture.node_address().parse().unwrap();
    let client = ClientInfo {
        name: "vessel-cli-test".to_owned(),
        version: "0".to_owned(),
    };
    NodeClient::connect(&url, &token, client).await.unwrap()
}

/// Follow `id` until its status passes `wanted`.
async fn until_status(owner: &NodeClient, id: SessionId, wanted: impl Fn(&SessionStatus) -> bool) {
    let mut stream = owner.follow(id, EventSeq::ZERO).await.unwrap();
    while let Some(record) = stream.next().await {
        if let SessionEvent::Status { status } = record.unwrap().event
            && wanted(&status)
        {
            return;
        }
    }
    panic!("session {id} ended its log first");
}

/// A structured session under `claude-alt`, running, and its harness
/// session id.
async fn running(owner: &NodeClient, cwd: &str) -> (SessionId, HarnessSessionId) {
    let mut spec = SessionSpec::new(
        HarnessKind::ClaudeCode,
        SessionMode::Structured {
            model: None,
            resume: None,
        },
    )
    .with_cwd(PathBuf::from(cwd));
    spec.profile = Some("claude-alt".parse().unwrap());
    let id = owner.create_session(spec).await.unwrap().id;
    until_status(owner, id, |status| *status == SessionStatus::Running).await;
    let session = owner.session(id).await.unwrap().harness_session.unwrap();
    (id, session)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_and_memory_are_copied_and_moved_between_profiles() {
    within(async {
        let fixture = Fixture::start_with(|_, data| profiles(data)).await;
        let homes = fixture.data().join("homes");
        let owner = owner(&fixture).await;
        let cwd = fixture.cwd();
        let (id, session) = running(&owner, &cwd).await;
        let project = claude_project(&homes.join("alt"), Path::new(&cwd)).unwrap();
        write(&project.join(format!("{session}.jsonl")), "{}\n");
        write(&project.join("memory/MEMORY.md"), "alt's\n");
        let id_text = id.to_string();

        let copied = fixture
            .vessel(
                &["session", "copy", &id_text, "--profile", "claude-work"],
                b"",
            )
            .await;
        assert!(copied.ok, "{}", copied.stderr);
        assert!(
            copied.stdout.contains("under claude-work"),
            "{}",
            copied.stdout
        );
        assert!(
            copied.stdout.contains("(1 files written)"),
            "{}",
            copied.stdout
        );
        assert!(!copied.stdout.contains("resumed"), "{}", copied.stdout);
        let work = claude_project(&homes.join("work"), Path::new(&cwd)).unwrap();
        assert!(work.join(format!("{session}.jsonl")).is_file());

        // A move elsewhere: resumed there, stopped here.
        let other = fixture.data().join("elsewhere");
        std::fs::create_dir(&other).unwrap();
        let other_text = other.display().to_string();
        let moved = fixture
            .vessel(
                &[
                    "session",
                    "move",
                    &id_text,
                    "--profile",
                    "claude-work",
                    "--cwd",
                    &other_text,
                ],
                b"",
            )
            .await;
        assert!(moved.ok, "{}", moved.stderr);
        assert!(
            moved.stdout.contains("resumed there as"),
            "{}",
            moved.stdout
        );
        assert!(moved.stdout.contains("stopped here"), "{}", moved.stdout);
        until_status(&owner, id, |status| !status.is_live()).await;
        let renamed = claude_project(&homes.join("work"), &other).unwrap();
        assert!(renamed.join(format!("{session}.jsonl")).is_file());

        // Memory: one project's, into the other profile.
        let memory = fixture
            .vessel(
                &[
                    "fleet",
                    "memory",
                    "copy",
                    "--from-profile",
                    "claude-alt",
                    "--from-project",
                    &cwd,
                    "--profile",
                    "claude-work",
                ],
                b"",
            )
            .await;
        assert!(memory.ok, "{}", memory.stderr);
        assert!(
            memory.stdout.starts_with("1 memory files written"),
            "{}",
            memory.stdout
        );
        assert_eq!(
            std::fs::read_to_string(work.join("memory/MEMORY.md")).unwrap(),
            "alt's\n"
        );

        // A profile the node lacks: refused before anything is read or
        // written. (Every step names a profile: the default one's home is
        // this machine's own.)
        let unknown = fixture
            .vessel(&["session", "copy", &id_text, "--profile", "nope"], b"")
            .await;
        assert!(!unknown.ok);
        assert!(unknown.stderr.contains("nope"), "{}", unknown.stderr);
        fixture.stop().await;
    })
    .await;
}
