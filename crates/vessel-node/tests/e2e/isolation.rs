//! Guard: a node started in process with a temporary home reaches nothing
//! of the account it runs under, whatever that account's environment says.
//!
//! [Decision Log]
//! - Purpose: a test once wrote into the machine's real `~/.claude` through
//!   an in-process node that resolved the default profile's home from the
//!   process. `HarnessEnv` closes that by construction; this test makes a
//!   regression fail loudly instead of writing somewhere.
//! - How: the test binary runs this one test again as a child process with
//!   `HOME`, `USERPROFILE`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` pointing
//!   into a sentinel directory, whose harness homes exist (so a wrong
//!   resolution would find them and write). The child starts a node with
//!   its fixture's temporary home, makes it a fleet home that applies its
//!   bundle to every profile's home (the default ones included), and runs a
//!   default-profile session that reports its environment. The parent then
//!   checks that the child passed and that the sentinel holds no file.
//! - Why a child process: the environment is process-global and tests run
//!   in parallel, so changing it here would change every other test's.
//! - Not vacuous: the child checks that the bundle did land in the
//!   temporary home's harness homes, so the apply ran.

use std::path::{Path, PathBuf};
use std::time::Duration;

use vessel_node::fleet::{self, FleetFiles, FleetInit};
use vessel_node::{FleetConfig, FleetTargets};
use vessel_proto::{EventSeq, SessionStatus};

use crate::fleet::{until, write};
use crate::support::{Fixture, ask, read_until, status_of, structured_spec};

/// Set in the child: run the body instead of spawning.
const CHILD: &str = "VESSEL_ISOLATION_CHILD";

/// This test's full name, to run only it in the child.
const NAME: &str = "isolation::a_node_with_a_temporary_home_touches_nothing_of_the_real_one";

#[test]
fn a_node_with_a_temporary_home_touches_nothing_of_the_real_one() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let sentinel = tempfile::tempdir().unwrap();
    let homes = [
        sentinel.path().join(".claude"),
        sentinel.path().join(".codex"),
        sentinel.path().join("claude-config"),
        sentinel.path().join("codex-home"),
    ];
    for home in &homes {
        std::fs::create_dir_all(home).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("HOME", sentinel.path())
        .env("USERPROFILE", sentinel.path())
        .env("CLAUDE_CONFIG_DIR", &homes[2])
        .env("CODEX_HOME", &homes[3])
        .output()
        .unwrap();
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "the child failed:\n{report}");
    assert!(
        report.contains("1 passed"),
        "the child ran nothing:\n{report}"
    );
    let written = files_under(sentinel.path());
    assert!(
        written.is_empty(),
        "written under the real account's home: {written:?}"
    );
}

/// Every file below `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// In the child, with the sentinel environment.
fn child() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        vessel_testkit::within(Duration::from_secs(60), Box::pin(child_body()))
            .await
            .expect("the child timed out");
    });
}

async fn child_body() {
    let fixture = Fixture::new();
    // The temporary home's harness homes exist, as the sentinel's do.
    let claude = fixture.user_home().join(".claude");
    let codex = fixture.user_home().join(".codex");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::create_dir_all(&codex).unwrap();
    let sources = fixture.cwd().join("sources");
    write(&sources.join("AGENTS.md"), "fleet instructions\n");
    std::fs::create_dir_all(sources.join("skills")).unwrap();
    let _toml = fleet::init(
        &FleetFiles::new(&fixture.data_dir()),
        &FleetInit {
            instructions: Some(sources.join("AGENTS.md")),
            skills: Some(sources.join("skills")),
            skip: Vec::new(),
        },
    )
    .unwrap();
    // Every profile's home, the default ones included: the real targets.
    let node = fixture
        .start_with(|config| {
            config.fleet = FleetConfig {
                targets: FleetTargets::Profiles,
                poll: Duration::from_millis(50),
                ..FleetConfig::default()
            };
        })
        .await;
    until("the bundle lands in the temporary home", || {
        claude.join("CLAUDE.md").is_file() && codex.join("AGENTS.md").is_file()
    })
    .await;

    // A default-profile session sees the temporary account only.
    let owner = fixture.connect(&node).await;
    let session = owner
        .create_session(structured_spec(&fixture, None))
        .await
        .unwrap()
        .id;
    let mut stream = owner.follow(session, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    read_until(&mut stream, &mut seen, |record| {
        status_of(record) == Some(&SessionStatus::Running)
    })
    .await;
    let config_dir = ask(
        &owner,
        &mut stream,
        &mut seen,
        session,
        "env CLAUDE_CONFIG_DIR",
    )
    .await;
    assert_eq!(config_dir, "env CLAUDE_CONFIG_DIR unset");
    if cfg!(unix) {
        let home = ask(&owner, &mut stream, &mut seen, session, "env HOME").await;
        assert_eq!(home, format!("env HOME={}", fixture.user_home().display()));
    }
    owner.kill(session).await.unwrap();
    node.stop().await.unwrap();
}
