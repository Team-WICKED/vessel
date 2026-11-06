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

mod subscriptions;

use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{Sink, SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::encode::Frame;
use super::outbound::Outgoing;
use crate::error::ServeError;
use subscriptions::{Departure, Subscriptions};

/// Frames written before a flush even while more are queued, so steady
/// output (terminal echo) is not held back waiting for an empty queue.
const FLUSH_FRAMES: usize = 32;
/// Bytes written before a flush even while more are queued.
const FLUSH_BYTES: usize = 64 * 1024;

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
/// A frame is handed to the sink only once the sink has room, so a close
/// that interrupts that wait keeps the frame (it is already off the queue)
/// and writes it first. After `closing`, it writes until the reader has
/// `settled` (answers to calls that were still running), then what is
/// queued, all within `close_timeout`.
pub(super) async fn write<K, E>(
    mut sink: K,
    mut queue: mpsc::Receiver<Outgoing>,
    closing: CancellationToken,
    settled: CancellationToken,
    close_timeout: Duration,
) -> Result<(), ServeError<E>>
where
    K: Sink<String, Error = E> + Unpin,
{
    let _close_on_exit = closing.clone().drop_guard();
    let mut subscriptions = Subscriptions::default();
    let mut departures = FuturesUnordered::new();
    let mut unflushed = Unflushed::default();
    // Frames taken off the queue but not yet written when closing began.
    let mut carried = Vec::new();
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
        let mut frames = frames.into_iter().flatten();
        while let Some(frame) = frames.next() {
            // Not `feed` raced against `closing`: the losing `feed` would
            // drop the frame it owns, and an answer would go missing.
            let ready = tokio::select! {
                biased;
                () = closing.cancelled() => None,
                ready = std::future::poll_fn(|cx| sink.poll_ready_unpin(cx)) => Some(ready),
            };
            let Some(ready) = ready else {
                carried.push(frame);
                carried.extend(frames);
                break 'serve;
            };
            ready.map_err(ServeError::Transport)?;
            let text = frame.encode().map_err(ServeError::Encode)?;
            let burst = unflushed.add(text.len());
            sink.start_send_unpin(text).map_err(ServeError::Transport)?;
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
        finish(
            &mut sink,
            carried,
            &mut queue,
            &mut subscriptions,
            &mut departures,
            &settled,
        ),
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

/// Write what the closing connection still owes the client, then close the
/// sink. Best effort: the client may already be gone.
///
/// First the frames `carried` over from the interrupted write, then
/// everything queued until the reader has settled, which covers the
/// answers to calls that were still running when the connection began
/// closing (`node.stop{now}` among them). Then only what is queued at that
/// point: forwarders keep producing until they notice the close, and the
/// close must not wait for them.
async fn finish<K, E>(
    sink: &mut K,
    carried: Vec<Frame>,
    queue: &mut mpsc::Receiver<Outgoing>,
    subscriptions: &mut Subscriptions,
    departures: &mut FuturesUnordered<Departure>,
    settled: &CancellationToken,
) where
    K: Sink<String, Error = E> + Unpin,
{
    for frame in carried {
        if !feed_closing(sink, frame).await {
            return;
        }
    }
    loop {
        let item = tokio::select! {
            biased;
            () = settled.cancelled() => break,
            item = queue.recv() => item,
        };
        // `None`: every sender is gone, the reader's included.
        let Some(item) = item else { break };
        if !write_closing(sink, item, subscriptions, departures).await {
            return;
        }
    }
    let queued = queue.len();
    for _ in 0..queued {
        let Ok(item) = queue.try_recv() else { break };
        if !write_closing(sink, item, subscriptions, departures).await {
            return;
        }
    }
    if sink.close().await.is_err() {
        tracing::trace!("client gone while closing");
    }
}

/// Write one queued item while closing; `false` once the client is gone.
async fn write_closing<K, E>(
    sink: &mut K,
    item: Outgoing,
    subscriptions: &mut Subscriptions,
    departures: &mut FuturesUnordered<Departure>,
) -> bool
where
    K: Sink<String, Error = E> + Unpin,
{
    for frame in subscriptions.apply(item, departures).into_iter().flatten() {
        if !feed_closing(sink, frame).await {
            return false;
        }
    }
    true
}

/// Write one frame while closing; `false` once the client is gone.
async fn feed_closing<K, E>(sink: &mut K, frame: Frame) -> bool
where
    K: Sink<String, Error = E> + Unpin,
{
    let Ok(text) = frame.encode() else {
        return false;
    };
    if sink.feed(text).await.is_err() {
        tracing::trace!("client gone while closing");
        return false;
    }
    true
}

#[cfg(test)]
mod tests;
