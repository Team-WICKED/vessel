//! The nodes' connections: one per node, shared by every browser tab.
//!
//! [Decision Log]
//! - Purpose: a browser tab lists every node, opens sessions on several,
//!   and may be one of several tabs. Each tab dialling its own
//!   connections would start an `ssh` process per tab and per stream for
//!   an SSH node, and show each tab a different idea of whether a node is
//!   up.
//! - Choice: one supervisor task per node, started when something first
//!   asks for the node. It dials through the [`NodeSource`]'s connector,
//!   publishes the connection, and on a loss dials again with the CLI's
//!   backoff ([`Backoff`]), reporting each state ([`Link`]) to every tab.
//!   Calls take the current connection; a node that is down fails them at
//!   once rather than queueing them (the UI shows the node's state).
//!   Streams resume through [`Shared`], a `Connector` that waits for the
//!   supervisor's next connection instead of dialling one of its own, so
//!   `follow_resuming` keeps its gap-free resume and the node keeps one
//!   connection.
//! - A node that fails in a way waiting does not fix (no such node, a
//!   wrong token, a changed identity) stops its supervisor; the next call,
//!   follow or `retry` starts it again.
//! - Costs: a connection stays open while `vessel ui` runs, even when no
//!   tab looks at the node any more; the UI is a short-lived local
//!   process, and the fleet overview looks at every node anyway.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::FutureExt as _;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vessel_client::{Backoff, Connector, NodeClient};
use vessel_proto::ClientInfo;

use crate::source::NodeSource;
use crate::wire::{Failure, NodeKey, NodeStatus};

/// The most nodes the bridge keeps connections for; a browser naming more
/// is refused (each entry is a task and, for SSH, a process).
const MAX_NODES: usize = 256;

/// How many node changes a slow tab may fall behind before it is sent
/// every node's state again.
const CHANGES: usize = 256;

/// Where a node's connection is, with the connection itself.
#[derive(Debug, Clone)]
pub(crate) enum Link {
    Idle,
    Connecting,
    Up(NodeClient),
    Down {
        message: String,
        attempt: u32,
        retry_in: Duration,
    },
    Failed {
        message: String,
    },
}

impl Link {
    pub(crate) fn status(&self) -> NodeStatus {
        match self {
            Self::Idle => NodeStatus::Idle,
            Self::Connecting => NodeStatus::Connecting,
            Self::Up(client) => NodeStatus::Connected {
                node: client.node().clone(),
                protocol: client.protocol(),
            },
            Self::Down {
                message,
                attempt,
                retry_in,
            } => NodeStatus::Reconnecting {
                message: message.clone(),
                attempt: *attempt,
                retry_in_ms: u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX),
            },
            Self::Failed { message } => NodeStatus::Failed {
                message: message.clone(),
            },
        }
    }
}

/// One node: its link, and its supervisor if one runs.
#[derive(Debug)]
pub(crate) struct Entry {
    link: watch::Sender<Link>,
    /// Wakes a supervisor waiting to dial again.
    retry: Notify,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

/// Every node's connection.
pub(crate) struct Registry<S> {
    source: Arc<S>,
    client: ClientInfo,
    backoff: Backoff,
    entries: Mutex<HashMap<NodeKey, Arc<Entry>>>,
    /// Which node changed; tabs read its state from the entry.
    changes: broadcast::Sender<NodeKey>,
    shutdown: CancellationToken,
}

impl<S> std::fmt::Debug for Registry<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl<S: NodeSource> Registry<S> {
    pub(crate) fn new(source: S, backoff: Backoff, shutdown: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            source: Arc::new(source),
            client: ClientInfo {
                name: "vessel-ui".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            backoff,
            entries: Mutex::new(HashMap::new()),
            changes: broadcast::channel(CHANGES).0,
            shutdown,
        })
    }

    pub(crate) fn source(&self) -> &S {
        &self.source
    }

    pub(crate) const fn client_info(&self) -> &ClientInfo {
        &self.client
    }

    /// Node changes from now on.
    pub(crate) fn changes(&self) -> broadcast::Receiver<NodeKey> {
        self.changes.subscribe()
    }

    /// Every node asked for so far, with its state.
    pub(crate) fn statuses(&self) -> Vec<(NodeKey, NodeStatus)> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut statuses: Vec<_> = entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.link.borrow().status()))
            .collect();
        statuses.sort_by(|one, two| one.0.cmp(&two.0));
        statuses
    }

    /// One node's state.
    pub(crate) fn status(&self, key: &NodeKey) -> Option<NodeStatus> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.get(key).map(|entry| entry.link.borrow().status())
    }

    /// `key`'s entry, its supervisor running (started again if it
    /// stopped).
    pub(crate) fn ensure(self: &Arc<Self>, key: &NodeKey) -> Result<Arc<Entry>, Failure> {
        let entry = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = entries.get(key) {
                Arc::clone(entry)
            } else {
                if entries.len() >= MAX_NODES {
                    return Err(Failure::Refused {
                        message: format!("the UI keeps at most {MAX_NODES} nodes"),
                    });
                }
                let entry = Arc::new(Entry {
                    link: watch::Sender::new(Link::Idle),
                    retry: Notify::new(),
                    supervisor: Mutex::new(None),
                });
                let _new = entries.insert(key.clone(), Arc::clone(&entry));
                entry
            }
        };
        let mut supervisor = entry
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let running = supervisor.as_ref().is_some_and(|task| !task.is_finished());
        if !running && !self.shutdown.is_cancelled() {
            // Connecting before the task runs, so nothing waiting on the
            // link sees the old failure and gives up.
            self.set(key, &entry, Link::Connecting);
            let task = tokio::spawn(Arc::clone(self).supervise(key.clone(), Arc::clone(&entry)));
            *supervisor = Some(task);
        }
        drop(supervisor);
        Ok(entry)
    }

    /// Dial `key` again now.
    pub(crate) fn retry(self: &Arc<Self>, key: &NodeKey) -> Result<(), Failure> {
        let entry = self.ensure(key)?;
        entry.retry.notify_one();
        Ok(())
    }

    /// `key`'s connection, waiting while it is being dialled; a node that
    /// is down fails at once.
    pub(crate) async fn client(self: &Arc<Self>, key: &NodeKey) -> Result<NodeClient, Failure> {
        let entry = self.ensure(key)?;
        let mut link = entry.link.subscribe();
        loop {
            {
                let current = link.borrow_and_update();
                match &*current {
                    Link::Up(client) => return Ok(client.clone()),
                    Link::Down { message, .. } | Link::Failed { message } => {
                        return Err(Failure::Unreachable {
                            message: message.clone(),
                        });
                    }
                    Link::Idle | Link::Connecting => {}
                }
            }
            if link.changed().await.is_err() {
                return Err(stopping());
            }
        }
    }

    fn set(&self, key: &NodeKey, entry: &Entry, link: Link) {
        let _previous = entry.link.send_replace(link);
        // No tab listening is fine.
        let _listeners = self.changes.send(key.clone());
    }

    /// Keep `key` connected until the UI stops or the node fails for good.
    async fn supervise(self: Arc<Self>, key: NodeKey, entry: Arc<Entry>) {
        let shutdown = self.shutdown.clone();
        tokio::select! {
            () = self.keep_connected(&key, &entry) => {}
            () = shutdown.cancelled() => {}
        }
    }

    async fn keep_connected(&self, key: &NodeKey, entry: &Entry) {
        let connector = match self.source.connector(key).await {
            Ok(connector) => connector,
            Err(error) => {
                let message = chain(&error);
                self.set(key, entry, Link::Failed { message });
                return;
            }
        };
        let mut attempt: u32 = 0;
        loop {
            match connector.connect(self.client.clone()).await {
                Ok(client) => {
                    attempt = 0;
                    self.source.seen(key, client.node().id).await;
                    tracing::debug!(node = %key, "connected");
                    self.set(key, entry, Link::Up(client.clone()));
                    client.closed().await;
                    drop(client);
                    tracing::info!(node = %key, "lost the node's connection");
                    attempt = attempt.saturating_add(1);
                    let retry_in = self.backoff.delay(attempt);
                    let message = "the connection closed".to_owned();
                    self.wait(key, entry, message, attempt, retry_in).await;
                }
                Err(error) if connector.is_transient(&error) => {
                    attempt = attempt.saturating_add(1);
                    let retry_in = self.backoff.delay(attempt);
                    self.wait(key, entry, chain(&error), attempt, retry_in)
                        .await;
                }
                Err(error) => {
                    let message = chain(&error);
                    tracing::info!(node = %key, %message, "the node cannot be reached");
                    self.set(key, entry, Link::Failed { message });
                    return;
                }
            }
        }
    }

    /// Down: say so, then wait `retry_in` (or a retry) before dialling.
    async fn wait(
        &self,
        key: &NodeKey,
        entry: &Entry,
        message: String,
        attempt: u32,
        retry_in: Duration,
    ) {
        self.set(
            key,
            entry,
            Link::Down {
                message,
                attempt,
                retry_in,
            },
        );
        tokio::select! {
            () = tokio::time::sleep(retry_in) => {}
            () = entry.retry.notified() => {}
        }
    }
}

/// Opens "connections" to a node by waiting for its supervisor's: the
/// connector streams resume through ([`vessel_client::follow_resuming`]).
#[derive(Debug)]
pub(crate) struct Shared {
    entry: Arc<Entry>,
}

impl Shared {
    pub(crate) const fn new(entry: Arc<Entry>) -> Self {
        Self { entry }
    }
}

/// Why [`Shared`] has no connection to give.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SharedError {
    /// The node failed for good; follow again to retry.
    #[error("{0}")]
    Failed(String),
    /// The UI is stopping.
    #[error("the UI is stopping")]
    Stopping,
}

impl Connector for Shared {
    type Error = SharedError;

    async fn connect(&self, _client: ClientInfo) -> Result<NodeClient, SharedError> {
        let mut link = self.entry.link.subscribe();
        loop {
            {
                let current = link.borrow_and_update();
                match &*current {
                    // A connection that closed and that the supervisor has
                    // not replaced yet is no use.
                    Link::Up(client) if client.closed().now_or_never().is_none() => {
                        return Ok(client.clone());
                    }
                    Link::Failed { message } => return Err(SharedError::Failed(message.clone())),
                    Link::Up(_) | Link::Idle | Link::Connecting | Link::Down { .. } => {}
                }
            }
            if link.changed().await.is_err() {
                return Err(SharedError::Stopping);
            }
        }
    }

    fn is_transient(&self, error: &SharedError) -> bool {
        match error {
            SharedError::Failed(_) | SharedError::Stopping => false,
        }
    }
}

fn stopping() -> Failure {
    Failure::Unreachable {
        message: "the UI is stopping".to_owned(),
    }
}

/// `error` and its causes, one line.
pub(crate) fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}
