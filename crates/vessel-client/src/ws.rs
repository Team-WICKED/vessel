//! The websocket transport: `tokio-tungstenite` adapted to `vessel-rpc`'s
//! frame transport (one text message per frame).
//!
//! Pings are answered by tungstenite itself; this adapter only skips them.
//! A binary message is an error, since protocol 2 is text frames only and a
//! peer sending binary is speaking something else.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::ConnectError;
use crate::token::BearerToken;
use crate::url::NodeUrl;

/// Why the websocket transport failed.
#[derive(Debug, thiserror::Error)]
pub enum WsTransportError {
    /// The websocket itself failed.
    #[error("websocket failed")]
    Websocket(#[from] tungstenite::Error),
    /// The peer sent a binary message.
    #[error("the peer sent a binary message; protocol 2 uses text frames only")]
    BinaryFrame,
}

/// A websocket carrying protocol frames.
#[derive(Debug)]
pub(crate) struct WsTransport<S> {
    inner: WebSocketStream<S>,
}

impl<S> WsTransport<S> {
    pub(crate) const fn new(inner: WebSocketStream<S>) -> Self {
        Self { inner }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream for WsTransport<S> {
    type Item = Result<String, WsTransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let message = match ready!(Pin::new(&mut self.inner).poll_next(cx)) {
                None => return Poll::Ready(None),
                Some(Err(error)) => return Poll::Ready(Some(Err(error.into()))),
                Some(Ok(message)) => message,
            };
            match message {
                Message::Text(text) => return Poll::Ready(Some(Ok(text.as_str().to_owned()))),
                Message::Binary(_) => {
                    return Poll::Ready(Some(Err(WsTransportError::BinaryFrame)));
                }
                Message::Close(_) => return Poll::Ready(None),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Sink<String> for WsTransport<S> {
    type Error = WsTransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx).map_err(Into::into)
    }

    fn start_send(mut self: Pin<&mut Self>, frame: String) -> Result<(), Self::Error> {
        Pin::new(&mut self.inner)
            .start_send(Message::Text(frame.into()))
            .map_err(Into::into)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(Into::into)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx).map_err(Into::into)
    }
}

/// Open the websocket to `url`, presenting `token`.
pub(crate) async fn connect(
    url: &NodeUrl,
    token: &BearerToken,
) -> Result<WsTransport<MaybeTlsStream<TcpStream>>, ConnectError> {
    if url.is_tls() {
        return Err(ConnectError::TlsUnsupported);
    }
    let websocket = |source| ConnectError::Websocket {
        url: url.clone(),
        source,
    };
    let mut request = url
        .as_uri()
        .clone()
        .into_client_request()
        .map_err(websocket)?;
    // The token is visible ASCII (checked when it was parsed), so this only
    // fails if that check is broken; report it without echoing the value.
    let mut authorization = HeaderValue::try_from(format!("Bearer {}", token.expose()))
        .map_err(|_invalid| ConnectError::InvalidToken)?;
    authorization.set_sensitive(true);
    let _previous = request.headers_mut().insert(AUTHORIZATION, authorization);
    match tokio_tungstenite::connect_async(request).await {
        Ok((stream, _response)) => Ok(WsTransport::new(stream)),
        Err(tungstenite::Error::Http(response))
            if response.status() == StatusCode::UNAUTHORIZED =>
        {
            Err(ConnectError::Unauthorized)
        }
        Err(source) => Err(websocket(source)),
    }
}
