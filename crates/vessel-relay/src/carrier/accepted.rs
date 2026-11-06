//! The websockets a node accepts on `/secure` (axum).

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bytes::Bytes;
use futures_util::{Sink, SinkExt as _, Stream};
use vessel_proto::NodeClose;

use super::{CLOSE_TIMEOUT, Idle, NodeCarrier, closed, text_message};

/// A websocket a node accepted, carrying records as binary messages.
///
/// Upgrade with `max_message_size` and `max_frame_size` of
/// [`vessel_secure::MAX_RECORD`], so an oversized message is refused from
/// its header before it is read.
pub struct AxumCarrier {
    socket: WebSocket,
    idle: Option<Idle>,
}

impl fmt::Debug for AxumCarrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AxumCarrier")
            .field("idle", &self.idle.as_ref().map(|idle| idle.after))
            .finish_non_exhaustive()
    }
}

impl AxumCarrier {
    /// Carry records over `socket`. With `idle`, fail after that long
    /// without a message (only useful if someone pings).
    #[must_use]
    pub fn new(socket: WebSocket, idle: Option<Duration>) -> Self {
        Self {
            socket,
            idle: Idle::new(idle),
        }
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
                Poll::Pending => return Idle::poll_expired(&mut this.idle, cx),
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
        Pin::new(&mut self.get_mut().socket)
            .poll_ready(cx)
            .map_err(io_error)
    }

    fn start_send(self: Pin<&mut Self>, record: Bytes) -> io::Result<()> {
        Pin::new(&mut self.get_mut().socket)
            .start_send(Message::Binary(record))
            .map_err(io_error)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket)
            .poll_flush(cx)
            .map_err(io_error)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket)
            .poll_close(cx)
            .map_err(io_error)
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
