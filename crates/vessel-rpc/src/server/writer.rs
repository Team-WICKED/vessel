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
use std::time::Duration;

use futures_util::{Sink, SinkExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{Outcome, Reply, ServerFrame, SubscriptionEnd, SubscriptionId};

use super::outbound::Outgoing;
use crate::error::ServeError;

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
}

impl Subscriptions {
    /// Apply one queued item and return the frame to write, if any.
    fn apply(&mut self, item: Outgoing) -> Option<ServerFrame> {
        match item {
            Outgoing::Response { id, outcome } => {
                if let Outcome::Ok(Reply::Subscribe(subscribed)) = &outcome {
                    self.announce(subscribed.subscription);
                }
                Some(ServerFrame::Response { id, outcome })
            }
            Outgoing::Open {
                subscription,
                announce,
                cancel,
            } => {
                let open = Open {
                    announce: Some(announce),
                    cancel,
                };
                if self.open.insert(subscription, open).is_some() {
                    tracing::error!(%subscription, "subscription id opened twice");
                }
                None
            }
            Outgoing::Event {
                subscription,
                record,
            } => self
                .open
                .contains_key(&subscription)
                .then_some(ServerFrame::Event {
                    subscription,
                    record,
                }),
            Outgoing::End { subscription, end } => self
                .open
                .remove(&subscription)
                .map(|_open| ServerFrame::SubscriptionEnded { subscription, end }),
            Outgoing::Close {
                subscription,
                found,
            } => {
                let closed = self.open.remove(&subscription);
                let frame = closed.as_ref().map(|open| {
                    open.cancel.cancel();
                    ServerFrame::SubscriptionEnded {
                        subscription,
                        end: SubscriptionEnd::Unsubscribed,
                    }
                });
                if found.send(closed.is_some()).is_err() {
                    tracing::trace!(%subscription, "unsubscribe call went away");
                }
                frame
            }
            Outgoing::Released(subscription) => {
                if let Some(open) = self.open.remove(&subscription) {
                    open.cancel.cancel();
                }
                None
            }
        }
    }

    fn announce(&mut self, subscription: SubscriptionId) {
        let announce = self
            .open
            .get_mut(&subscription)
            .and_then(|open| open.announce.take());
        let Some(announce) = announce else {
            tracing::warn!(%subscription, "subscribe reply names a subscription that is not open");
            return;
        };
        if announce.send(()).is_err() {
            tracing::trace!(%subscription, "forwarder gone before announcement");
        }
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
    loop {
        let item = tokio::select! {
            biased;
            () = closing.cancelled() => break,
            item = queue.recv() => item,
        };
        // Every sender lives in this connection's context; `None` means it is
        // gone, which only happens on the way out.
        let Some(item) = item else { break };
        let Some(frame) = subscriptions.apply(item) else {
            continue;
        };
        let text = serde_json::to_string(&frame).map_err(ServeError::Encode)?;
        // Queue frames without flushing while more are waiting, so a burst
        // costs one flush rather than one per frame.
        let flush = queue.is_empty();
        let written = tokio::select! {
            biased;
            () = closing.cancelled() => break,
            written = write_frame(&mut sink, text, flush) => written,
        };
        written.map_err(ServeError::Transport)?;
    }
    let finished = tokio::time::timeout(
        close_timeout,
        finish(&mut sink, &mut queue, &mut subscriptions),
    );
    if finished.await.is_err() {
        tracing::debug!("client did not take the last frames in time");
    }
    Ok(())
}

async fn write_frame<K, E>(sink: &mut K, text: String, flush: bool) -> Result<(), E>
where
    K: Sink<String, Error = E> + Unpin,
{
    sink.feed(text).await?;
    if flush {
        sink.flush().await?;
    }
    Ok(())
}

/// Write what was already queued when the connection started closing (for
/// example answers to calls that finished just before a shutdown), then
/// close the sink. Best effort: the client may already be gone.
async fn finish<K, E>(
    sink: &mut K,
    queue: &mut mpsc::Receiver<Outgoing>,
    subscriptions: &mut Subscriptions,
) where
    K: Sink<String, Error = E> + Unpin,
{
    // Only what is queued now: forwarders keep producing until they notice
    // the close, and the close must not wait for them.
    let queued = queue.len();
    for _ in 0..queued {
        let Ok(item) = queue.try_recv() else { break };
        let Some(frame) = subscriptions.apply(item) else {
            continue;
        };
        let Ok(text) = serde_json::to_string(&frame) else {
            break;
        };
        if sink.feed(text).await.is_err() {
            tracing::trace!("client gone while closing");
            return;
        }
    }
    if sink.close().await.is_err() {
        tracing::trace!("client gone while closing");
    }
}
