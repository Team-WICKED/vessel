//! Transfers across a fleet (protocol 6, plan F4): a member pulls a
//! session and a memory from another member through the home, on an
//! in-process relay. The target sends the source's export, reads and close
//! in the envelope as its own caller, so the source applies its grant to
//! the home and its ownership rules to that caller, and the transfer it
//! opens belongs to that delivery alone. Every node's harness homes are
//! under its fixture's temporary user home.

use std::path::{Path, PathBuf};

use vessel_client::NodeClient;
use vessel_harness::Profile;
use vessel_harness::fakes::claude_project;
use vessel_node::fleet::DEFAULT_GRANT;
use vessel_proto::{
    AgentEvent, ContentBlock, ErrorCode, EventSeq, HarnessKind, HarnessSessionId, ImportMemory,
    ImportSession, KillSession, MemoryRef, Scope, SessionId, SessionOrigin, SessionRef,
    SessionStatus,
};

use crate::across::{Member, home, member_granting};
use crate::agents::code;
use crate::fleet::write;
use crate::secure::Relay;
use crate::support::{agent_of, ask, read_until, status_of, structured_spec, within};

/// The default Claude Code profile's home on `member`'s node.
fn claude_home(member: &Member) -> PathBuf {
    member
        .fixture
        .harness_env()
        .home(&Profile::default_for(HarnessKind::ClaudeCode))
        .unwrap()
}

/// Where an import for directory `cwd` puts Claude Code's project in
/// `home`: named after the directory's resolved form (macOS's temporary
/// directories are behind `/var` -> `/private/var`); Windows keeps the
/// given form.
fn project_in(home: &Path, cwd: &Path) -> PathBuf {
    #[cfg(unix)]
    let cwd = cwd.canonicalize().unwrap();
    #[cfg(not(unix))]
    let cwd = cwd.to_owned();
    claude_project(home, &cwd).unwrap()
}

/// Follow `id` on `client` until its status passes `wanted`.
async fn until_status(client: &NodeClient, id: SessionId, wanted: impl Fn(&SessionStatus) -> bool) {
    let mut stream = client.follow(id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    read_until(&mut stream, &mut seen, |record| {
        status_of(record).is_some_and(&wanted)
    })
    .await;
}

/// A home, a laptop that pulls (it may relay through the home) and a
/// desktop that is pulled from, granting the home `admin` too, which
/// copying its memory needs.
async fn fleet(relay: &Relay, sources: &Path) -> [Member; 3] {
    let home = home(relay, sources).await;
    let laptop = member_granting(
        relay,
        &home,
        "laptop",
        vec![Scope::Sync, Scope::Relay],
        DEFAULT_GRANT.to_vec(),
    )
    .await;
    let desktop = member_granting(
        relay,
        &home,
        "desktop",
        vec![Scope::Sync],
        vec![Scope::Read, Scope::Operate, Scope::Admin],
    )
    .await;
    [home, laptop, desktop]
}

/// A session on `desktop`, its conversation and its project's memory where
/// Claude Code keeps them.
async fn conversation(desktop: &Member) -> (SessionId, HarnessSessionId) {
    let id = desktop
        .owner
        .create_session(structured_spec(&desktop.fixture, None))
        .await
        .unwrap()
        .id;
    until_status(&desktop.owner, id, |status| {
        *status == SessionStatus::Running
    })
    .await;
    let session = desktop
        .owner
        .session(id)
        .await
        .unwrap()
        .harness_session
        .unwrap();
    let project = claude_project(&claude_home(desktop), &desktop.fixture.cwd()).unwrap();
    write(&project.join(format!("{session}.jsonl")), "{\"turn\":1}\n");
    write(&project.join("memory/MEMORY.md"), "desktop's\n");
    (id, session)
}

/// Into the laptop's own directory, resumed there.
fn to_laptop(laptop: &Member, from: SessionRef) -> ImportSession {
    ImportSession {
        from,
        profile: None,
        cwd: Some(laptop.fixture.cwd()),
        start: true,
    }
}

/// The session moved to the laptop as `vessel session move` does it:
/// imported and resumed there, then stopped on the desktop.
async fn moved(laptop: &Member, desktop: &Member, from: SessionRef, session: &HarnessSessionId) {
    let imported = laptop
        .owner
        .import_session(to_laptop(laptop, from))
        .await
        .unwrap();
    assert_eq!(imported.written, 1);
    let copy = imported.session.unwrap();
    assert_eq!(
        copy.origin,
        Some(SessionOrigin::Imported {
            node: from.node,
            session: from.session,
        })
    );
    let mut stream = laptop.owner.follow(copy.id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    read_until(&mut stream, &mut seen, |record| {
        matches!(
            agent_of(record),
            Some(AgentEvent::MessageChunk { content: ContentBlock::Text { text } })
                if *text == format!("replayed {session}")
        )
    })
    .await;
    assert_eq!(
        ask(&laptop.owner, &mut stream, &mut seen, copy.id, "loaded").await,
        format!("loaded {session}")
    );
    assert_eq!(
        ask(&laptop.owner, &mut stream, &mut seen, copy.id, "cwd").await,
        format!("cwd {}", laptop.fixture.cwd().display())
    );
    let placed = project_in(&claude_home(laptop), &laptop.fixture.cwd());
    assert_eq!(
        std::fs::read_to_string(placed.join(format!("{session}.jsonl"))).unwrap(),
        "{\"turn\":1}\n"
    );
    assert!(!placed.join("memory").exists(), "memory stays per profile");
    let _stopped = laptop
        .owner
        .call_on(
            from.node,
            KillSession {
                session: from.session,
            },
        )
        .await
        .unwrap();
    until_status(&desktop.owner, from.session, |status| !status.is_live()).await;
    // The desktop's transfers were closed once the laptop had them.
    let staging = desktop.fixture.data_dir().root().join("transfers");
    let left = std::fs::read_dir(&staging).map_or(0, Iterator::count);
    assert_eq!(left, 0, "{}", staging.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_moves_and_a_memory_copies_between_members_through_the_home() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let [home, laptop, desktop] = fleet(&relay, sources.path()).await;
        let (id, session) = conversation(&desktop).await;
        let from = SessionRef {
            node: desktop.id(),
            session: id,
        };

        // Claude Code has never run on the laptop: no home to place into,
        // and none is made.
        let refused = laptop.owner.import_session(to_laptop(&laptop, from)).await;
        assert_eq!(code(refused), ErrorCode::Conflict);
        assert!(!claude_home(&laptop).exists());
        std::fs::create_dir_all(claude_home(&laptop)).unwrap();
        moved(&laptop, &desktop, from, &session).await;

        // The project's memory, into the laptop's project.
        let copied = laptop
            .owner
            .import_memory(ImportMemory {
                from: MemoryRef {
                    node: desktop.id(),
                    profile: None,
                    project: Some(desktop.fixture.cwd()),
                },
                profile: None,
                project: Some(laptop.fixture.cwd()),
            })
            .await
            .unwrap();
        assert_eq!((copied.written, copied.same), (1, 0));
        let placed = project_in(&claude_home(&laptop), &laptop.fixture.cwd());
        assert_eq!(
            std::fs::read_to_string(placed.join("memory/MEMORY.md")).unwrap(),
            "desktop's\n"
        );

        // The home pulls from a member with its own grant there: the laptop
        // granted it `read` and `operate`, not the `admin` memory needs.
        let refused = home
            .owner
            .import_memory(ImportMemory {
                from: MemoryRef {
                    node: laptop.id(),
                    profile: None,
                    project: None,
                },
                profile: None,
                project: None,
            })
            .await;
        assert_eq!(code(refused), ErrorCode::Forbidden);
        relay.stop().await;
    })
    .await;
}
