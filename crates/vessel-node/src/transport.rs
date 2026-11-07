//! axum's websocket adapted to `vessel-rpc`'s frame transport: one text
//! message per frame. axum answers pings itself; a binary message is an
//! error, since protocol 2 is text frames only.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{Sink, Stream};

/// Why the websocket transport failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// The websocket itself failed.
    #[error("websocket failed")]
    Websocket(#[from] axum::Error),
    /// The client sent a binary message.
    #[error("the client sent a binary message; protocol 2 uses text frames only")]
    BinaryFrame,
}

/// A client's websocket carrying protocol frames.
#[derive(Debug)]
pub(crate) struct WsTransport(WebSocket);

impl WsTransport {
    pub(crate) const fn new(socket: WebSocket) -> Self {
        Self(socket)
    }
}

impl Stream for WsTransport {
    type Item = Result<String, TransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let message = match ready!(Pin::new(&mut self.0).poll_next(cx)) {
                None => return Poll::Ready(None),
                Some(Err(error)) => return Poll::Ready(Some(Err(error.into()))),
                Some(Ok(message)) => message,
            };
            match message {
                Message::Text(text) => return Poll::Ready(Some(Ok(text.as_str().to_owned()))),
                Message::Binary(_) => return Poll::Ready(Some(Err(TransportError::BinaryFrame))),
                Message::Close(_) => return Poll::Ready(None),
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    }
}

impl Sink<String> for WsTransport {
    type Error = TransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll_ready(cx).map_err(Into::into)
    }

    fn start_send(mut self: Pin<&mut Self>, frame: String) -> Result<(), Self::Error> {
        Pin::new(&mut self.0)
            .start_send(Message::Text(frame.into()))
            .map_err(Into::into)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll_flush(cx).map_err(Into::into)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll_close(cx).map_err(Into::into)
    }
}
