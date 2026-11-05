//! The handles a handler uses to push subscription frames into its
//! connection: [`Outbound`] (per connection) and [`SubscriptionSink`] (per
//! subscription). Both only queue items; the writer (`super::writer`)
//! decides what is written and when.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{EventRecord, Outcome, RequestId, SubscriptionEnd, SubscriptionId};

use crate::error::OutboundError;

/// One item for the writer.
pub(super) enum Outgoing {
    /// The answer to a call.
    Response { id: RequestId, outcome: Outcome },
    /// A handler opened a subscription; its events wait for `announce`.
    Open {
        subscription: SubscriptionId,
        announce: oneshot::Sender<()>,
        cancel: CancellationToken,
    },
    /// A record for an open subscription, shared with its other
    /// subscribers.
    Event {
        subscription: SubscriptionId,
        record: Arc<EventRecord>,
    },
    /// The forwarder ended its subscription.
    End {
        subscription: SubscriptionId,
        end: SubscriptionEnd,
    },
    /// The client asked to end a subscription (`session.unsubscribe`).
    Close {
        subscription: SubscriptionId,
        found: oneshot::Sender<bool>,
    },
    /// A sink was dropped without ending its subscription.
    Released(SubscriptionId),
}

/// A connection's outlet for subscriptions. Cheap to clone; a handler keeps
/// it in its per-connection context.
#[derive(Debug, Clone)]
pub struct Outbound {
    queue: mpsc::Sender<Outgoing>,
    next_subscription: Arc<AtomicU64>,
    closing: CancellationToken,
}

impl Outbound {
    pub(super) fn new(queue: mpsc::Sender<Outgoing>, closing: CancellationToken) -> Self {
        Self {
            queue,
            next_subscription: Arc::new(AtomicU64::new(0)),
            closing,
        }
    }

    /// Open a subscription: mint its id and get the sink its frames go
    /// through. Return the id in this call's `Subscribed` reply; the sink
    /// holds every frame back until that reply has been queued.
    ///
    /// If the call fails after this, drop the sink and the subscription is
    /// discarded without the client ever seeing it.
    ///
    /// # Errors
    /// The connection is closing, or its subscription ids are exhausted.
    pub async fn open_subscription(&self) -> Result<SubscriptionSink, OutboundError> {
        let id = self
            .next_subscription
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map(SubscriptionId::new)
            .map_err(|_exhausted| OutboundError::SubscriptionIdsExhausted)?;
        let (announce, announced) = oneshot::channel();
        let cancel = self.closing.child_token();
        let open = Outgoing::Open {
            subscription: id,
            announce,
            cancel: cancel.clone(),
        };
        self.queue
            .send(open)
            .await
            .map_err(|_closed| OutboundError::ConnectionClosed)?;
        Ok(SubscriptionSink {
            id,
            queue: self.queue.clone(),
            announced: Some(announced),
            cancel,
            ended: false,
        })
    }

    /// End a subscription at the client's request. The client receives
    /// `SubscriptionEnded{Unsubscribed}` before this call's answer, and the
    /// subscription's forwarder sees [`SubscriptionSink::cancelled`].
    ///
    /// # Errors
    /// No such subscription is open, or the connection is closing.
    pub async fn close_subscription(&self, id: SubscriptionId) -> Result<(), OutboundError> {
        let (found, was_open) = oneshot::channel();
        let close = Outgoing::Close {
            subscription: id,
            found,
        };
        self.queue
            .send(close)
            .await
            .map_err(|_closed| OutboundError::ConnectionClosed)?;
        match was_open.await {
            Ok(true) => Ok(()),
            Ok(false) => Err(OutboundError::UnknownSubscription(id)),
            Err(_writer_gone) => Err(OutboundError::ConnectionClosed),
        }
    }

    /// Resolves once the connection is closing.
    pub async fn closed(&self) {
        self.closing.cancelled().await;
    }
}

/// A subscription's sink was closed: the client unsubscribed or the
/// connection is closing. The forwarder should stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the subscription is closed")]
pub struct SubscriptionClosed;

/// Where one subscription's frames go. Owned by the task forwarding that
/// subscription's records.
///
/// End it with [`end`](Self::end). Dropping it without ending discards the
/// subscription silently (the right thing when the call that opened it
/// failed).
#[derive(Debug)]
pub struct SubscriptionSink {
    id: SubscriptionId,
    queue: mpsc::Sender<Outgoing>,
    /// Resolves when the reply announcing `id` was queued; `None` after.
    announced: Option<oneshot::Receiver<()>>,
    cancel: CancellationToken,
    ended: bool,
}

impl SubscriptionSink {
    /// The id to put in the `Subscribed` reply.
    #[must_use]
    pub const fn id(&self) -> SubscriptionId {
        self.id
    }

    /// Queue one record. Waits for the announcement on first use, and while
    /// the connection's queue is full (that wait is the backpressure that
    /// makes a slow client lag instead of growing a buffer).
    ///
    /// # Errors
    /// The client unsubscribed or the connection is closing.
    ///
    /// The record is shared, not copied: the same `Arc` can go to every
    /// subscriber of a session.
    pub async fn send(&mut self, record: Arc<EventRecord>) -> Result<(), SubscriptionClosed> {
        self.wait_announced().await?;
        let event = Outgoing::Event {
            subscription: self.id,
            record,
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(SubscriptionClosed),
            sent = self.queue.send(event) => sent.map_err(|_closed| SubscriptionClosed),
        }
    }

    /// End the subscription; `SubscriptionEnded` is its last frame.
    pub async fn end(mut self, end: SubscriptionEnd) {
        if self.wait_announced().await.is_err() {
            return;
        }
        self.ended = true;
        let end = Outgoing::End {
            subscription: self.id,
            end,
        };
        if self.queue.send(end).await.is_err() {
            tracing::trace!(subscription = %self.id, "connection gone before subscription end");
        }
    }

    /// Resolves when the client unsubscribed or the connection is closing.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    async fn wait_announced(&mut self) -> Result<(), SubscriptionClosed> {
        let Some(announced) = self.announced.as_mut() else {
            return Ok(());
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(SubscriptionClosed),
            result = announced => result.map_err(|_never_announced| SubscriptionClosed)?,
        }
        self.announced = None;
        Ok(())
    }
}

impl Drop for SubscriptionSink {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        // Best effort: if the queue is full the entry stays in the table
        // until the connection closes, which costs a few bytes and nothing
        // else (no frame is ever written for it).
        if self.queue.try_send(Outgoing::Released(self.id)).is_err() {
            tracing::trace!(subscription = %self.id, "could not release subscription early");
        }
    }
}
