//! The websockets nodes and clients open: to a relay, or to a node's
//! `/secure`.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use vessel_proto::NodeClose;

use super::{CLOSE_TIMEOUT, Idle, NodeCarrier, closed, text_message};
use crate::socket::{Socket, io_error};

/// A websocket this end opened, carrying records as binary messages.
pub struct WsCarrier {
    socket: Socket,
    idle: Option<Idle>,
}

impl fmt::Debug for WsCarrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsCarrier")
            .field("idle", &self.idle.as_ref().map(|idle| idle.after))
            .finish_non_exhaustive()
    }
}

impl WsCarrier {
    /// Carry records over `socket`, failing after `idle` without a message.
    pub(crate) fn new(socket: Socket, idle: Option<Duration>) -> Self {
        Self {
            socket,
            idle: Idle::new(idle),
        }
    }
}

impl Stream for WsCarrier {
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
                    let code = frame.map(|frame| u16::from(frame.code));
                    return Poll::Ready(closed(code).map(Err));
                }
                Message::Text(_) => return Poll::Ready(Some(Err(text_message()))),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }
}

impl Sink<Bytes> for WsCarrier {
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

impl NodeCarrier for WsCarrier {
    async fn close_with(mut self, close: NodeClose) {
        let frame = CloseFrame {
            code: close.code().into(),
            reason: "".into(),
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, self.socket.close(Some(frame)))
            .await
            .is_err()
        {
            tracing::debug!(?close, "closing a connection timed out");
        }
    }
}
