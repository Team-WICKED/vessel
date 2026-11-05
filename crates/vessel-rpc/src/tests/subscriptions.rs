//! Subscriptions: ordering, unsubscribe, and lag on a slow subscriber.

use super::*;

#[tokio::test]
async fn subscriptions_deliver_every_record_after_their_reply() {
    const SUBSCRIPTIONS: usize = 20;
    let mut running = start(ClientConfig::default()).await;
    // Many at once, each with a forwarder that tries to queue its records
    // before the reply: any ordering race loses records.
    let subscriptions = (0..SUBSCRIPTIONS).map(|_| {
        running.client.subscribe(Subscribe {
            session: session(1),
            from: EventSeq::new(1),
        })
    });
    let subscriptions = futures_util::future::join_all(subscriptions).await;
    for _ in 0..SUBSCRIPTIONS {
        running.forwarded.recv().await.unwrap();
    }
    // Answered after every subscription frame, so afterwards everything is
    // buffered client-side and nothing below needs to wait.
    let _barrier = running.client.call(GetNode).await.unwrap();
    for subscription in subscriptions {
        let mut subscription = subscription.unwrap();
        assert_eq!(subscription.info().id, session(1));
        let mut next = || subscription.next().now_or_never();
        for seq in 1..RECORDS {
            assert_eq!(next(), Some(SubscriptionItem::Record(record(seq))));
        }
        let ended = Some(SubscriptionItem::Ended(SubscriptionEnd::SessionEnded));
        assert_eq!(next(), ended);
        assert_eq!(next(), ended);
    }
}

#[tokio::test]
async fn dropping_a_subscription_unsubscribes() {
    let running = start(ClientConfig::default()).await;
    let subscription = running
        .client
        .subscribe(Subscribe {
            session: endless(),
            from: EventSeq::ZERO,
        })
        .await
        .unwrap();
    drop(subscription);
    running.probe.unsubscribed.notified().await;
}

#[tokio::test]
async fn unsubscribe_ends_the_subscription_before_its_answer() {
    let running = start(ClientConfig::default()).await;
    let mut subscription = running
        .client
        .subscribe(Subscribe {
            session: endless(),
            from: EventSeq::ZERO,
        })
        .await
        .unwrap();
    let request = Unsubscribe {
        subscription: subscription.id(),
    };
    assert_eq!(running.client.call(request).await, Ok(Ack));
    // The end was written before the ack, so it is already buffered.
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Ended(SubscriptionEnd::Unsubscribed)
    );
    running.probe.unsubscribed.notified().await;
    assert_eq!(
        running.client.call(request).await.unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
}

#[tokio::test]
async fn slow_subscriber_lags_without_blocking_the_connection() {
    let config = ClientConfig {
        subscription_capacity: NonZeroUsize::new(2).unwrap(),
        ..ClientConfig::default()
    };
    let mut running = start(config).await;
    let mut subscription = running
        .client
        .subscribe(Subscribe {
            session: session(1),
            from: EventSeq::ZERO,
        })
        .await
        .unwrap();
    running.forwarded.recv().await.unwrap();
    // Its answer is written after every record, so once it is back the
    // driver has handled all of them while nobody read the subscription.
    assert_eq!(
        running.client.call(GetNode).await.unwrap().name,
        "test-node"
    );
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Record(record(0))
    );
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Record(record(1))
    );
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Ended(SubscriptionEnd::Lagged {
            resume_from: EventSeq::new(2)
        })
    );
}

#[tokio::test]
async fn an_abandoned_subscription_ends_lagged_at_the_first_unsent_record() {
    let running = start(ClientConfig::default()).await;
    let mut subscription = running
        .client
        .subscribe(Subscribe {
            session: abandoned(),
            from: EventSeq::new(5),
        })
        .await
        .unwrap();
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Record(record(5))
    );
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Record(record(6))
    );
    assert_eq!(
        subscription.next().await,
        SubscriptionItem::Ended(SubscriptionEnd::Lagged {
            resume_from: EventSeq::new(7)
        })
    );
}
