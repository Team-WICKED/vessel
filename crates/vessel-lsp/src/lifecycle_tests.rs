//! Deterministic timer checks: advance Tokio time, then await lifecycle events.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{Reply, lsp};
use vessel_testkit::{Fake, within};

use super::{Config, Key, Manager, Work, server};
use crate::registry::Override;
use crate::{LspError, Query};

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
fn config(mode: &str) -> Config {
    let mut config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
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
        let dir = root();
        let root = dir.path().canonicalize().unwrap();
        let config = Config {
            idle_timeout: Duration::from_secs(5),
            ..config("normal")
        };
        let (sender, receiver) = mpsc::channel(8);
        let task = tokio::spawn(server(
            Key {
                root: root.clone(),
                language: lsp::Language::Rust,
                full: false,
            },
            config,
            CancellationToken::new(),
            receiver,
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
        let dir = root();
        let root = dir.path().canonicalize().unwrap();
        let stop = CancellationToken::new();
        let manager = Manager::start(config("crash"), stop.child_token());
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
                .query(root.clone(), false, Query::Hover(lsp::GetHover(at)))
                .await
                .is_err()
        );
        assert!(matches!(
            manager
                .query(root.clone(), false, query(root.clone()))
                .await,
            Err(LspError::Unavailable)
        ));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::time::resume();
        assert!(
            manager
                .query(root.clone(), false, query(root))
                .await
                .is_ok()
        );
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_global_server_bound_is_enforced() {
    within(Duration::from_secs(45), async {
        let first = root();
        let second = root();
        let first = first.path().canonicalize().unwrap();
        let second = second.path().canonicalize().unwrap();
        let config = Config {
            max_servers: NonZeroUsize::MIN,
            ..config("normal")
        };
        let stop = CancellationToken::new();
        let manager = Manager::start(config, stop.child_token());
        assert!(
            manager
                .query(first.clone(), false, query(first))
                .await
                .is_ok()
        );
        assert!(matches!(
            manager.query(second.clone(), false, query(second)).await,
            Err(LspError::Unavailable)
        ));
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}

/// Review finding: open documents were never evicted, so 64 distinct files
/// locked every new file out for as long as queries kept the server alive.
#[tokio::test]
async fn the_least_recently_used_document_makes_room_for_a_new_one() {
    within(Duration::from_secs(90), async {
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
        let manager = Manager::start(config("normal"), stop.child_token());
        for index in 0..=crate::process::MAX_DOCUMENTS {
            let outcome = manager.query(root.clone(), false, file(index)).await;
            assert!(outcome.is_ok(), "file {index}: {outcome:?}");
        }
        // The evicted first file can be opened again, evicting another.
        assert!(manager.query(root.clone(), false, file(0)).await.is_ok());
        stop.cancel();
        manager.stopped().await;
    })
    .await
    .unwrap();
}
