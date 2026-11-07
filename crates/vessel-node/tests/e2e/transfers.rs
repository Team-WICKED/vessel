//! Transfers end to end (protocol 6, plan F4), between two profiles of one
//! node: `session.export` refuses a turn in progress unless forced, a
//! snapshot is read back checked and then closed, `session.import` copies
//! a conversation into another profile and directory and resumes it there
//! with its origin recorded, never over a different file, and
//! `memory.import` writes beside what differs. The homes are temporary
//! directories laid out as Claude Code would have written them; only named
//! profiles are used, so nothing reads the machine's own `~/.claude`.

use std::path::{Path, PathBuf};

use vessel_client::{ChunkSink, NodeClient, SessionStream};
use vessel_fleet::{BlobStore, BlobWriter};
use vessel_harness::fakes::claude_project;
use vessel_proto::{
    AgentEvent, BundlePath, ContentBlock, ErrorCode, EventRecord, EventSeq, HarnessKind,
    HarnessSessionId, ImportMemory, ImportSession, MemoryRef, NodeId, ProfileName, SessionId,
    SessionInput, SessionMode, SessionOrigin, SessionRef, SessionSpec, SessionStatus,
    TransferDetail,
};
use vessel_rpc::CallError;

use crate::support::{
    Fixture, agent_of, ask, prompt, read_until, status_of, structured_spec, within,
};

/// Where an import into `home` for directory `cwd` puts Claude Code's
/// project: named after the directory's resolved form, as Claude Code
/// names it (macOS's temporary directories are behind `/var` ->
/// `/private/var`); Windows keeps the given form.
fn project_in(home: &Path, cwd: &Path) -> PathBuf {
    #[cfg(unix)]
    let cwd = cwd.canonicalize().unwrap();
    #[cfg(not(unix))]
    let cwd = cwd.to_owned();
    claude_project(home, &cwd).unwrap()
}

/// A directory reached through a symbolic link on Unix, so its given and
/// resolved spellings differ here as macOS's temporary directories do
/// everywhere; the directory itself elsewhere.
#[cfg(unix)]
fn through_link(dir: &Path) -> PathBuf {
    let real = dir.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    link
}

#[cfg(not(unix))]
fn through_link(dir: &Path) -> PathBuf {
    dir.to_owned()
}

/// The fleet's blob writer as the client's chunk sink: written as it
/// arrives, kept only if its hash is right.
struct Blob(BlobWriter);

impl ChunkSink for Blob {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(&mut self.0, data)
    }

    fn finish(self) -> std::io::Result<()> {
        self.0.finish().map(drop)
    }
}

fn name(text: &str) -> ProfileName {
    text.parse().unwrap()
}

fn profiles_toml(homes: &Path) -> String {
    format!(
        r#"
[profiles.claude-alt]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = '{alt}' }}

[profiles.claude-work]
harness = "claude"
env = {{ CLAUDE_CONFIG_DIR = '{work}' }}
"#,
        alt = homes.join("alt").display(),
        work = homes.join("work").display(),
    )
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The code a call failed with.
fn code<T: std::fmt::Debug>(result: Result<T, CallError>) -> ErrorCode {
    match result {
        Err(CallError::Remote(error)) => error.code,
        other => panic!("expected a refusal from the node, got {other:?}"),
    }
}

fn import(from: SessionRef, profile: &str, cwd: Option<PathBuf>, start: bool) -> ImportSession {
    ImportSession {
        from,
        profile: Some(name(profile)),
        cwd,
        start,
    }
}

/// A node with the two profiles, their homes under `homes`.
async fn start(fixture: &Fixture, homes: &Path) -> (vessel_node::Node, NodeClient, NodeId) {
    for home in ["alt", "work"] {
        std::fs::create_dir_all(homes.join(home)).unwrap();
    }
    std::fs::write(fixture.data_dir().profiles_path(), profiles_toml(homes)).unwrap();
    let node = fixture.start().await;
    let client = fixture.connect(&node).await;
    let id = client.node_info().await.unwrap().id;
    (node, client, id)
}

/// A structured session under `claude-alt`, between turns, with files
/// as Claude Code would have written them.
struct Source {
    id: SessionId,
    session: HarnessSessionId,
    stream: SessionStream,
    seen: Vec<EventRecord>,
    /// Home-relative: the transcript, a subagent's file, a file-history
    /// entry (the order of the manifest).
    transcript: String,
    subagent: String,
    history: String,
}

impl Source {
    async fn start(fixture: &Fixture, client: &NodeClient, alt: &Path) -> Self {
        let mut spec = structured_spec(fixture, None);
        spec.profile = Some(name("claude-alt"));
        let id = client.create_session(spec).await.unwrap().id;
        let mut stream = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        read_until(&mut stream, &mut seen, |record| {
            status_of(record) == Some(&SessionStatus::Running)
        })
        .await;
        let session = client.session(id).await.unwrap().harness_session.unwrap();
        let project = claude_project(alt, &fixture.cwd()).unwrap();
        let slug = project.file_name().unwrap().to_str().unwrap().to_owned();
        let source = Self {
            id,
            transcript: format!("projects/{slug}/{session}.jsonl"),
            subagent: format!("projects/{slug}/{session}/subagents/a.jsonl"),
            history: format!("file-history/{session}/1"),
            session,
            stream,
            seen,
        };
        write(&alt.join(&source.transcript), "{\"turn\":1}\n");
        write(&alt.join(&source.subagent), "{\"sub\":1}\n");
        write(&alt.join(&source.history), "v1\n");
        write(&project.join("memory/MEMORY.md"), "per profile\n");
        source
    }
}

/// Export refuses a turn in progress unless forced; a snapshot holds the
/// session's files (never memory), reads back checked, and is gone once
/// closed.
async fn exports(fixture: &Fixture, client: &NodeClient, alt: &Path, source: &mut Source) {
    let id = source.id;
    client.send(id, prompt("wait")).await.unwrap();
    assert_eq!(
        code(client.export_session(id, false).await),
        ErrorCode::Conflict
    );
    let forced = client.export_session(id, true).await.unwrap();
    client.close_transfer(forced.transfer).await.unwrap();
    client.send(id, SessionInput::CancelTurn).await.unwrap();
    read_until(&mut source.stream, &mut source.seen, |record| {
        matches!(agent_of(record), Some(AgentEvent::TurnEnded { .. }))
    })
    .await;

    let manifest = client.export_session(id, false).await.unwrap();
    let paths: Vec<&str> = manifest.files.keys().map(BundlePath::as_str).collect();
    assert_eq!(
        paths,
        [
            source.history.as_str(),
            source.transcript.as_str(),
            source.subagent.as_str()
        ]
    );
    let TransferDetail::Session {
        harness_session,
        cwd,
        info,
    } = &manifest.detail
    else {
        panic!("a session's transfer: {:?}", manifest.detail);
    };
    assert_eq!((harness_session, cwd), (&source.session, &fixture.cwd()));
    assert_eq!(info.id, id);
    // Streamed into a blob store, each file checked against its hash.
    let staging = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(staging.path().join("blobs")).unwrap();
    for (path, file) in &manifest.files {
        let writer = Blob(blobs.writer(file.hash).unwrap());
        client
            .fetch_transfer_file(manifest.transfer, file, writer)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(blobs.path(&file.hash)).unwrap(),
            std::fs::read(alt.join(path.as_str())).unwrap(),
            "{path}"
        );
    }
    // A manifest that lies about a file: refused, nothing kept.
    let (_, first) = manifest.files.iter().next().unwrap();
    let mut wrong = first.clone();
    wrong.hash = vessel_proto::BlobHash::from_bytes([9; 32]);
    let elsewhere = BlobStore::open(staging.path().join("other")).unwrap();
    let refused = client
        .fetch_transfer_file(
            manifest.transfer,
            &wrong,
            Blob(elsewhere.writer(wrong.hash).unwrap()),
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(!elsewhere.contains(&wrong.hash));
    client.close_transfer(manifest.transfer).await.unwrap();
    let some = manifest.files.values().next().unwrap();
    assert_eq!(
        code(client.read_transfer(manifest.transfer, some.hash, 0).await),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(client.close_transfer(manifest.transfer).await),
        ErrorCode::NotFound
    );
}

/// An import into `claude-work`, started: the files placed, the session
/// resuming the conversation and recording where it came from.
async fn resumed(
    fixture: &Fixture,
    client: &NodeClient,
    work: &Path,
    from: SessionRef,
    session: &HarnessSessionId,
) -> SessionId {
    let imported = client
        .import_session(import(from, "claude-work", None, true))
        .await
        .unwrap();
    assert_eq!(
        (imported.written, &imported.harness_session, &imported.cwd),
        (3, session, &fixture.cwd())
    );
    assert_eq!(imported.profile, Some(name("claude-work")));
    let copy = imported.session.unwrap();
    assert_eq!(copy.profile, Some(name("claude-work")));
    assert_eq!(
        copy.origin,
        Some(SessionOrigin::Imported {
            node: from.node,
            session: from.session,
        })
    );
    let mut stream = client.follow(copy.id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    // `session/load` replays the conversation it loaded.
    read_until(&mut stream, &mut seen, |record| {
        matches!(
            agent_of(record),
            Some(AgentEvent::MessageChunk { content: ContentBlock::Text { text } })
                if *text == format!("replayed {session}")
        )
    })
    .await;
    assert_eq!(
        ask(client, &mut stream, &mut seen, copy.id, "loaded").await,
        format!("loaded {session}")
    );
    let placed = claude_project(work, &fixture.cwd()).unwrap();
    assert_eq!(
        std::fs::read_to_string(placed.join(format!("{session}.jsonl"))).unwrap(),
        "{\"turn\":1}\n"
    );
    assert!(!placed.join("memory").exists(), "memory stays per profile");
    copy.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conversation_is_copied_to_another_profile_and_resumed_there() {
    within(async {
        let fixture = Fixture::new();
        let homes = tempfile::tempdir().unwrap();
        let (node, client, node_id) = start(&fixture, homes.path()).await;
        let (alt, work) = (homes.path().join("alt"), homes.path().join("work"));
        let mut source = Source::start(&fixture, &client, &alt).await;
        exports(&fixture, &client, &alt, &mut source).await;

        let from = SessionRef {
            node: node_id,
            session: source.id,
        };
        // Where it already is: refused. From another node, while this one
        // is in no fleet: nothing to reach it through (across a fleet:
        // `transfers_across.rs`).
        let here = import(from, "claude-alt", None, true);
        assert_eq!(code(client.import_session(here).await), ErrorCode::Conflict);
        let elsewhere = SessionRef {
            node: NodeId::random(),
            session: source.id,
        };
        let remote = import(elsewhere, "claude-work", None, true);
        assert_eq!(
            code(client.import_session(remote).await),
            ErrorCode::NotFound
        );

        let copy = resumed(&fixture, &client, &work, from, &source.session).await;

        // Into another directory: renamed for it; what is there already
        // is not written again.
        let session = &source.session;
        let other = tempfile::tempdir().unwrap();
        let elsewhere = through_link(other.path());
        let again = import(from, "claude-work", Some(elsewhere.clone()), false);
        let again = client.import_session(again).await.unwrap();
        assert_eq!((again.written, again.session.is_none()), (2, true));
        let renamed = project_in(&work, &elsewhere);
        assert!(renamed.join(format!("{session}.jsonl")).is_file());
        #[cfg(unix)]
        assert!(
            !claude_project(&work, &elsewhere).unwrap().exists(),
            "named after the resolved directory, not the link"
        );
        assert!(
            renamed
                .join(format!("{session}/subagents/a.jsonl"))
                .is_file()
        );

        // A different file in the way: nothing at all is written.
        write(&work.join(&source.history), "edited here\n");
        let third = tempfile::tempdir().unwrap();
        let blocked = import(from, "claude-work", Some(third.path().to_owned()), false);
        assert_eq!(
            code(client.import_session(blocked).await),
            ErrorCode::Conflict
        );
        assert!(!project_in(&work, third.path()).exists());
        assert!(!claude_project(&work, third.path()).unwrap().exists());
        assert_eq!(
            std::fs::read_to_string(work.join(&source.history)).unwrap(),
            "edited here\n"
        );

        client.kill(copy).await.unwrap();
        client.kill(source.id).await.unwrap();
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_goes_beside_what_differs_and_terminal_sessions_stay() {
    within(async {
        let fixture = Fixture::new();
        let homes = tempfile::tempdir().unwrap();
        let (node, client, node_id) = start(&fixture, homes.path()).await;
        let (alt, work) = (homes.path().join("alt"), homes.path().join("work"));
        let from_memory = claude_project(&alt, &fixture.cwd()).unwrap().join("memory");
        write(&from_memory.join("MEMORY.md"), "alt's\n");
        write(&from_memory.join("notes/a.md"), "a\n");
        let to_memory = claude_project(&work, &fixture.cwd())
            .unwrap()
            .join("memory");
        write(&to_memory.join("MEMORY.md"), "work's\n");

        let request = ImportMemory {
            from: MemoryRef {
                node: node_id,
                profile: Some(name("claude-alt")),
                project: Some(fixture.cwd()),
            },
            profile: Some(name("claude-work")),
            project: None,
        };
        let imported = client.import_memory(request.clone()).await.unwrap();
        assert_eq!((imported.written, imported.same), (1, 0));
        assert_eq!(imported.beside.len(), 1);
        assert!(imported.beside[0].as_str().ends_with("memory/MEMORY.md"));
        assert_eq!(
            std::fs::read_to_string(to_memory.join("MEMORY.md")).unwrap(),
            "work's\n"
        );
        assert_eq!(
            std::fs::read_to_string(to_memory.join("MEMORY.md.vessel-incoming")).unwrap(),
            "alt's\n"
        );
        assert_eq!(
            std::fs::read_to_string(to_memory.join("notes/a.md")).unwrap(),
            "a\n"
        );

        // Into another project: renamed for it.
        let other = tempfile::tempdir().unwrap();
        let mut elsewhere = request.clone();
        elsewhere.project = Some(other.path().to_owned());
        let moved = client.import_memory(elsewhere).await.unwrap();
        assert_eq!(moved.written, 2);
        let other_memory = project_in(&work, other.path()).join("memory");
        assert!(other_memory.join("notes/a.md").is_file());

        // All of a profile's memory into one project: refused.
        let mut all = request;
        all.from.project = None;
        all.project = Some(other.path().to_owned());
        assert_eq!(
            code(client.import_memory(all).await),
            ErrorCode::InvalidRequest
        );

        // A terminal session is not copied.
        let mut terminal = crate::support::terminal_spec(&fixture, HarnessKind::ClaudeCode);
        terminal.profile = Some(name("claude-alt"));
        let terminal = client.create_session(terminal).await.unwrap().id;
        assert_eq!(
            code(client.export_session(terminal, false).await),
            ErrorCode::Unsupported
        );
        assert_eq!(
            code(client.export_session(SessionId::random(), false).await),
            ErrorCode::NotFound
        );
        client.kill(terminal).await.unwrap();
        node.stop().await.unwrap();
    })
    .await;
}

/// Codex files a rollout by date and finds it by id, so a copy into
/// another profile and directory lands at the same path and resumes with
/// `session/load` given the new directory. Checked with fakes only
/// (fake-acp as `codex-acp`), never a Codex login: what the real adapter
/// does with that directory is recorded in ADR 0012.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_rollout_keeps_its_place_and_resumes_in_the_new_directory() {
    within(async {
        let fixture = Fixture::with_codex();
        let homes = tempfile::tempdir().unwrap();
        for home in ["a", "b"] {
            std::fs::create_dir_all(homes.path().join(home)).unwrap();
        }
        let toml = format!(
            "[profiles.codex-a]\nharness = \"codex\"\nenv = {{ CODEX_HOME = '{}' }}\n\n\
             [profiles.codex-b]\nharness = \"codex\"\nenv = {{ CODEX_HOME = '{}' }}\n",
            homes.path().join("a").display(),
            homes.path().join("b").display(),
        );
        std::fs::write(fixture.data_dir().profiles_path(), toml).unwrap();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        let node_id = client.node_info().await.unwrap().id;

        let mut spec = SessionSpec::new(
            HarnessKind::Codex,
            SessionMode::Structured {
                model: None,
                resume: None,
            },
        )
        .with_cwd(fixture.cwd());
        spec.profile = Some(name("codex-a"));
        let id = client.create_session(spec).await.unwrap().id;
        let mut stream = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        read_until(&mut stream, &mut seen, |record| {
            status_of(record) == Some(&SessionStatus::Running)
        })
        .await;
        let session = client.session(id).await.unwrap().harness_session.unwrap();
        let rollout = format!("sessions/2026/11/06/rollout-2026-11-06T10-00-00-{session}.jsonl");
        write(&homes.path().join("a").join(&rollout), "{}\n");

        let other = tempfile::tempdir().unwrap();
        let from = SessionRef {
            node: node_id,
            session: id,
        };
        let imported = client
            .import_session(import(from, "codex-b", Some(other.path().to_owned()), true))
            .await
            .unwrap();
        assert_eq!(imported.written, 1);
        assert!(homes.path().join("b").join(&rollout).is_file(), "same path");
        let copy = imported.session.unwrap();
        let mut copy_stream = client.follow(copy.id, EventSeq::ZERO).await.unwrap();
        let mut copy_seen = Vec::new();
        read_until(&mut copy_stream, &mut copy_seen, |record| {
            status_of(record) == Some(&SessionStatus::Running)
        })
        .await;
        assert_eq!(
            ask(&client, &mut copy_stream, &mut copy_seen, copy.id, "loaded").await,
            format!("loaded {session}")
        );
        assert_eq!(
            ask(&client, &mut copy_stream, &mut copy_seen, copy.id, "cwd").await,
            format!("cwd {}", other.path().display())
        );
        client.kill(copy.id).await.unwrap();
        client.kill(id).await.unwrap();
        node.stop().await.unwrap();
    })
    .await;
}
