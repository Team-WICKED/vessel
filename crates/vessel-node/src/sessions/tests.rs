//! Visibility of sessions while they are created, and when the store
//! cannot record one.

use std::sync::Arc;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_harness::Lookup;
use vessel_harness::fakes::{Role, install};
use vessel_proto::{HarnessKind, SessionId, SessionMode, SessionSpec, TerminalSize};
use vessel_session::{
    DEFAULT_DURABLE_CAPACITY, DEFAULT_RING_CAPACITY, RuntimeConfig, SessionRuntime,
};
use vessel_store::{DataDir, Store, StoreConfig};
use vessel_testkit::Fake;

use super::Sessions;
use crate::error::ServiceError;

struct Fixture {
    data: TempDir,
    _bin: TempDir,
    runtime: Arc<SessionRuntime>,
    store: Store,
    sessions: Arc<Sessions>,
}

impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        let runtime = Arc::new(SessionRuntime::new(
            RuntimeConfig {
                harness_lookup: Lookup::new([bin.path().to_path_buf()]),
                ring_capacity: DEFAULT_RING_CAPACITY,
                durable_capacity: DEFAULT_DURABLE_CAPACITY,
            },
            CancellationToken::new(),
        ));
        let store = Store::open(&DataDir::new(data.path()), StoreConfig::default())
            .await
            .unwrap()
            .store;
        let sessions = Arc::new(Sessions::new(
            Arc::clone(&runtime),
            store.clone(),
            TaskTracker::new(),
        ));
        Self {
            data,
            _bin: bin,
            runtime,
            store,
            sessions,
        }
    }

    fn spec(&self) -> SessionSpec {
        SessionSpec {
            harness: HarnessKind::ClaudeCode,
            cwd: Some(self.data.path().to_path_buf()),
            mode: SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        }
    }

    fn listed(&self) -> Vec<SessionId> {
        self.runtime
            .list()
            .into_iter()
            .map(|info| info.id)
            .collect()
    }

    async fn stop(self) {
        self.runtime.shutdown().await;
        self.store.close().await;
    }
}

/// The runtime knows a session before the node has its entry (the window
/// between `runtime.create` and the durability table). Such a session is
/// neither listed nor found, so nothing can see it half created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_session_without_an_entry_is_neither_listed_nor_found() {
    let fixture = Fixture::new().await;
    let created = fixture.runtime.create(fixture.spec()).await.unwrap();
    let id = created.handle.id();
    assert_eq!(fixture.listed(), [id]);

    let listed = fixture.sessions.list().await.unwrap();
    assert!(listed.iter().all(|info| info.id != id), "{listed:?}");
    assert!(matches!(
        fixture.sessions.find(id).await,
        Err(ServiceError::NotFound(missing)) if missing == id
    ));
    // Let it end without a persist task.
    created.handle.kill();
    drop(created.durable);
    fixture.stop().await;
}

/// A session the store cannot record is killed, fails anyone waiting for
/// its log, and disappears: it is not listed and cannot be found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_the_store_cannot_record_is_killed_and_forgotten() {
    let fixture = Fixture::new().await;
    // Writes fail from here on; reads keep working.
    fixture.store.close().await;
    let refused = fixture.sessions.create(fixture.spec()).await;
    assert!(
        matches!(refused, Err(ServiceError::Store(_))),
        "{refused:?}"
    );

    let [id] = fixture.listed()[..] else {
        panic!("expected exactly one runtime session");
    };
    let handle = fixture.runtime.get(id).unwrap();
    let mut info = handle.watch_info();
    let ended = info.wait_for(|info| !info.status.is_live()).await;
    // The watch closes with the final status; either way it has ended.
    drop(ended);
    assert!(!handle.info().status.is_live());

    assert!(fixture.sessions.list().await.unwrap().is_empty());
    assert!(matches!(
        fixture.sessions.find(id).await,
        Err(ServiceError::NotFound(_))
    ));
    fixture.stop().await;
}
