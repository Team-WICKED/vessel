//! Visibility of sessions while they are created, and when the store
//! cannot record one.

use std::sync::Arc;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_harness::Lookup;
use vessel_harness::fakes::{Role, install};
use vessel_proto::{
    HarnessKind, SessionId, SessionInput, SessionMode, SessionSpec, SessionStatus, TerminalSize,
};
use vessel_session::{
    DEFAULT_DURABLE_CAPACITY, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RING_CAPACITY, LaunchOptions,
    RuntimeConfig, SessionRuntime,
};
use vessel_store::{DataDir, Store, StoreConfig};
use vessel_testkit::Fake;

use super::{Found, Sessions};
use crate::error::ServiceError;

struct Fixture {
    data: TempDir,
    _bin: TempDir,
    runtime: Arc<SessionRuntime>,
    store: Store,
    sessions: Arc<Sessions>,
    /// Session creations, persistence and hand-overs.
    tasks: TaskTracker,
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
                harness_env: vessel_harness::HarnessEnv::new(data.path().to_owned()),
                ring_capacity: DEFAULT_RING_CAPACITY,
                durable_capacity: DEFAULT_DURABLE_CAPACITY,
                agent_handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            },
            CancellationToken::new(),
        ));
        let store = Store::open(&DataDir::new(data.path()), StoreConfig::default())
            .await
            .unwrap()
            .store;
        let tasks = TaskTracker::new();
        let sessions = Arc::new(Sessions::new(
            Arc::clone(&runtime),
            store.clone(),
            tasks.clone(),
            Arc::new(crate::agents::Agents::new(vessel_proto::NodeId::random())),
        ));
        Self {
            data,
            _bin: bin,
            runtime,
            store,
            sessions,
            tasks,
        }
    }

    fn spec(&self) -> SessionSpec {
        SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        )
        .with_cwd(self.data.path().to_path_buf())
    }

    fn listed(&self) -> Vec<SessionId> {
        self.runtime
            .list()
            .into_iter()
            .map(|info| info.id)
            .collect()
    }

    /// Wait until every session task (creation, persistence, hand-over)
    /// has finished.
    async fn settle(&self) {
        let _first_close = self.tasks.close();
        self.tasks.wait().await;
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

/// A session the store cannot record is killed and disappears: it is not
/// listed, and once it has ended the runtime no longer has it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_the_store_cannot_record_is_killed_and_forgotten() {
    let fixture = Fixture::new().await;
    // Writes fail from here on; reads keep working.
    fixture.store.close().await;
    let refused = fixture
        .sessions
        .create(fixture.spec(), LaunchOptions::default())
        .await;
    assert!(
        matches!(refused, Err(ServiceError::Store(_))),
        "{refused:?}"
    );
    assert!(fixture.sessions.list().await.unwrap().is_empty());

    // It ends (it was killed) and is removed from the runtime.
    fixture.settle().await;
    assert!(fixture.listed().is_empty());
    assert!(fixture.sessions.list().await.unwrap().is_empty());
    fixture.stop().await;
}

/// Once a session's whole log is committed it is handed over to the
/// store: the runtime frees it, and lookups, listing and input refusals
/// come from the store, with the final status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_session_is_handed_over_to_the_store() {
    let fixture = Fixture::new().await;
    let id = fixture
        .sessions
        .create(fixture.spec(), LaunchOptions::default())
        .await
        .unwrap()
        .id;
    let Found::Live { handle, .. } = fixture.sessions.find(id).await.unwrap() else {
        panic!("a new session is live");
    };
    handle.kill();
    fixture.settle().await;

    assert!(fixture.runtime.get(id).is_none());
    let Found::Stored(stored) = fixture.sessions.find(id).await.unwrap() else {
        panic!("a finished session is served from the store");
    };
    assert!(
        matches!(stored.status, SessionStatus::Exited { .. }),
        "{stored:?}"
    );
    assert_eq!(stored.next_seq, handle.info().next_seq);
    let listed = fixture.sessions.list().await.unwrap();
    assert_eq!(listed.iter().map(|info| info.id).collect::<Vec<_>>(), [id]);
    assert!(matches!(
        fixture.sessions.send(id, SessionInput::CancelTurn).await,
        Err(ServiceError::Ended(ended)) if ended == id
    ));
    fixture.stop().await;
}
