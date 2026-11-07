//! Unit tests of the writer's subscription table.

use super::*;

/// Open `subscription` at `from` and return the sender that drops its
/// sink when dropped.
fn open(
    subscriptions: &mut Subscriptions,
    departures: &mut FuturesUnordered<Departure>,
    subscription: SubscriptionId,
    from: EventSeq,
) -> oneshot::Sender<()> {
    let (announce, _announced) = oneshot::channel();
    let (departing, departure) = oneshot::channel();
    let frames = subscriptions.apply(
        Outgoing::Open {
            subscription,
            from,
            announce,
            cancel: CancellationToken::new(),
            departure,
        },
        departures,
    );
    assert!(frames.iter().all(Option::is_none));
    departing
}

fn reply(subscription: SubscriptionId) -> Outgoing {
    let mut info = vessel_proto::SessionInfo::new(
        vessel_proto::SessionId::random(),
        vessel_proto::SessionSpec::new(
            vessel_proto::HarnessKind::Codex,
            vessel_proto::SessionMode::Terminal {
                args: Vec::new(),
                size: vessel_proto::TerminalSize::default(),
            },
        ),
        chrono::DateTime::UNIX_EPOCH,
    );
    info.status = vessel_proto::SessionStatus::Running;
    Outgoing::Response {
        id: vessel_proto::RequestId::new(1),
        outcome: Box::new(Outcome::Ok(Reply::Subscribe(vessel_proto::Subscribed {
            subscription,
            info,
        }))),
    }
}

fn ended(frame: Option<&Frame>) -> Option<&SubscriptionEnd> {
    if let Some(Frame::Server(frame)) = frame
        && let ServerFrame::SubscriptionEnded { end, .. } = &**frame
    {
        Some(end)
    } else {
        None
    }
}

/// A sink dropped before the reply naming it was written: the reply
/// still goes out, followed by `Lagged` from where the subscription
/// started (not from zero).
#[test]
fn a_reply_for_a_released_subscription_resumes_from_its_start() {
    let mut subscriptions = Subscriptions::default();
    let mut departures = FuturesUnordered::new();
    let id = SubscriptionId::new(1);
    drop(open(
        &mut subscriptions,
        &mut departures,
        id,
        EventSeq::new(42),
    ));
    assert!(subscriptions.depart(id, false).is_none());

    let frames = subscriptions.apply(reply(id), &mut departures);
    assert!(matches!(
        &frames[0],
        Some(Frame::Server(frame)) if matches!(**frame, ServerFrame::Response { .. })
    ));
    assert_eq!(
        ended(frames[1].as_ref()),
        Some(&SubscriptionEnd::Lagged {
            resume_from: EventSeq::new(42)
        })
    );
    assert!(subscriptions.released.is_empty());
}

/// Releases whose reply never comes are forgotten oldest first, so the
/// memory stays bounded; a late reply then resumes from zero.
#[test]
fn released_subscriptions_are_remembered_within_a_bound() {
    let mut subscriptions = Subscriptions::default();
    let mut departures = FuturesUnordered::new();
    let count = u64::try_from(RELEASED).unwrap() + 1;
    for n in 1..=count {
        let id = SubscriptionId::new(n);
        drop(open(
            &mut subscriptions,
            &mut departures,
            id,
            EventSeq::new(n),
        ));
        assert!(subscriptions.depart(id, false).is_none());
    }
    assert_eq!(subscriptions.released.len(), RELEASED);
    let evicted = subscriptions.apply(reply(SubscriptionId::new(1)), &mut departures);
    assert_eq!(
        ended(evicted[1].as_ref()),
        Some(&SubscriptionEnd::Lagged {
            resume_from: EventSeq::ZERO
        })
    );
    let kept = subscriptions.apply(reply(SubscriptionId::new(count)), &mut departures);
    assert_eq!(
        ended(kept[1].as_ref()),
        Some(&SubscriptionEnd::Lagged {
            resume_from: EventSeq::new(count)
        })
    );
}
