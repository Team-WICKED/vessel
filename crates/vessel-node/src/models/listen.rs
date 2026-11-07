//! The model proxy's accept loop: a bounded number of connections, a
//! deadline on each request's headers, and closes that do not reset what
//! the harness has not read yet.
//!
//! [Decision Log] (the security review of milestone 5, finding 4)
//! - Problem: under `axum::serve` nothing bounded the proxy's connections
//!   or how long one took to send its headers (axum sets no timer, so
//!   hyper waits for good), and the handlers took the body as `Bytes`, so
//!   a whole body was read before any check: an unauthenticated local
//!   process could hold any number of connections, each up to the body
//!   cap, outside both call limits.
//! - Choice: the relay's pattern (`vessel-relay`'s `server::accept`), cut
//!   down to loopback: our own loop over `hyper`'s HTTP/1 connection with
//!   a timer and [`HEADER_READ`]; a socket takes one of
//!   [`MAX_CONNECTIONS`] places before any byte is read, or is answered
//!   `503` and closed. The handlers check every header before they read
//!   the body (`proxy::admit`), and read it under a cap and a deadline.
//! - A refused request's body is never read by the handler, so hyper
//!   closes the connection with it unread; closing a socket with unread
//!   bytes resets it, and a reset can throw away the refusal before the
//!   harness reads it. So a socket is not dropped at once ([`linger`]):
//!   its write side is shut (a FIN behind the answer), and what the peer
//!   still sends is read and discarded until it closes too, for at most
//!   [`LINGER`], holding its place meanwhile.
//! - Alternatives: `axum::serve` with a `Listener` wrapper (no header
//!   deadline); a timeout around each connection's whole life (cuts long
//!   streamed answers).
//! - Cost: HTTP/2 is not served; Claude Code and Codex speak HTTP/1.1 to
//!   a plain `http://` base URL.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

/// Connections the proxy holds at once. Each routed harness keeps a few
/// (a pool of idle ones, the calls in flight); the call limits
/// (`super::PER_NODE`) bound what they do.
pub(crate) const MAX_CONNECTIONS: usize = 256;

/// How long a connection may take to send a request's headers.
pub(crate) const HEADER_READ: Duration = Duration::from_secs(10);

/// How long a closed socket is read before it is dropped ([`linger`]).
pub(crate) const LINGER: Duration = Duration::from_secs(1);

/// How long the proxy lets its connections finish when the node stops.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// What a connection past [`MAX_CONNECTIONS`] is told.
const REFUSED: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";

/// Serve `app` on `listener` until `stop`.
pub(crate) async fn run(listener: TcpListener, app: Router, stop: CancellationToken) {
    let places = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            Some(_ended) = connections.join_next(), if !connections.is_empty() => continue,
            () = stop.cancelled() => break,
        };
        let stream = match accepted {
            Ok((stream, _peer)) => stream,
            Err(error) => {
                // Out of descriptors, most likely: they free up as
                // connections end, so wait a moment rather than spin.
                tracing::debug!(%error, "the model proxy could not accept a connection");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(100)) => continue,
                    () = stop.cancelled() => break,
                }
            }
        };
        let Ok(place) = Arc::clone(&places).try_acquire_owned() else {
            let _written = stream.try_write(REFUSED);
            tracing::info!("refused a model proxy connection past the limit");
            continue;
        };
        let _nodelay = stream.set_nodelay(true);
        let io = Held {
            socket: Some((stream, place, stop.clone())),
        };
        let service = TowerToHyperService::new(
            app.clone()
                .map_request(|request: hyper::Request<Incoming>| request.map(Body::new)),
        );
        drop(connections.spawn(serve_connection(io, service, stop.clone())));
    }
    let left = tokio::time::timeout(STOP_GRACE, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if left.is_err() {
        tracing::debug!(
            connections = connections.len(),
            "the model proxy dropped the connections still open after the grace"
        );
    }
    connections.shutdown().await;
}

/// One connection: HTTP/1 with the header deadline, until it ends or the
/// node stops.
async fn serve_connection<S>(io: Held, service: S, stop: CancellationToken)
where
    S: hyper::service::Service<hyper::Request<Incoming>, Response = axum::response::Response>
        + Send,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut builder = http1::Builder::new();
    let _configured = builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ);
    let connection = builder.serve_connection(TokioIo::new(io), service);
    tokio::pin!(connection);
    let served = tokio::select! {
        served = connection.as_mut() => served,
        () = stop.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = served {
        tracing::debug!(%error, "a model proxy connection ended with an error");
    }
}

/// A socket holding its place among [`MAX_CONNECTIONS`], given back after
/// [`linger`].
struct Held {
    /// Taken only when dropped, by [`linger`].
    socket: Option<(TcpStream, OwnedSemaphorePermit, CancellationToken)>,
}

impl Held {
    fn stream(&mut self) -> io::Result<&mut TcpStream> {
        self.socket
            .as_mut()
            .map(|(stream, _place, _stop)| stream)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let Some((stream, place, stop)) = self.socket.take() else {
            return;
        };
        // Outside a runtime (never, here) the socket just drops.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(linger(stream, place, stop)));
        }
    }
}

/// Close `stream` without a reset: shut its write side, then read and
/// discard until the peer closes, at most [`LINGER`] or until the node
/// stops; `place` goes back only then.
async fn linger(mut stream: TcpStream, place: OwnedSemaphorePermit, stop: CancellationToken) {
    let draining = async {
        let _shut = stream.shutdown().await;
        let _discarded = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
    };
    tokio::select! {
        _drained = tokio::time::timeout(LINGER, draining) => {}
        () = stop.cancelled() => {}
    }
    drop(stream);
    drop(place);
}

impl AsyncRead for Held {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(stream).poll_read(cx, buf),
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

impl AsyncWrite for Held {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(stream).poll_write(cx, buf),
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.socket
            .as_ref()
            .is_some_and(|(stream, _place, _stop)| stream.is_write_vectored())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(stream).poll_flush(cx),
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(stream).poll_shutdown(cx),
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}
