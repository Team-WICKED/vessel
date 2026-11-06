//! The writer's table of one connection's subscriptions: the
//! announcement gate of each, their departures, and the ones released
//! before their reply was written. It is what keeps the ordering rules of
//! the parent module.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::stream::FuturesUnordered;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use vessel_proto::{EventSeq, Outcome, Reply, ServerFrame, SubscriptionEnd, SubscriptionId};

use super::super::encode::Frame;
use super::super::outbound::Outgoing;

/// Subscriptions released before their announcement that are remembered
/// for a reply that may still name them (see [`Subscriptions::released`]).
const RELEASED: usize = 32;

/// The open subscriptions of one connection. Owned by the writer.
#[derive(Default)]
pub(super) struct Subscriptions {
    open: HashMap<SubscriptionId, Open>,
    /// Subscriptions whose sink went away before the reply naming them was
    /// written, with where each started. If that reply still comes, the
    /// subscription is ended at once with `Lagged` from its start, the
    /// first record the client did not get.
    ///
    /// Bounded and oldest first: an entry whose reply never comes (the call
    /// failed or was cancelled after opening) would otherwise stay for the
    /// connection's life. A reply for an evicted entry falls back to
    /// resuming from zero, which is always valid, only slower; that takes
    /// [`RELEASED`] such releases in between, each needing a call to fail
    /// at exactly that point.
    released: VecDeque<(SubscriptionId, EventSeq)>,
}

struct Open {
    /// Fired when the reply naming this subscription is written; `None`
    /// afterwards.
    announce: Option<oneshot::Sender<()>>,
    /// Cancelled when the client unsubscribes; the forwarder watches it.
    cancel: CancellationToken,
    /// The first record not written yet: where the client resumes if the
    /// subscription is released without an end.
    next: EventSeq,
}

/// Resolves when a sink goes away, with whether it ended cleanly.
pub(super) struct Departure {
    subscription: SubscriptionId,
    signal: oneshot::Receiver<()>,
}

impl Future for Departure {
    type Output = (SubscriptionId, bool);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let subscription = self.subscription;
        Pin::new(&mut self.signal)
            .poll(cx)
            .map(|signal| (subscription, signal.is_ok()))
    }
}

/// The frames one queued item produces: none, one, or (a subscribe reply
/// naming a subscription that is already gone) a reply and an end. A fixed
/// array, so the per-event path does not allocate for it.
pub(super) type Frames = [Option<Frame>; 2];

impl Subscriptions {
    /// Apply one queued item and return the frames to write. A newly opened
    /// subscription's departure is added to `departures`.
    pub(super) fn apply(
        &mut self,
        item: Outgoing,
        departures: &mut FuturesUnordered<Departure>,
    ) -> Frames {
        match item {
            Outgoing::Response { id, outcome } => {
                let announced = match &*outcome {
                    Outcome::Ok(Reply::Subscribe(subscribed)) => Some(subscribed.subscription),
                    Outcome::Ok(_) | Outcome::Err(_) => None,
                };
                let response = Some(Frame::server(ServerFrame::Response {
                    id,
                    outcome: *outcome,
                }));
                let orphan = announced
                    .filter(|&subscription| !self.announce(subscription))
                    .map(|subscription| {
                        // Named but not open (its sink is already gone): end
                        // it right away so the client does not wait.
                        let resume_from = self.take_released(subscription);
                        Frame::server(ServerFrame::SubscriptionEnded {
                            subscription,
                            end: SubscriptionEnd::Lagged { resume_from },
                        })
                    });
                [response, orphan]
            }
            Outgoing::Open {
                subscription,
                from,
                announce,
                cancel,
                departure,
            } => {
                let open = Open {
                    announce: Some(announce),
                    cancel,
                    next: from,
                };
                if self.open.insert(subscription, open).is_some() {
                    tracing::error!(%subscription, "subscription id opened twice");
                }
                departures.push(Departure {
                    subscription,
                    signal: departure,
                });
                [None, None]
            }
            Outgoing::Event {
                subscription,
                record,
            } => {
                let open = self.open.get_mut(&subscription);
                let frame = open.map(|open| {
                    open.next = record.seq.next().unwrap_or(record.seq);
                    Frame::Event {
                        subscription,
                        record,
                    }
                });
                [frame, None]
            }
            Outgoing::End { subscription, end } => {
                let frame = self.open.remove(&subscription).map(|_open| {
                    Frame::server(ServerFrame::SubscriptionEnded { subscription, end })
                });
                [frame, None]
            }
            Outgoing::Close {
                subscription,
                found,
            } => {
                let closed = self.open.remove(&subscription);
                let frame = closed.as_ref().map(|open| {
                    open.cancel.cancel();
                    Frame::server(ServerFrame::SubscriptionEnded {
                        subscription,
                        end: SubscriptionEnd::Unsubscribed,
                    })
                });
                if found.send(closed.is_some()).is_err() {
                    tracing::trace!(%subscription, "unsubscribe call went away");
                }
                [frame, None]
            }
        }
    }

    /// Fire the announcement gate; `false` if the subscription is not open.
    fn announce(&mut self, subscription: SubscriptionId) -> bool {
        let Some(open) = self.open.get_mut(&subscription) else {
            tracing::warn!(%subscription, "subscribe reply names a subscription that is not open");
            return false;
        };
        if let Some(announce) = open.announce.take()
            && announce.send(()).is_err()
        {
            tracing::trace!(%subscription, "forwarder gone before announcement");
        }
        true
    }

    /// Where a subscription released before its announcement started;
    /// zero if it is no longer remembered.
    fn take_released(&mut self, subscription: SubscriptionId) -> EventSeq {
        let Some(index) = self
            .released
            .iter()
            .position(|(released, _)| *released == subscription)
        else {
            tracing::warn!(%subscription, "released subscription forgotten; resuming from zero");
            return EventSeq::ZERO;
        };
        self.released
            .remove(index)
            .map_or(EventSeq::ZERO, |(_, from)| from)
    }

    /// A sink went away. After a clean end there is nothing to do (its end
    /// is queued or written). Otherwise the subscription is removed; if the
    /// client already knows it, it gets `Lagged` from the first record it
    /// did not receive, and if not, it is remembered in case the reply
    /// naming it is still to come.
    pub(super) fn depart(&mut self, subscription: SubscriptionId, cleanly: bool) -> Option<Frame> {
        if cleanly {
            return None;
        }
        let open = self.open.remove(&subscription)?;
        open.cancel.cancel();
        if open.announce.is_some() {
            if self.released.len() >= RELEASED
                && let Some((forgotten, _)) = self.released.pop_front()
            {
                tracing::debug!(subscription = %forgotten, "forgetting a released subscription");
            }
            self.released.push_back((subscription, open.next));
            return None;
        }
        tracing::debug!(%subscription, "subscription released without an end");
        Some(Frame::server(ServerFrame::SubscriptionEnded {
            subscription,
            end: SubscriptionEnd::Lagged {
                resume_from: open.next,
            },
        }))
    }
}

#[cfg(test)]
mod tests;
