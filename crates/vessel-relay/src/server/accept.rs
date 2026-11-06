//! The relay's accept loop: every connection counted from `accept`, HTTP/1
//! with a deadline on the request's headers, and a stop that waits a while
//! and then drops what is left.
//!
//! [Decision Log] (the security review, round 7)
//! - Problem: under `axum::serve` a connection that sent half a request
//!   stayed open for good (hyper reads headers without a deadline unless a
//!   timer is set, and axum sets none), the per-address limits applied only
//!   once a request reached a handler, and a graceful stop waited for every
//!   such connection. One host could hold all of a relay's descriptors and
//!   keep it from stopping.
//! - Choice: our own loop over `hyper`'s HTTP/1 connection with a timer and
//!   `header_read_timeout`, the router as its service (`ConnectInfo` added
//!   per connection, as axum does). Before any byte is read, a connection
//!   takes a place in all (`max_connections`) and one for its address
//!   (`max_connections_per_address`, an IPv4 address or an IPv6 `/64`);
//!   without one it gets `503` and is closed. The place goes with the
//!   socket (`Counted`), through a websocket upgrade, until the socket
//!   closes. Stopping: no more accepts, every connection told to finish,
//!   `shutdown_grace` for them, then the rest are dropped.
//! - Behind a reverse proxy (`client_address_header`) every connection
//!   comes from the proxy, so the per-address count from `accept` is not
//!   applied (the proxy's own limits stand in for it); the counts at the
//!   handlers use the proxy's header.
//! - Alternatives: a timeout around each connection's whole life (cuts
//!   long websockets); `hyper-util`'s auto builder (HTTP/2 too, which the
//!   relay does not serve).
//! - Cost: HTTP/2 is not served (it never was through a proxy that speaks
//!   HTTP/1 to the relay, and websockets here are HTTP/1 upgrades).

use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use super::Relay;
use super::throttle::Refusal;

/// What a connection refused before any HTTP is told.
const REFUSED: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";

/// Serve `app` on `listener` until `shutdown`, then stop as the module
/// says.
pub(super) async fn run(
    listener: TcpListener,
    app: Router,
    relay: Arc<Relay>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            Some(_ended) = connections.join_next(), if !connections.is_empty() => continue,
            () = shutdown.cancelled() => break,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) if is_connection_error(&error) => continue,
            Err(error) => {
                // Out of descriptors, most likely: they free up as
                // connections end, so wait a moment rather than spin.
                tracing::warn!(%error, "accepting a connection failed");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(100)) => continue,
                    () = shutdown.cancelled() => break,
                }
            }
        };
        let Some(place) = relay.connection(peer.ip()) else {
            // Best effort and never waiting: a client that reads learns
            // why, and nothing more is spent on it.
            let _written = stream.try_write(REFUSED);
            continue;
        };
        let _nodelay = stream.set_nodelay(true);
        let io = Counted {
            stream,
            _place: place,
        };
        let service = TowerToHyperService::new(app.clone().map_request(
            move |mut request: hyper::Request<Incoming>| {
                let _previous = request.extensions_mut().insert(ConnectInfo(peer));
                request
            },
        ));
        drop(connections.spawn(serve_connection(
            io,
            service,
            relay.limits.header_read_timeout,
            shutdown.clone(),
        )));
    }
    let left = tokio::time::timeout(relay.limits.shutdown_grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if left.is_err() {
        tracing::info!(
            connections = connections.len(),
            "stopping: dropped the connections still open after the grace"
        );
    }
    connections.shutdown().await;
    Ok(())
}

/// Errors of one connection, not of the listener.
fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// One connection: HTTP/1 with the header deadline and upgrades, until it
/// ends, or until `shutdown` asks it to finish what it is doing.
async fn serve_connection<S>(
    io: Counted,
    service: S,
    header_read_timeout: Duration,
    shutdown: CancellationToken,
) where
    S: hyper::service::Service<hyper::Request<Incoming>, Response = axum::response::Response>
        + Send,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut builder = http1::Builder::new();
    let _configured = builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let connection = builder
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::pin!(connection);
    let served = tokio::select! {
        served = connection.as_mut() => served,
        () = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = served {
        tracing::debug!(%error, "a connection ended with an error");
    }
}

/// A connection's places in the relay's counts, given back when the socket
/// closes (`Relay::connection`).
pub(crate) struct ConnectionPlace {
    pub(super) relay: Arc<Relay>,
    /// The address counted, when counted by address.
    pub(super) address: Option<IpAddr>,
}

impl Drop for ConnectionPlace {
    fn drop(&mut self) {
        self.relay.release_connection(self.address);
    }
}

/// A socket holding its place: it travels with the socket through hyper and
/// a websocket upgrade, so the place is held exactly as long as the socket.
struct Counted {
    stream: TcpStream,
    _place: ConnectionPlace,
}

impl AsyncRead for Counted {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// The address a peer is counted under from `accept`, if any.
pub(super) fn counted_address(relay: &Relay, peer: IpAddr) -> Option<IpAddr> {
    relay
        .client_address_header
        .is_none()
        .then(|| super::rate::bucket_key(peer))
}

/// Which limit refused a connection at `accept`.
pub(super) const fn refusal(by_address: bool) -> Refusal {
    if by_address {
        Refusal::AddressConnections
    } else {
        Refusal::Connections
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[test]
    fn only_a_listeners_own_failures_stop_it_for_a_moment() {
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
        ] {
            assert!(is_connection_error(&io::Error::from(kind)));
        }
        assert!(!is_connection_error(&io::Error::other(
            "too many open files"
        )));
    }

    #[test]
    fn a_refusal_is_a_whole_http_answer() {
        let text = std::str::from_utf8(REFUSED).unwrap();
        assert!(text.starts_with("HTTP/1.1 503 "));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn peers_are_counted_by_their_block() {
        let peer: SocketAddr = "[2001:db8:1:2:3:4:5:6]:7".parse().unwrap();
        assert_eq!(
            super::super::rate::bucket_key(peer.ip()),
            "2001:db8:1:2::".parse::<IpAddr>().unwrap()
        );
    }
}
