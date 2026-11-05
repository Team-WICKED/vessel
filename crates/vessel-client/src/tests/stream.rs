//! `SessionStream`: lag, catch-up, resubscription and pruned ranges.

use super::*;
use crate::StreamError;

/// Drain a stream, keeping errors in place.
async fn drain(stream: &mut crate::SessionStream) -> Vec<Result<u64, StreamError>> {
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item.map(|record| record.seq.get()));
    }
    items
}

#[tokio::test]
async fn a_lagging_follow_catches_up_without_gaps_or_repeats() {
    let node = Arc::new(FakeNode::new(0));
    let client = connect(Arc::clone(&node)).await;
    let mut stream = client.follow(session_id(), EventSeq::ZERO).await.unwrap();
    assert_eq!(stream.info().id, session_id());

    let items = drain(&mut stream).await;
    let expected: Vec<_> = (0..LOG_LEN).map(Ok).collect();
    assert_eq!(items, expected);
    assert_eq!(stream.cursor(), EventSeq::new(LOG_LEN));
    // Lagged once, paged the stored part, then subscribed again for the rest.
    assert_eq!(node.subscribes.load(Ordering::Relaxed), 2);
    assert!(node.reads.load(Ordering::Relaxed) >= 2);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn following_from_the_middle_starts_there() {
    let node = Arc::new(FakeNode::new(0));
    let client = connect(node).await;
    let mut stream = client.follow(session_id(), EventSeq::new(7)).await.unwrap();
    let items = drain(&mut stream).await;
    let expected: Vec<_> = (7..LOG_LEN).map(Ok).collect();
    assert_eq!(items, expected);
}

#[tokio::test]
async fn pruned_records_are_reported_once_and_the_stream_continues() {
    let node = Arc::new(FakeNode::new(3));
    let client = connect(node).await;
    let mut stream = client.follow(session_id(), EventSeq::ZERO).await.unwrap();
    let items = drain(&mut stream).await;
    let missing = StreamError::Missing {
        expected: EventSeq::ZERO,
        resumed_at: EventSeq::new(3),
    };
    assert!(!missing.is_fatal());
    let mut expected = vec![Err(missing)];
    expected.extend((3..LOG_LEN).map(Ok));
    assert_eq!(items, expected);
}

#[tokio::test]
async fn unknown_sessions_fail_up_front() {
    let client = connect(Arc::new(FakeNode::new(0))).await;
    let other = SessionId::random();
    let error = client.follow(other, EventSeq::ZERO).await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::NotFound));
    assert_eq!(
        client.session(other).await.unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(client.sessions().await.unwrap(), vec![info()]);
    assert_eq!(client.node().name, "fake");
}
