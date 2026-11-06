//! `ResumingStream`: losing the connection, dialling again with backoff,
//! resuming at the cursor; and what is never retried.

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use vessel_rpc::CallError;

use super::*;
use crate::{Backoff, Connector, ResumingStream, StreamError, follow_resuming};

/// Connects to one fake node over the in-memory transport, and can cut
/// every connection it opened ("drops on command").
struct MemoryConnector {
    node: Arc<FakeNode>,
    serving: Mutex<Vec<JoinHandle<()>>>,
    connects: AtomicUsize,
    /// Dials still to fail, and whether they fail for good.
    failures: AtomicUsize,
    refuse: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
enum DialError {
    #[error("the fake network is down")]
    Down,
    #[error("the fake node refused")]
    Refused,
    #[error("the handshake failed")]
    Handshake(#[source] vessel_rpc::ConnectError<Disconnected>),
}

impl MemoryConnector {
    fn new(node: Arc<FakeNode>) -> Arc<Self> {
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
    fn fail_next(&self, count: usize, refuse: bool) {
        self.refuse.store(refuse, Ordering::Relaxed);
        self.failures.store(count, Ordering::Relaxed);
    }

    /// Cut every open connection, and wait until the node's ends are gone.
    async fn drop_all(&self) {
        let serving = std::mem::take(&mut *self.serving.lock().unwrap());
        for task in serving {
            task.abort();
            let _cancelled = task.await;
        }
    }

    fn connects(&self) -> usize {
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
const FAST: Backoff = Backoff {
    first: Duration::from_millis(1),
    max: Duration::from_millis(1),
};

/// What a resuming stream yielded, in order.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Record(u64),
    Notice(StreamError),
}

/// Drain `stream`, calling `after` with the count of records seen after
/// each record.
async fn drain<C: Connector + 'static>(
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

fn records(seen: &[Seen]) -> Vec<u64> {
    seen.iter()
        .filter_map(|item| match item {
            Seen::Record(seq) => Some(*seq),
            Seen::Notice(_) => None,
        })
        .collect()
}

fn notices(seen: &[Seen]) -> Vec<&StreamError> {
    seen.iter()
        .filter_map(|item| match item {
            Seen::Record(_) => None,
            Seen::Notice(error) => Some(error),
        })
        .collect()
}

#[tokio::test]
async fn a_dropped_connection_resumes_at_the_cursor_without_gaps() {
    let node = Arc::new(FakeNode::new(0));
    let connector = MemoryConnector::new(Arc::clone(&node));
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);
    let clients = stream.client();
    assert!(clients.borrow().is_none(), "nothing is dialled up front");

    let seen = drain(&mut stream, async |records| {
        if records == 3 {
            assert!(clients.borrow().is_some());
            connector.drop_all().await;
        }
    })
    .await;

    assert_eq!(records(&seen), (0..LOG_LEN).collect::<Vec<_>>(), "{seen:?}");
    let notices = notices(&seen);
    let [
        StreamError::Reconnecting {
            attempt: 1, reason, ..
        },
        StreamError::Resumed { at },
    ] = notices.as_slice()
    else {
        panic!("{seen:?}");
    };
    assert_eq!(reason, "the connection closed");
    // The record right after the resume is the one the cursor named.
    let resumed = seen
        .iter()
        .position(|item| matches!(item, Seen::Notice(StreamError::Resumed { .. })))
        .unwrap();
    assert_eq!(seen[resumed.saturating_add(1)], Seen::Record(at.get()));
    assert_eq!(connector.connects(), 2);
    assert!(
        clients.borrow().is_some(),
        "the new connection is published"
    );
    assert_eq!(stream.cursor(), EventSeq::new(LOG_LEN));
}

#[tokio::test]
async fn an_open_connection_is_reused_and_redialled_only_after_a_loss() {
    let node = Arc::new(FakeNode::new(0));
    let connector = MemoryConnector::new(Arc::clone(&node));
    let (client, serving) = open(Arc::clone(&node)).await.unwrap();
    connector.serving.lock().unwrap().push(serving);
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::new(2),
    )
    .reusing(client)
    .with_backoff(FAST);

    let seen = drain(&mut stream, async |records| {
        if records == 1 {
            assert_eq!(connector.connects(), 0);
            connector.drop_all().await;
        }
    })
    .await;
    assert_eq!(records(&seen), (2..LOG_LEN).collect::<Vec<_>>(), "{seen:?}");
    assert_eq!(notices(&seen).len(), 2, "{seen:?}");
    assert_eq!(connector.connects(), 1);
}

#[tokio::test(start_paused = true)]
async fn failed_dials_back_off_until_the_node_answers() {
    let connector = MemoryConnector::new(Arc::new(FakeNode::new(0)));
    connector.fail_next(3, false);
    let started = Instant::now();
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    );

    let mut waited = Duration::ZERO;
    let mut previous = Duration::ZERO;
    for expected in 1..=3 {
        let Some(Err(StreamError::Reconnecting {
            attempt,
            retry_in,
            reason,
        })) = stream.next().await
        else {
            panic!("expected attempt {expected}");
        };
        assert_eq!(attempt, expected);
        assert_eq!(reason, "the fake network is down");
        assert!(retry_in > previous, "{retry_in:?} after {previous:?}");
        assert!(retry_in >= Backoff::default().first);
        previous = retry_in;
        waited = waited.saturating_add(retry_in);
    }
    let resumed = stream.next().await;
    assert_eq!(
        resumed,
        Some(Err(StreamError::Resumed { at: EventSeq::ZERO }))
    );
    assert!(started.elapsed() >= waited);
    assert_eq!(connector.connects(), 4);

    let seen = drain(&mut stream, async |_| {}).await;
    assert_eq!(records(&seen), (0..LOG_LEN).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_refused_dial_ends_the_stream_with_the_reason() {
    let connector = MemoryConnector::new(Arc::new(FakeNode::new(0)));
    connector.fail_next(1, true);
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(
        error,
        StreamError::Unreachable {
            reason: "the fake node refused".to_owned()
        }
    );
    assert!(error.is_fatal());
    assert!(stream.next().await.is_none());
    assert_eq!(connector.connects(), 1, "a refusal is not retried");
}

#[tokio::test]
async fn a_session_gone_after_a_reconnect_ends_the_stream() {
    let node = Arc::new(FakeNode::new(0));
    let connector = MemoryConnector::new(Arc::clone(&node));
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);
    let seen = drain(&mut stream, async |records| {
        if records == 1 {
            node.forgotten.store(true, Ordering::Relaxed);
            connector.drop_all().await;
        }
    })
    .await;
    let Some(Seen::Notice(StreamError::Call(error))) = seen.last() else {
        panic!("{seen:?}");
    };
    assert_eq!(error.code(), Some(ErrorCode::NotFound));
    assert!(stream.next().await.is_none());
}

/// A call in flight when the connection drops fails with `Disconnected`
/// (its outcome is unknown) and is not sent again on the new connection.
#[tokio::test]
async fn calls_in_flight_fail_and_are_never_repeated() {
    let node = Arc::new(FakeNode::new(0));
    let connector = MemoryConnector::new(Arc::clone(&node));
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.seq, EventSeq::ZERO);

    let client = stream.client().borrow().clone().unwrap();
    let kill = tokio::spawn(async move { client.kill(session_id()).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while node.kills.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    connector.drop_all().await;
    assert_eq!(kill.await.unwrap(), Err(CallError::Disconnected));

    let seen = drain(&mut stream, async |_| {}).await;
    assert_eq!(records(&seen), (1..LOG_LEN).collect::<Vec<_>>(), "{seen:?}");
    assert_eq!(connector.connects(), 2);
    assert_eq!(node.kills.load(Ordering::Relaxed), 1);
}

/// `next` dropped while waiting (a `select!` that picked another branch)
/// keeps its place: the wait is not restarted and nothing extra is dialled.
#[tokio::test(start_paused = true)]
async fn a_cancelled_next_keeps_its_place() {
    let connector = MemoryConnector::new(Arc::new(FakeNode::new(0)));
    connector.fail_next(1, false);
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    );
    assert!(matches!(
        stream.next().await,
        Some(Err(StreamError::Reconnecting { attempt: 1, .. }))
    ));
    // Cancel during the backoff wait, many times over.
    for _ in 0..5 {
        let cancelled = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(cancelled.is_err());
    }
    assert_eq!(connector.connects(), 1);
    assert_eq!(
        stream.next().await,
        Some(Err(StreamError::Resumed { at: EventSeq::ZERO }))
    );
    assert_eq!(connector.connects(), 2);
}
