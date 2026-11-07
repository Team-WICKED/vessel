//! Admission starts at TCP accept, not after headers or websocket hello.
//! Counted I/O keeps permits through Hyper's upgrade, so an upgraded or
//! silent socket cannot disappear from the overall connection budget.
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::{Extension, Router};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// A socket owns these until its actual I/O is dropped, including upgrades.
#[derive(Debug)]
pub(crate) struct Connection {
    _total: OwnedSemaphorePermit,
    pending: Mutex<Option<OwnedSemaphorePermit>>,
}
impl Connection {
    pub(crate) fn authenticated(&self) {
        let _permit = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    total: usize,
    pending: usize,
    header_timeout: Duration,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let total = Arc::new(Semaphore::new(total));
    let pending = Arc::new(Semaphore::new(pending));
    let mut tasks = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            Some(_done) = tasks.join_next(), if !tasks.is_empty() => continue,
            accepted = listener.accept() => accepted?,
        };
        let (stream, _) = accepted;
        let Ok(total) = Arc::clone(&total).try_acquire_owned() else {
            continue;
        };
        let Ok(pending) = Arc::clone(&pending).try_acquire_owned() else {
            continue;
        };
        let place = Arc::new(Connection {
            _total: total,
            pending: Mutex::new(Some(pending)),
        });
        let app = app.clone().layer(Extension(Arc::clone(&place)));
        let io = Counted {
            stream,
            _place: place,
        };
        drop(tasks.spawn(async move {
            let mut http = http1::Builder::new();
            // The guard closes ordinary HTTP responses; upgrades keep their
            // Connection: upgrade header intact.
            let _configured = http
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout)
                .max_buf_size(16 * 1024);
            let _served = http
                .serve_connection(TokioIo::new(io), TowerToHyperService::new(app))
                .with_upgrades()
                .await;
        }));
    }
}

struct Counted {
    stream: TcpStream,
    _place: Arc<Connection>,
}
impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for Counted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
