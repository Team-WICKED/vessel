//! The websockets a node accepts on `/secure` (axum).

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bytes::Bytes;
use futures_util::{Sink, SinkExt as _, Stream};
use vessel_proto::NodeClose;

use super::{
    CLOSE_TIMEOUT, Idle, Keepalive, NodeCarrier, Pinger, closed, ping_before_write, poll_ping,
    text_message,
};

/// A websocket a node accepted, carrying records as binary messages.
///
/// Upgrade with `max_message_size` and `max_frame_size` of
/// [`vessel_secure::MAX_RECORD`], so an oversized message is refused from
/// its header before it is read.
pub struct AxumCarrier {
    socket: WebSocket,
    idle: Option<Idle>,
    pinger: Option<Pinger>,
    /// The writing side is waiting on the socket (its last `poll_ready` or
    /// `poll_flush` was pending): the reading side must not take the
    /// socket's write waker from it.
    writer_waiting: bool,
}

impl fmt::Debug for AxumCarrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AxumCarrier")
            .field("idle", &self.idle.as_ref().map(|idle| idle.after))
            .field("ping", &self.pinger.as_ref().map(|pinger| pinger.every))
            .finish_non_exhaustive()
    }
}

impl AxumCarrier {
    /// Carry records over `socket`. With a `keepalive`, ping the client
    /// every `keepalive.ping` and fail after `keepalive.silence` without a
    /// message (its pongs count): what a node's own `/secure` needs, since
    /// no relay pings it.
    #[must_use]
    pub fn new(socket: WebSocket, keepalive: Option<Keepalive>) -> Self {
        Self {
            socket,
            idle: Idle::new(keepalive.map(|keepalive| keepalive.silence)),
            pinger: Pinger::new(keepalive.map(|keepalive| keepalive.ping)),
            writer_waiting: false,
        }
    }

    /// Note how a writing-side call on the socket ended.
    fn wrote<T>(&mut self, polled: Poll<T>) -> Poll<T> {
        self.writer_waiting = polled.is_pending();
        polled
    }
}

fn io_error(error: axum::Error) -> io::Error {
    io::Error::other(error)
}

impl Stream for AxumCarrier {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let message = match Pin::new(&mut this.socket).poll_next(cx) {
                Poll::Ready(Some(Ok(message))) => message,
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(io_error(error)))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {
                    let pinged = poll_ping(
                        &mut this.pinger,
                        Pin::new(&mut this.socket),
                        || Message::Ping(Bytes::new()),
                        this.writer_waiting,
                        cx,
                    );
                    if let Err(error) = pinged {
                        return Poll::Ready(Some(Err(io_error(error))));
                    }
                    return Idle::poll_expired(&mut this.idle, cx);
                }
            };
            Idle::heard(&mut this.idle);
            match message {
                Message::Binary(record) => return Poll::Ready(Some(Ok(record))),
                Message::Close(frame) => {
                    return Poll::Ready(closed(frame.map(|frame| frame.code)).map(Err));
                }
                Message::Text(_) => return Poll::Ready(Some(Err(text_message()))),
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    }
}

impl Sink<Bytes> for AxumCarrier {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            let ready = Pin::new(&mut this.socket).poll_ready(cx).map_err(io_error);
            match this.wrote(ready) {
                Poll::Ready(Ok(())) => {}
                other @ (Poll::Ready(Err(_)) | Poll::Pending) => return other,
            }
            let ping = || Message::Ping(Bytes::new());
            // An owed ping goes first; then the socket must be ready again.
            if !ping_before_write(&mut this.pinger, Pin::new(&mut this.socket), ping)
                .map_err(io_error)?
            {
                return Poll::Ready(Ok(()));
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, record: Bytes) -> io::Result<()> {
        Pin::new(&mut self.get_mut().socket)
            .start_send(Message::Binary(record))
            .map_err(io_error)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let flushed = Pin::new(&mut this.socket).poll_flush(cx).map_err(io_error);
        if let (Poll::Ready(Ok(())), Some(pinger)) = (&flushed, &mut this.pinger) {
            pinger.unflushed = false;
        }
        this.wrote(flushed)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let closed = Pin::new(&mut this.socket).poll_close(cx).map_err(io_error);
        this.wrote(closed)
    }
}

impl NodeCarrier for AxumCarrier {
    async fn close_with(mut self, close: NodeClose) {
        let frame = CloseFrame {
            code: close.code(),
            reason: "".into(),
        };
        let closing = async {
            let _sent = self.socket.send(Message::Close(Some(frame))).await;
            let _closed = self.socket.close().await;
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, closing).await.is_err() {
            tracing::debug!(?close, "closing a connection timed out");
        }
    }
}
