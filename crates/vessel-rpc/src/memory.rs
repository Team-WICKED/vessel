//! An in-memory [`FrameTransport`](crate::FrameTransport) pair.
//!
//! Used by tests across the workspace (no sockets, deterministic) and usable
//! for running a client and a node in one process. Each direction is a
//! bounded channel, so a reader that stops reading makes the writer wait,
//! exactly like a socket with a full buffer.

use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Sink, Stream};
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

/// One end of an in-memory connection.
#[derive(Debug)]
pub struct MemoryTransport {
    incoming: mpsc::Receiver<String>,
    outgoing: PollSender<String>,
}

/// The other end of the in-memory connection was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the other end of the in-memory transport is gone")]
pub struct Disconnected;

/// Two connected ends. Frames sent into one come out of the other; each
/// direction buffers at most `capacity` frames.
#[must_use]
pub fn pair(capacity: NonZeroUsize) -> (MemoryTransport, MemoryTransport) {
    let (a_tx, a_rx) = mpsc::channel(capacity.get());
    let (b_tx, b_rx) = mpsc::channel(capacity.get());
    (
        MemoryTransport {
            incoming: b_rx,
            outgoing: PollSender::new(a_tx),
        },
        MemoryTransport {
            incoming: a_rx,
            outgoing: PollSender::new(b_tx),
        },
    )
}

#[cfg(test)]
impl MemoryTransport {
    /// Close only the sending direction, as a peer that hangs up its side
    /// but never reads again.
    pub(crate) fn close_sending(&mut self) {
        self.outgoing.close();
    }
}

impl Stream for MemoryTransport {
    type Item = Result<String, Disconnected>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.incoming.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

impl Sink<String> for MemoryTransport {
    type Error = Disconnected;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Disconnected>> {
        self.outgoing
            .poll_reserve(cx)
            .map(|ready| ready.map_err(|_closed| Disconnected))
    }

    fn start_send(mut self: Pin<&mut Self>, frame: String) -> Result<(), Disconnected> {
        self.outgoing
            .send_item(frame)
            .map_err(|_closed| Disconnected)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Disconnected>> {
        // A channel send is complete once `start_send` returns.
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Disconnected>> {
        self.outgoing.close();
        Poll::Ready(Ok(()))
    }
}
