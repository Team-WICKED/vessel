//! `Pinned`: only the pinned node is accepted, also on a reconnect.

use super::memory::{FAST, MemoryConnector, Seen, drain};
use super::*;
use crate::{Connector, Pinned, PinnedError, StreamError, follow_resuming};

#[tokio::test]
async fn a_pinned_connector_accepts_only_its_node() {
    let node = Arc::new(FakeNode::new(0));
    let id = *node.id.lock().unwrap();
    let connector = MemoryConnector::new(Arc::clone(&node));

    let known = Pinned::new(Arc::clone(&connector), Some(id));
    assert_eq!(known.connect(client_info()).await.unwrap().node().id, id);
    assert_eq!(known.pinned(), Some(id));

    let other = NodeId::random();
    let wrong = Pinned::new(Arc::clone(&connector), Some(other));
    let error = wrong.connect(client_info()).await.unwrap_err();
    assert!(
        matches!(error, PinnedError::Changed { expected, found } if expected == other && found == id),
        "{error:?}"
    );
    assert!(!wrong.is_transient(&error));

    let fresh = Pinned::new(Arc::clone(&connector), None);
    assert_eq!(fresh.pinned(), None);
    let _client = fresh.connect(client_info()).await.unwrap();
    assert_eq!(fresh.pinned(), Some(id), "the first node seen is pinned");

    connector.fail_next(1, false);
    let error = fresh.connect(client_info()).await.unwrap_err();
    assert!(fresh.is_transient(&error), "the inner connector decides");
}

/// A different node answering a reconnect (a reinstalled machine, a reused
/// address) ends the stream instead of resuming against it.
#[tokio::test]
async fn a_different_node_after_a_reconnect_is_refused() {
    let node = Arc::new(FakeNode::new(0));
    let connector = MemoryConnector::new(Arc::clone(&node));
    let mut stream = follow_resuming(
        Pinned::new(Arc::clone(&connector), None),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);
    let seen = drain(&mut stream, async |records| {
        if records == 1 {
            *node.id.lock().unwrap() = NodeId::random();
            connector.drop_all().await;
        }
    })
    .await;
    let Some(Seen::Notice(StreamError::Unreachable { reason })) = seen.last() else {
        panic!("{seen:?}");
    };
    assert!(reason.contains("a different node answered"), "{reason}");
    assert_eq!(connector.connects(), 2, "not retried");
}
