//! Deterministic timer checks: advance Tokio time, then await lifecycle events.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{Reply, lsp};
use vessel_testkit::{Fake, within};

use super::{Config, Key, Manager, Scope, Work, server};
use crate::registry::Override;
use crate::{LspError, Query};

/// The whole root, as a node grants a configured root.
fn scope(root: &std::path::Path) -> Scope {
    Scope::new(root.to_owned(), root.to_owned(), false).unwrap()
}

fn query(root: PathBuf) -> Query {
    Query::DocumentSymbols(lsp::DocumentSymbols {
        workspace: lsp::Workspace {
            root,
            language: lsp::Language::Rust,
        },
        file: lsp::FilePath::try_from(PathBuf::from("fixture.rs")).unwrap(),
        page: lsp::PageRequest::default(),
    })
}
fn config(mode: &str, state: &tempfile::TempDir) -> Config {
    let mut config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    config.registry.state_dir = Some(state.path().to_owned());
    config.registry.rust_toolchain = Some(vessel_testkit::rust_toolchain(state.path()).unwrap());
    let _previous = config.registry.overrides.insert(
        lsp::Language::Rust,
        Override {
            program: Fake::Lsp.path().unwrap(),
            args: vec![mode.into()],
        },
    );
    config
}
fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("fixture.rs"),
        "pub fn caller() {}\npub fn target() {}\n",
    )
    .unwrap();
    dir
}
async fn call(sender: &mpsc::Sender<Work>, query: Query) -> Result<Reply, LspError> {
    let (reply, response) = oneshot::channel();
    sender.send(Work { query, reply }).await.unwrap();
    response.await.unwrap()
}

#[tokio::test]
async fn an_idle_server_shuts_down_and_closes_its_mailbox() {
    within(Duration::from_secs(45), async {
        let state = tempfile::tempdir().unwrap();
        let dir = root();
        let root = dir.path().canonicalize().unwrap();
        let config = Config {
            idle_timeout: Duration::from_secs(5),
            ..config("normal", &state)
        };
        let (sender, receiver) = mpsc::channel(8);
        let task = tokio::spawn(server(
            Key {
                workspace: root.clone(),
                language: lsp::Language::Rust,
                full: false,
            },
            config,
            CancellationToken::new(),
            CancellationToken::new(),
            receiver,
            super::Tools::default(),
        ));
        assert!(call(&sender, query(root)).await.is_ok());
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::time::resume();
        sender.closed().await;
        task.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_crashed_server_restarts_only_after_backoff() {
    within(Duration::from_secs(45), async {
        let state = tempfile::tempdir().unwrap();
        let dir = root();
        let root = dir.path().canonicalize().unwrap();
        let stop = CancellationToken::new();
        let manager = Manager::start(config("crash", &state), stop.child_token());
        let at = lsp::AtPosition {
            workspace: lsp::Workspace {
                root: root.clone(),
                language: lsp::Language::Rust,
            },
            file: lsp::FilePath::try_from(PathBuf::from("fixture.rs")).unwrap(),
            position: lsp::Position {
                line: std::num::NonZeroU32::MIN,
                column: std::num::NonZeroU32::MIN,
            },
            page: lsp::PageRequest::default(),
        };
        assert!(
            manager
                .query(scope(&root), Query::Hover(lsp::GetHover(at)))
                .await
                .is_err()
        );
        assert!(matches!(
            manager.query(scope(&root), query(root.clone())).await,
            Err(LspError::Unavailable)
        ));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::time::resume();
        assert!(manager.query(scope(&root), query(root)).await.is_ok());
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}

/// With every slot its own, a root recycles its least recently used server
/// instead of exceeding the bound or being refused.
#[tokio::test]
async fn the_global_server_bound_is_enforced() {
    within(Duration::from_secs(45), async {
        let state = tempfile::tempdir().unwrap();
        let first = root();
        let first = first.path().canonicalize().unwrap();
        let mut config = Config {
            max_servers: NonZeroUsize::MIN,
            ..config("normal", &state)
        };
        let _previous = config.registry.overrides.insert(
            lsp::Language::Go,
            Override {
                program: Fake::Lsp.path().unwrap(),
                args: vec!["normal".into()],
            },
        );
        let stop = CancellationToken::new();
        let manager = Manager::start(config, stop.child_token());
        for language in [lsp::Language::Rust, lsp::Language::Go, lsp::Language::Rust] {
            let outcome = manager
                .query(scope(&first), in_language(&first, language))
                .await;
            assert!(outcome.is_ok(), "{language:?}: {outcome:?}");
        }
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}

fn in_language(root: &std::path::Path, language: lsp::Language) -> Query {
    Query::DocumentSymbols(lsp::DocumentSymbols {
        workspace: lsp::Workspace {
            root: root.to_owned(),
            language,
        },
        file: lsp::FilePath::try_from(PathBuf::from("fixture.rs")).unwrap(),
        page: lsp::PageRequest::default(),
    })
}

/// Review finding: open documents were never evicted, so 64 distinct files
/// locked every new file out for as long as queries kept the server alive.
#[tokio::test]
async fn the_least_recently_used_document_makes_room_for_a_new_one() {
    within(Duration::from_secs(90), async {
        let state = tempfile::tempdir().unwrap();
        let dir = root();
        let root = dir.path().canonicalize().unwrap();
        for index in 0..=crate::process::MAX_DOCUMENTS {
            std::fs::write(root.join(format!("f{index}.rs")), "pub fn target() {}\n").unwrap();
        }
        let file = |index: usize| {
            Query::DocumentSymbols(lsp::DocumentSymbols {
                workspace: lsp::Workspace {
                    root: root.clone(),
                    language: lsp::Language::Rust,
                },
                file: lsp::FilePath::try_from(PathBuf::from(format!("f{index}.rs"))).unwrap(),
                page: lsp::PageRequest::default(),
            })
        };
        let stop = CancellationToken::new();
        let manager = Manager::start(config("normal", &state), stop.child_token());
        for index in 0..=crate::process::MAX_DOCUMENTS {
            let outcome = manager.query(scope(&root), file(index)).await;
            assert!(outcome.is_ok(), "file {index}: {outcome:?}");
        }
        // The evicted first file can be opened again, evicting another.
        assert!(manager.query(scope(&root), file(0)).await.is_ok());
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}

/// Review finding: one root could hold every server slot while another
/// root was refused. A root holding fewer servers takes over the busiest
/// root's least recently used slot; a root holding as many recycles its own
/// and never takes another root's.
#[tokio::test]
async fn a_root_holding_every_slot_yields_one_to_another_root() {
    within(Duration::from_secs(45), async {
        let state = tempfile::tempdir().unwrap();
        let first = root();
        let second = root();
        let first = first.path().canonicalize().unwrap();
        let second = second.path().canonicalize().unwrap();
        let started = state.path().join("second-root-started");
        let interpreter = state.path().join("python");
        std::fs::write(&interpreter, "stand-in interpreter").unwrap();
        let mut config = Config {
            max_servers: NonZeroUsize::MIN.saturating_add(1),
            ..config("normal", &state)
        };
        config.registry.python_interpreter = Some(interpreter);
        for (language, extra) in [
            (lsp::Language::Go, None),
            (lsp::Language::Cpp, None),
            (lsp::Language::Python, Some(&started)),
        ] {
            let mut args = vec!["normal".to_owned()];
            if let Some(path) = extra {
                args.extend(["--record".to_owned(), path.to_string_lossy().into_owned()]);
            }
            let _previous = config.registry.overrides.insert(
                language,
                Override {
                    program: Fake::Lsp.path().unwrap(),
                    args,
                },
            );
        }
        let stop = CancellationToken::new();
        let manager = Manager::start(config, stop.child_token());
        let ask =
            |root: &PathBuf, language| manager.query(scope(root), in_language(root, language));
        for language in [lsp::Language::Rust, lsp::Language::Go] {
            let outcome = ask(&first, language).await;
            assert!(outcome.is_ok(), "{language:?}: {outcome:?}");
        }
        let outcome = ask(&second, lsp::Language::Python).await;
        assert!(
            outcome.is_ok(),
            "the second root waited for a slot: {outcome:?}"
        );
        std::fs::remove_file(&started).unwrap();
        // Each root now holds one server: the first recycles its own.
        let outcome = ask(&first, lsp::Language::Cpp).await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(ask(&second, lsp::Language::Python).await.is_ok());
        assert!(!started.exists(), "the second root's server was taken");
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}

/// Verification finding: a document that grows past the aggregate byte
/// budget evicts another during a bulk refresh; the refresh must skip the
/// evicted entry instead of failing and restarting a healthy server.
#[tokio::test]
async fn review_byte_eviction_during_refresh_keeps_server_healthy() {
    within(Duration::from_secs(45), async {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (watcher, _changes) = crate::sync::Watcher::new().await.unwrap();
        let spec = vessel_proc::CommandSpec::new(Fake::Lsp.path().unwrap())
            .args(["normal"])
            .cwd(&root)
            .env_clear();
        let mut process = crate::process::Process::start(
            &spec,
            crate::process::Launch {
                root: root.clone(),
                language: lsp::Language::Rust,
                settings: crate::protocol::initialize::Settings::new(false, &[]),
                request_timeout: Duration::from_secs(10),
                typescript_engine: None,
                python_interpreter: None,
            },
        )
        .await
        .unwrap();
        // 17 files just under 2 MB each fit the 32 MiB budget together;
        // growing the first one does not.
        for index in 0..17 {
            let name = format!("f{index:02}.rs");
            std::fs::write(root.join(&name), " ".repeat(1_970_000)).unwrap();
            let _uri = process
                .open(
                    &lsp::FilePath::try_from(PathBuf::from(name)).unwrap(),
                    &watcher,
                )
                .await
                .unwrap();
        }
        assert_eq!(process.documents.len(), 17);
        std::fs::write(root.join("f00.rs"), " ".repeat(2_090_000)).unwrap();
        let result = process.refresh_all(&watcher).await;
        let open = process.documents.len();
        process.shutdown().await;
        assert!(result.is_ok(), "eviction made refresh fail: {result:?}");
        assert_eq!(open, 16, "exactly one document made room");
    })
    .await
    .unwrap();
}

/// Verification finding: with A and B holding one slot each, A's first
/// replacement retired A's own server, and A's second then counted A as
/// empty and retired B's. A reserved replacement now counts toward its root.
#[tokio::test]
async fn review_parked_replacements_do_not_steal_an_equal_roots_slot() {
    let mut pool = super::Pool::new(
        Config {
            max_servers: NonZeroUsize::MIN.saturating_add(1),
            ..Config::default()
        },
        CancellationToken::new(),
    );
    let a = PathBuf::from("/a");
    let b = PathBuf::from("/b");
    let key = |workspace: &str| Key {
        workspace: PathBuf::from(workspace),
        language: lsp::Language::Go,
        full: false,
    };
    let (a_sender, _a_receiver) = mpsc::channel(8);
    let (b_sender, _b_receiver) = mpsc::channel(8);
    for (owned, root, sender) in [
        (key("/a/old"), a.clone(), a_sender),
        (key("/b/old"), b, b_sender),
    ] {
        drop(pool.servers.insert(
            owned,
            super::Slot {
                sender,
                root,
                retire: CancellationToken::new(),
                retiring: None,
                used: 0,
            },
        ));
    }
    let mut responses = Vec::new();
    for workspace in ["/a/new1", "/a/new2"] {
        let (reply, response) = oneshot::channel();
        responses.push(response);
        pool.admit(super::Admission {
            key: key(workspace),
            root: a.clone(),
            work: Work {
                query: in_language(&PathBuf::from(workspace), lsp::Language::Go),
                reply,
            },
        });
    }
    assert!(pool.servers[&key("/a/old")].retiring.is_some());
    assert!(
        pool.servers[&key("/b/old")].retiring.is_none(),
        "A's second parked replacement retired B despite equal shares"
    );
    assert_eq!(pool.parked.len(), 2);
}

/// Parking room is reserved before a server is retired: with the queue
/// full, a new root is refused and no server is cancelled for it.
#[tokio::test]
async fn no_server_is_retired_for_an_admission_that_cannot_wait() {
    let mut pool = super::Pool::new(
        Config {
            max_servers: NonZeroUsize::MIN,
            ..Config::default()
        },
        CancellationToken::new(),
    );
    let key = |workspace: &str| Key {
        workspace: PathBuf::from(workspace),
        language: lsp::Language::Go,
        full: false,
    };
    let (sender, _receiver) = mpsc::channel(8);
    drop(pool.servers.insert(
        key("/a/old"),
        super::Slot {
            sender,
            root: PathBuf::from("/a"),
            retire: CancellationToken::new(),
            retiring: None,
            used: 0,
        },
    ));
    let admission = |workspace: &str, root: &str| {
        let (reply, response) = oneshot::channel();
        let admission = super::Admission {
            key: key(workspace),
            root: PathBuf::from(root),
            work: Work {
                query: in_language(&PathBuf::from(workspace), lsp::Language::Go),
                reply,
            },
        };
        (admission, response)
    };
    let mut waiting = Vec::new();
    for index in 0..super::MAX_PARKED {
        let (parked, response) = admission(&format!("/c/{index}"), "/c");
        pool.parked.push_back(parked);
        waiting.push(response);
    }
    let (refused, mut response) = admission("/b/new", "/b");
    pool.admit(refused);
    assert!(pool.servers[&key("/a/old")].retiring.is_none());
    assert!(matches!(
        response.try_recv(),
        Ok(Err(LspError::Unavailable))
    ));
}
