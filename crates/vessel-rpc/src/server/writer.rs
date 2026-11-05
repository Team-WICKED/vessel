//! The writing half of a server connection.
//!
//! Every frame for one connection goes through one bounded queue to this
//! writer, which also owns the connection's subscription table. Owning both
//! is what makes the ordering rules cheap to keep:
//!
//! - **No event before its announcement.** A handler opens a subscription,
//!   starts a forwarder, and returns `Subscribed`; the forwarder runs in
//!   another task and could otherwise queue an `Event` before the reader
//!   queues the `Subscribed` answer, and a client would then see events for
//!   an id it has never heard of. So a `SubscriptionSink` holds back until
//!   this writer has taken the `Subscribed` answer naming its id off the
//!   queue; everything the sink queues after that is written after it.
//! - **Nothing after the end.** Once a subscription ended (by its forwarder,
//!   or by the client unsubscribing), the writer drops any frame still
//!   queued for it, so `SubscriptionEnded` is always the last frame for an id.
//!
//! [Decision Log]
//! - Alternatives for the ordering problem: make the client buffer events
//!   for unknown ids until a reply names them (unbounded, and wrong for a
//!   cancelled subscribe whose reply never comes); have the node delay its
//!   forwarder (it cannot know when the reply was queued); park early events
//!   in the writer (unbounded).
//! - Choice: a one-shot "announced" gate per subscription, fired by the
//!   writer. Bounded, and a subscription whose call failed or was cancelled
//!   simply never opens.
//! - Cost: the writer recognises the `session.subscribe` reply by type. That
//!   is protocol knowledge, which is this crate's job; it is the only
//!   method it singles out.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{Sink, SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{EventSeq, Outcome, Reply, ServerFrame, SubscriptionEnd, SubscriptionId};

use super::encode::Frame;
use super::outbound::Outgoing;
use crate::error::ServeError;

/// Frames written before a flush even while more are queued, so steady
/// output (terminal echo) is not held back waiting for an empty queue.
const FLUSH_FRAMES: usize = 32;
/// Bytes written before a flush even while more are queued.
const FLUSH_BYTES: usize = 64 * 1024;

/// The open subscriptions of one connection. Owned by the writer.
#[derive(Default)]
struct Subscriptions {
    open: HashMap<SubscriptionId, Open>,
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
struct Departure {
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
type Frames = [Option<Frame>; 2];

impl Subscriptions {
    /// Apply one queued item and return the frames to write. A newly opened
    /// subscription's departure is added to `departures`.
    fn apply(&mut self, item: Outgoing, departures: &mut FuturesUnordered<Departure>) -> Frames {
        match item {
            Outgoing::Response { id, outcome } => {
                let announced = match &outcome {
                    Outcome::Ok(Reply::Subscribe(subscribed)) => Some(subscribed.subscription),
                    Outcome::Ok(_) | Outcome::Err(_) => None,
                };
                let response = Some(Frame::Server(ServerFrame::Response { id, outcome }));
                let orphan = announced
                    .filter(|&subscription| !self.announce(subscription))
                    .map(|subscription| {
                        // Named but not open (its sink is already gone): end
                        // it right away so the client does not wait.
                        Frame::Server(ServerFrame::SubscriptionEnded {
                            subscription,
                            end: SubscriptionEnd::Lagged {
                                resume_from: EventSeq::ZERO,
                            },
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
                    Frame::Server(ServerFrame::SubscriptionEnded { subscription, end })
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
                    Frame::Server(ServerFrame::SubscriptionEnded {
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

    /// A sink went away. After a clean end there is nothing to do (its end
    /// is queued or written). Otherwise the subscription is removed; if the
    /// client already knows it, it gets `Lagged` from the first record it
    /// did not receive.
    fn depart(&mut self, subscription: SubscriptionId, cleanly: bool) -> Option<Frame> {
        if cleanly {
            return None;
        }
        let open = self.open.remove(&subscription)?;
        open.cancel.cancel();
        if open.announce.is_some() {
            return None;
        }
        tracing::debug!(%subscription, "subscription released without an end");
        Some(Frame::Server(ServerFrame::SubscriptionEnded {
            subscription,
            end: SubscriptionEnd::Lagged {
                resume_from: open.next,
            },
        }))
    }
}

/// Frames fed to the sink since its last flush.
///
/// A sink may hold fed frames back until it is flushed (`SplitSink` keeps
/// the last one), so the writer flushes whenever its queue runs empty with
/// frames pending, whether or not the last item produced a frame, and also
/// after a bounded burst, so steady output is not held back.
#[derive(Default)]
struct Unflushed {
    frames: usize,
    bytes: usize,
}

impl Unflushed {
    /// Count a fed frame; returns whether the burst limit calls for a flush.
    fn add(&mut self, bytes: usize) -> bool {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.frames >= FLUSH_FRAMES || self.bytes >= FLUSH_BYTES
    }

    const fn pending(&self) -> bool {
        self.frames > 0
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Write queued frames until `closing` is cancelled or the transport fails.
/// Cancels `closing` on the way out so the reader stops too.
///
/// Every wait on the sink also watches `closing`: a client that stops
/// reading must not keep its connection (and a node shutdown) waiting.
pub(super) async fn write<K, E>(
    mut sink: K,
    mut queue: mpsc::Receiver<Outgoing>,
    closing: CancellationToken,
    close_timeout: Duration,
) -> Result<(), ServeError<E>>
where
    K: Sink<String, Error = E> + Unpin,
{
    let _close_on_exit = closing.clone().drop_guard();
    let mut subscriptions = Subscriptions::default();
    let mut departures = FuturesUnordered::new();
    let mut unflushed = Unflushed::default();
    'serve: loop {
        // Departures come after the queue: records a sink queued before it
        // went away are written first, so a released subscription's resume
        // point is the first record the client really did not get.
        let frames = tokio::select! {
            biased;
            () = closing.cancelled() => break,
            item = queue.recv() => {
                // Every sender lives in this connection's context; `None`
                // means it is gone, which only happens on the way out.
                let Some(item) = item else { break };
                subscriptions.apply(item, &mut departures)
            }
            Some((subscription, cleanly)) = departures.next(), if !departures.is_empty() => {
                [subscriptions.depart(subscription, cleanly), None]
            }
        };
        for frame in frames.into_iter().flatten() {
            let text = frame.encode().map_err(ServeError::Encode)?;
            let burst = unflushed.add(text.len());
            let written = tokio::select! {
                biased;
                () = closing.cancelled() => break 'serve,
                written = sink.feed(text) => written,
            };
            written.map_err(ServeError::Transport)?;
            if burst {
                flush(&mut sink, &closing, &mut unflushed).await?;
            }
        }
        if unflushed.pending() && queue.is_empty() {
            flush(&mut sink, &closing, &mut unflushed).await?;
        }
    }
    let finished = tokio::time::timeout(
        close_timeout,
        finish(&mut sink, &mut queue, &mut subscriptions, &mut departures),
    );
    if finished.await.is_err() {
        tracing::debug!("client did not take the last frames in time");
    }
    Ok(())
}

/// Flush the sink, unless the connection starts closing first (then the
/// close path flushes what it can).
async fn flush<K, E>(
    sink: &mut K,
    closing: &CancellationToken,
    unflushed: &mut Unflushed,
) -> Result<(), ServeError<E>>
where
    K: Sink<String, Error = E> + Unpin,
{
    let flushed = tokio::select! {
        biased;
        () = closing.cancelled() => return Ok(()),
        flushed = sink.flush() => flushed,
    };
    unflushed.reset();
    flushed.map_err(ServeError::Transport)
}

/// Write what was already queued when the connection started closing (for
/// example answers to calls that finished just before a shutdown), then
/// close the sink. Best effort: the client may already be gone.
async fn finish<K, E>(
    sink: &mut K,
    queue: &mut mpsc::Receiver<Outgoing>,
    subscriptions: &mut Subscriptions,
    departures: &mut FuturesUnordered<Departure>,
) where
    K: Sink<String, Error = E> + Unpin,
{
    // Only what is queued now: forwarders keep producing until they notice
    // the close, and the close must not wait for them.
    let queued = queue.len();
    for _ in 0..queued {
        let Ok(item) = queue.try_recv() else { break };
        for frame in subscriptions.apply(item, departures).into_iter().flatten() {
            let Ok(text) = frame.encode() else {
                return;
            };
            if sink.feed(text).await.is_err() {
                tracing::trace!("client gone while closing");
                return;
            }
        }
    }
    if sink.close().await.is_err() {
        tracing::trace!("client gone while closing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_flushed_after_a_bounded_number_of_frames_or_bytes() {
        let mut unflushed = Unflushed::default();
        assert!(!unflushed.pending());
        for _ in 1..FLUSH_FRAMES {
            assert!(!unflushed.add(10));
        }
        assert!(unflushed.pending());
        assert!(unflushed.add(10), "the frame limit calls for a flush");
        unflushed.reset();
        assert!(unflushed.add(FLUSH_BYTES), "so does the byte limit");
    }
}
