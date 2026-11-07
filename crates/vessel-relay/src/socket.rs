//! Opening a websocket to a relay, as a node or a client.

use std::io;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use vessel_secure::MAX_RECORD;

use crate::dial::DialError;
use crate::reach::{Reach, is_public};
use crate::tls::TlsConfig;

/// A websocket to a relay.
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to `url` (`ws://` or `wss://`). Messages from the relay are
/// bounded by one record, checked from each frame's header before its
/// payload is read.
pub(crate) async fn open(url: &str, tls: &TlsConfig) -> Result<Socket, DialError> {
    open_reaching(url, tls, Reach::Any).await
}

/// [`open`], connecting only to addresses `reach` allows: for
/// [`Reach::Public`], the host is resolved here and only its public
/// addresses are tried, so the connection goes to an address that was
/// checked.
pub(crate) async fn open_reaching(
    url: &str,
    tls: &TlsConfig,
    reach: Reach,
) -> Result<Socket, DialError> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RECORD))
        .max_frame_size(Some(MAX_RECORD));
    if reach == Reach::Public {
        let request = url
            .into_client_request()
            .map_err(|error| DialError::Connect(io_error(error)))?;
        let stream = public_stream(request.uri()).await?;
        return match Box::pin(tokio_tungstenite::client_async_tls_with_config(
            request,
            stream,
            Some(config),
            Some(tls.connector()),
        ))
        .await
        {
            Ok((socket, _response)) => Ok(socket),
            Err(Error::Http(response)) => Err(DialError::Http {
                status: response.status().as_u16(),
            }),
            Err(error) => Err(DialError::Connect(io_error(error))),
        };
    }
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

/// A TCP connection to one of the public addresses `uri`'s host resolves
/// to, tried in the resolver's order.
async fn public_stream(
    uri: &tokio_tungstenite::tungstenite::http::Uri,
) -> Result<TcpStream, DialError> {
    let refused = |why: &str| {
        DialError::Connect(io::Error::new(
            io::ErrorKind::PermissionDenied,
            why.to_owned(),
        ))
    };
    let host = uri
        .host()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .ok_or_else(|| refused("the relay's URL names no host"))?;
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") => 443,
        _ => 80,
    });
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(DialError::Connect)?;
    let mut last = None;
    let mut any = false;
    for address in resolved.filter(|address| is_public(address.ip())) {
        any = true;
        match TcpStream::connect(address).await {
            Ok(stream) => {
                let _nagle = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    if !any {
        return Err(refused(
            "the relay's host resolves to no public address; only public ones are dialed for it",
        ));
    }
    Err(DialError::Connect(last.unwrap_or_else(|| {
        io::Error::from(io::ErrorKind::NotConnected)
    })))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_public_only_dial_never_reaches_this_machine() {
        let tls = TlsConfig::with_roots(rustls::RootCertStore::empty()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for url in [
            format!("ws://127.0.0.1:{port}"),
            format!("ws://localhost:{port}"),
            format!("ws://[::ffff:127.0.0.1]:{port}"),
        ] {
            let refused = open_reaching(&url, &tls, Reach::Public).await;
            let Err(DialError::Connect(error)) = refused else {
                panic!("{url} was dialed");
            };
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{url}");
        }
        // The same relay is dialed when the node's owner chose it.
        let accepted = tokio::spawn(async move { listener.accept().await.is_ok() });
        let _any = open_reaching(&format!("ws://127.0.0.1:{port}"), &tls, Reach::Any).await;
        assert!(accepted.await.unwrap());
    }
}
