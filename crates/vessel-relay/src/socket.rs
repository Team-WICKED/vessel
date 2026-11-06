//! Opening a websocket to a relay, as a node or a client.

use std::io;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use vessel_secure::MAX_RECORD;

use crate::dial::DialError;
use crate::tls::TlsConfig;

/// A websocket to a relay.
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to `url` (`ws://` or `wss://`). Messages from the relay are
/// bounded by one record, checked from each frame's header before its
/// payload is read.
pub(crate) async fn open(url: &str, tls: &TlsConfig) -> Result<Socket, DialError> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RECORD))
        .max_frame_size(Some(MAX_RECORD));
    // Boxed: the handshake future (TCP, TLS, HTTP upgrade) is about 20 KB,
    // and every caller would otherwise carry it inline in its own future.
    match Box::pin(tokio_tungstenite::connect_async_tls_with_config(
        url,
        Some(config),
        true,
        Some(tls.connector()),
    ))
    .await
    {
        Ok((socket, _response)) => Ok(socket),
        Err(Error::Http(response)) => Err(DialError::Http {
            status: response.status().as_u16(),
        }),
        Err(error) => Err(DialError::Connect(io_error(error))),
    }
}

/// A websocket error as an I/O error: I/O failures as they are, the rest
/// wrapped.
pub(crate) fn io_error(error: Error) -> io::Error {
    if let Error::Io(error) = error {
        return error;
    }
    if matches!(error, Error::ConnectionClosed | Error::AlreadyClosed) {
        return io::ErrorKind::BrokenPipe.into();
    }
    io::Error::other(error)
}
