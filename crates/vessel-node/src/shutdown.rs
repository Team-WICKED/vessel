//! Stopping a node without losing anything.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_session::SessionRuntime;
use vessel_store::Store;

/// How long subscription forwarders get to queue their last frames (the
/// sessions' final records and `SubscriptionEnded`) before connections
/// close. They normally need far less; a forwarder stuck behind a client
/// that stopped reading is not waited for longer.
const FORWARDER_GRACE: Duration = Duration::from_secs(2);

/// Everything that must be stopped, in [`Teardown::run`]'s order.
pub(crate) struct Teardown {
    pub(crate) runtime: Arc<SessionRuntime>,
    /// Session creations and persist tasks.
    pub(crate) tasks: TaskTracker,
    /// Subscription forwarders.
    pub(crate) forwarders: TaskTracker,
    /// Cancelling closes every RPC connection.
    pub(crate) closing: CancellationToken,
    pub(crate) connections: TaskTracker,
    pub(crate) store: Store,
}

impl Teardown {
    /// Called once the listener stopped accepting.
    ///
    /// 1. End every session. Each publishes a final `Interrupted` status,
    ///    which live subscribers still receive.
    /// 2. Wait until every record is committed.
    /// 3. Give subscription forwarders a moment to queue the final records
    ///    and `SubscriptionEnded` (bounded by [`FORWARDER_GRACE`]).
    /// 4. Close connections; each flushes what it already queued (bounded by
    ///    the RPC close timeout).
    /// 5. Close the store, folding its WAL into the database file.
    pub(crate) async fn run(self) {
        tracing::info!("stopping sessions");
        self.runtime.shutdown().await;
        let _first_close = self.tasks.close();
        self.tasks.wait().await;
        tracing::debug!("session logs committed");
        let _first_close = self.forwarders.close();
        if tokio::time::timeout(FORWARDER_GRACE, self.forwarders.wait())
            .await
            .is_err()
        {
            tracing::debug!("some subscribers did not take their last records in time");
        }
        self.closing.cancel();
        let _first_close = self.connections.close();
        self.connections.wait().await;
        self.store.close().await;
        tracing::info!("node stopped");
    }
}

/// Resolves when the process is asked to stop: `Ctrl-C`, or `SIGTERM` on
/// Unix (what service managers send).
pub async fn signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    () = interrupt => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM");
                interrupt.await;
            }
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}
