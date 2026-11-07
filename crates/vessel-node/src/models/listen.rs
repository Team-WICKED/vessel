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
//!
//! [Decision Log] (the verification round on the review's fixes)
//! - Problem: 256 local connections, each sending a request without a
//!   token every few seconds, held every place: a 401 kept its connection
//!   alive, so a refused peer stayed as long as it liked and the real
//!   harness got 503. And a connection that stopped halfway through a
//!   request's head was dropped at the deadline with no answer at all
//!   (hyper sends none).
//! - Choice: a refusal closes its connection (`Connection: close`, set by
//!   `proxy::refuse`). A connection is *unproven* until one of its
//!   requests shows a live session's token (`proxy::admit` calls
//!   [`Proof::prove`]); when every place is held, the oldest unproven
//!   connection is closed to make room for the new one ([`Unproven`]),
//!   and only when all are proven is the new one refused. A harness's
//!   pooled connections are proven by their first call, so no number of
//!   tokenless peers closes them. The header deadline is also the idle
//!   keep-alive timeout (hyper starts it whenever a connection waits for
//!   a request's head); a connection that timed out with part of a head
//!   read is answered 408 before it closes ([`TIMED_OUT`]), an idle one
//!   is just closed.
//! - Alternatives: a smaller cap on unproven connections, or places kept
//!   for proven ones. Every new connection starts unproven, a harness's
//!   too, so a full cap would still shut its next connection out.
//! - Cost: a peer opening connections fast enough can close a harness's
//!   new connection before its first request is read (it takes a burst of
//!   every place in that moment); the harness reconnects, and its proven
//!   connections stay.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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

/// How long the accept loop waits for a closed connection's place.
const EVICTION: Duration = Duration::from_millis(200);

/// What a connection past [`MAX_CONNECTIONS`] is told when every place is
/// held by a proven connection.
const REFUSED: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";

/// What a connection that sent part of a request's head and then nothing
/// for [`HEADER_READ`] is told.
pub(crate) const TIMED_OUT: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";

/// The connections that have not yet shown a session's token, oldest
/// first: the ones closed to make room.
#[derive(Default)]
struct Unproven {
    queue: Mutex<VecDeque<(u64, CancellationToken)>>,
}

impl Unproven {
    fn lock(&self) -> MutexGuard<'_, VecDeque<(u64, CancellationToken)>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn forget(&self, id: u64) {
        self.lock().retain(|(entry, _close)| *entry != id);
    }

    /// Close the oldest unproven connection; whether there was one.
    fn evict(&self) -> bool {
        let oldest = self.lock().pop_front();
        oldest.is_some_and(|(_id, close)| {
            close.cancel();
            true
        })
    }
}

/// In every request's extensions: the proxy proves the connection once a
/// request on it shows a live session's token.
#[derive(Clone)]
pub(crate) struct Proof {
    id: u64,
    unproven: Arc<Unproven>,
}

impl Proof {
    /// The connection is a harness's: it is never closed to make room.
    pub(crate) fn prove(&self) {
        self.unproven.forget(self.id);
    }
}

/// What a connection's socket knows of its requests, for the answer to a
/// head that never ended.
#[derive(Default)]
struct Progress {
    /// Bytes have been read since the last byte was written: a request
    /// has begun and is not answered.
    pending: AtomicBool,
    /// hyper gave up waiting for a request's head.
    timed_out: AtomicBool,
}

/// Serve `app` on `listener` until `stop`.
pub(crate) async fn run(listener: TcpListener, app: Router, stop: CancellationToken) {
    let places = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let unproven = Arc::new(Unproven::default());
    let mut next: u64 = 0;
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
        let place = match Arc::clone(&places).try_acquire_owned() {
            Ok(place) => Some(place),
            Err(_full) => make_room(&places, &unproven).await,
        };
        let Some(place) = place else {
            let _written = stream.try_write(REFUSED);
            tracing::info!("refused a model proxy connection past the limit");
            continue;
        };
        let _nodelay = stream.set_nodelay(true);
        let id = next;
        next = next.wrapping_add(1);
        // Closes this connection alone (to make room), or with the node.
        let close = stop.child_token();
        unproven.lock().push_back((id, close.clone()));
        let progress = Arc::new(Progress::default());
        let io = Held {
            socket: Some(Socket {
                stream,
                place,
                close: close.clone(),
                progress: Arc::clone(&progress),
                id,
                unproven: Arc::clone(&unproven),
            }),
        };
        let proof = Proof {
            id,
            unproven: Arc::clone(&unproven),
        };
        let service = TowerToHyperService::new(app.clone().map_request(
            move |request: hyper::Request<Incoming>| {
                let mut request = request.map(Body::new);
                let _first = request.extensions_mut().insert(proof.clone());
                request
            },
        ));
        let connection = serve_connection(io, service, stop.clone(), close, progress);
        drop(connections.spawn(connection));
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

/// Every place is held: close the oldest unproven connection and take its
/// place, or `None` if every connection is proven.
async fn make_room(places: &Arc<Semaphore>, unproven: &Unproven) -> Option<OwnedSemaphorePermit> {
    if !unproven.evict() {
        return None;
    }
    tracing::info!("closed the oldest tokenless model proxy connection to make room");
    tokio::time::timeout(EVICTION, Arc::clone(places).acquire_owned())
        .await
        .ok()?
        .ok()
}

/// One connection: HTTP/1 with the header deadline, until it ends, is
/// closed to make room (`close`), or the node stops.
async fn serve_connection<S>(
    io: Held,
    service: S,
    stop: CancellationToken,
    close: CancellationToken,
    progress: Arc<Progress>,
) where
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
        // `close` is a child of `stop`: a stopping node lets requests in
        // flight finish, a connection closed for room just goes.
        biased;
        () = stop.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
        () = close.cancelled() => return,
        served = connection.as_mut() => served,
    };
    if let Err(error) = served {
        if error.is_timeout() {
            // Read by the socket's linger, once hyper drops it below.
            progress.timed_out.store(true, Ordering::Relaxed);
        }
        tracing::debug!(%error, "a model proxy connection ended with an error");
    }
}

/// A connection's socket and what it holds: its place among
/// [`MAX_CONNECTIONS`] and its entry among the [`Unproven`].
struct Socket {
    stream: TcpStream,
    place: OwnedSemaphorePermit,
    close: CancellationToken,
    progress: Arc<Progress>,
    id: u64,
    unproven: Arc<Unproven>,
}

/// A socket holding its place, given back after [`linger`].
struct Held {
    /// Taken only when dropped, by [`linger`].
    socket: Option<Socket>,
}

impl Held {
    fn socket(&mut self) -> io::Result<&mut Socket> {
        self.socket
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
    }

    /// `polled` wrote bytes: what was read so far is being answered.
    fn answered(&mut self, polled: Poll<io::Result<usize>>) -> Poll<io::Result<usize>> {
        if matches!(polled, Poll::Ready(Ok(written)) if written > 0)
            && let Ok(socket) = self.socket()
        {
            socket.progress.pending.store(false, Ordering::Relaxed);
        }
        polled
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let Some(socket) = self.socket.take() else {
            return;
        };
        // Outside a runtime (never, here) the socket just drops.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(linger(socket)));
        }
    }
}

/// Close a socket without a reset: answer a head cut off by its deadline
/// ([`TIMED_OUT`]), shut the write side, then read and discard until the
/// peer closes, at most [`LINGER`] or until the connection is closed for
/// room or the node stops; its place goes back only then.
async fn linger(socket: Socket) {
    let Socket {
        mut stream,
        place,
        close,
        progress,
        id,
        unproven,
    } = socket;
    let unanswered =
        progress.timed_out.load(Ordering::Relaxed) && progress.pending.load(Ordering::Relaxed);
    let draining = async {
        if unanswered {
            let _written = stream.write_all(TIMED_OUT).await;
        }
        let _shut = stream.shutdown().await;
        let _discarded = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
    };
    tokio::select! {
        _drained = tokio::time::timeout(LINGER, draining) => {}
        () = close.cancelled() => {}
    }
    unproven.forget(id);
    drop(stream);
    drop(place);
}

impl AsyncRead for Held {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let socket = match self.get_mut().socket() {
            Ok(socket) => socket,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let before = buf.filled().len();
        let polled = Pin::new(&mut socket.stream).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            socket.progress.pending.store(true, Ordering::Relaxed);
        }
        polled
    }
}

impl AsyncWrite for Held {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = match this.socket() {
            Ok(socket) => Pin::new(&mut socket.stream).poll_write(cx, buf),
            Err(error) => return Poll::Ready(Err(error)),
        };
        this.answered(polled)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = match this.socket() {
            Ok(socket) => Pin::new(&mut socket.stream).poll_write_vectored(cx, bufs),
            Err(error) => return Poll::Ready(Err(error)),
        };
        this.answered(polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.socket
            .as_ref()
            .is_some_and(|socket| socket.stream.is_write_vectored())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().socket() {
            Ok(socket) => Pin::new(&mut socket.stream).poll_flush(cx),
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().socket() {
            Ok(socket) => Pin::new(&mut socket.stream).poll_shutdown(cx),
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}
