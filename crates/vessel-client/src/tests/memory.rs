//! An in-memory [`Connector`] that can drop every connection on command,
//! and helpers for reading a resuming stream, shared by the resume and
//! pinning tests.

use std::time::Duration;

use tokio::task::JoinHandle;

use super::*;
use crate::{Backoff, Connector, ResumingStream, StreamError};

/// Connects to one fake node over the in-memory transport, and can cut
/// every connection it opened ("drops on command").
pub(super) struct MemoryConnector {
    node: Arc<FakeNode>,
    pub(super) serving: Mutex<Vec<JoinHandle<()>>>,
    connects: AtomicUsize,
    /// Dials still to fail, and whether they fail for good.
    failures: AtomicUsize,
    refuse: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum DialError {
    #[error("the fake network is down")]
    Down,
    #[error("the fake node refused")]
    Refused,
    #[error("the handshake failed")]
    Handshake(#[source] vessel_rpc::ConnectError<Disconnected>),
}

impl MemoryConnector {
    pub(super) fn new(node: Arc<FakeNode>) -> Arc<Self> {
        Arc::new(Self {
            node,
            serving: Mutex::new(Vec::new()),
            connects: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            refuse: AtomicBool::new(false),
        })
    }

    /// Fail the next `count` dials: for good if `refuse`, else as a network
    /// that is down.
    pub(super) fn fail_next(&self, count: usize, refuse: bool) {
        self.refuse.store(refuse, Ordering::Relaxed);
        self.failures.store(count, Ordering::Relaxed);
    }

    /// Cut every open connection, and wait until the node's ends are gone.
    pub(super) async fn drop_all(&self) {
        let serving = std::mem::take(&mut *self.serving.lock().unwrap());
        for task in serving {
            task.abort();
            let _cancelled = task.await;
        }
    }

    pub(super) fn connects(&self) -> usize {
        self.connects.load(Ordering::Relaxed)
    }
}

impl Connector for MemoryConnector {
    type Error = DialError;

    async fn connect(&self, _client: ClientInfo) -> Result<NodeClient, DialError> {
        let _dialled = self.connects.fetch_add(1, Ordering::Relaxed);
        let failing = self
            .failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Err(if self.refuse.load(Ordering::Relaxed) {
                DialError::Refused
            } else {
                DialError::Down
            });
        }
        let (client, serving) = open(Arc::clone(&self.node))
            .await
            .map_err(DialError::Handshake)?;
        self.serving.lock().unwrap().push(serving);
        Ok(client)
    }

    fn is_transient(&self, error: &DialError) -> bool {
        !matches!(error, DialError::Refused)
    }
}

/// Reconnect at once; these tests are about order, not timing.
pub(super) const FAST: Backoff = Backoff {
    first: Duration::from_millis(1),
    max: Duration::from_millis(1),
};

/// What a resuming stream yielded, in order.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Seen {
    Record(u64),
    Notice(StreamError),
}

/// Drain `stream`, calling `after` with the count of records seen after
/// each record.
pub(super) async fn drain<C: Connector + 'static>(
    stream: &mut ResumingStream<C>,
    mut after: impl AsyncFnMut(usize),
) -> Vec<Seen> {
    let mut seen = Vec::new();
    let mut records = 0_usize;
    while let Some(item) = stream.next().await {
        match item {
            Ok(record) => {
                seen.push(Seen::Record(record.seq.get()));
                records = records.saturating_add(1);
                after(records).await;
            }
            Err(error) => seen.push(Seen::Notice(error)),
        }
    }
    seen
}

pub(super) fn records(seen: &[Seen]) -> Vec<u64> {
    seen.iter()
        .filter_map(|item| match item {
            Seen::Record(seq) => Some(*seq),
            Seen::Notice(_) => None,
        })
        .collect()
}

pub(super) fn notices(seen: &[Seen]) -> Vec<&StreamError> {
    seen.iter()
        .filter_map(|item| match item {
            Seen::Record(_) => None,
            Seen::Notice(error) => Some(error),
        })
        .collect()
}
