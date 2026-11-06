//! The node's local socket (ADR 0007): how `vesseld attach` reaches the
//! node on its own machine.
//!
//! A Unix socket at `<data_dir>/vesseld.sock` (0600, inside the 0700 data
//! directory), or on Windows a named pipe named after the node id. Frames
//! are `vessel-rpc`'s `Framed` (PROTOCOL.md version 3). The first frame of
//! every connection must be the node's token; a wrong or missing one is
//! answered with `rejected` (`unauthorized`) and the connection closes
//! before any call is served. After that, the connection is served exactly
//! like a websocket one, by the same `vessel-rpc` server and limits.
//!
//! [Decision Log]
//! - Purpose: SSH reaches a node through a proxy on the node's machine;
//!   that proxy needs a local endpoint which needs no TCP port, so a node
//!   can run with no listening port at all (`--bind none`).
//! - Why a token on a local socket: ADR 0005 keeps one rule for every
//!   connection. The socket's file mode already keeps other users out on
//!   Unix, but a Windows pipe's default ACL lets other local users open it,
//!   and checking peer credentials portably would need `unsafe` FFI. The
//!   proxy runs as the node's user and reads the token file, which is
//!   exactly the capability the token stands for.
//! - The answer to a bad token is the protocol's own `rejected` frame, so
//!   both the proxy (to exit with `ProxyExit::Refused`) and the client
//!   behind it (as a refused handshake) understand it.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_proto::{ErrorCode, NodeId, ServerFrame, WireError};
use vessel_rpc::{Framed, MAX_FRAME, Server};
use vessel_store::{DataDir, NodeToken};

use crate::service::{Connection, NodeService};

/// How long a new local connection has to present the token.
const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait after a failed `accept` (for example when the process
/// is out of file descriptors) before accepting again, so a lasting error
/// does not spin a core.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// Where the local socket is: a file path on Unix, a pipe name on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint(PathBuf);

impl Endpoint {
    /// The local endpoint of the node of `dir`, whose id is `node`.
    #[cfg(unix)]
    #[must_use]
    pub(crate) fn of(dir: &DataDir, _node: NodeId) -> Self {
        Self(dir.socket_path())
    }

    /// The local endpoint of the node of `dir`, whose id is `node`.
    #[cfg(windows)]
    #[must_use]
    pub(crate) fn of(_dir: &DataDir, node: NodeId) -> Self {
        Self(PathBuf::from(format!(r"\\.\pipe\vessel-node-{node}")))
    }

    /// The local endpoint of whatever node serves `dir`, for a client of
    /// it (`vesseld attach`); `None` when no node has ever run there (on
    /// Windows the pipe name needs the node id). Blocking.
    #[cfg_attr(
        unix,
        expect(
            clippy::unnecessary_wraps,
            reason = "Windows reads the node id, which can fail"
        )
    )]
    pub(crate) fn find(dir: &DataDir) -> Result<Option<Self>, vessel_store::StoreError> {
        #[cfg(unix)]
        {
            Ok(Some(Self(dir.socket_path())))
        }
        #[cfg(windows)]
        {
            Ok(dir.read_node_id()?.map(|node| Self::of(dir, node)))
        }
    }

    /// The socket path or pipe name.
    #[must_use]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

/// The client end of a local connection.
#[cfg(unix)]
pub(crate) type LocalClient = tokio::net::UnixStream;
/// The client end of a local connection.
#[cfg(windows)]
pub(crate) type LocalClient = tokio::net::windows::named_pipe::NamedPipeClient;

/// Connect to a node's local endpoint.
#[cfg(unix)]
pub(crate) async fn connect(endpoint: &Endpoint) -> io::Result<LocalClient> {
    tokio::net::UnixStream::connect(endpoint.path()).await
}

/// Connect to a node's local endpoint.
#[cfg(windows)]
pub(crate) async fn connect(endpoint: &Endpoint) -> io::Result<LocalClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint.path())
}

/// A connected local stream.
#[cfg(unix)]
pub(crate) type LocalStream = tokio::net::UnixStream;
/// A connected local stream.
#[cfg(windows)]
pub(crate) type LocalStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// The bound local endpoint.
#[derive(Debug)]
pub(crate) struct LocalListener {
    endpoint: Endpoint,
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    /// The pipe instance the next client will connect to.
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl LocalListener {
    /// Bind `endpoint`. The caller holds the node lock, so a socket file
    /// left by an earlier node is stale and is replaced.
    #[cfg(unix)]
    pub(crate) fn bind(endpoint: Endpoint) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        match std::fs::remove_file(endpoint.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = tokio::net::UnixListener::bind(endpoint.path())?;
        // The directory is 0700 already; the socket itself is owner-only
        // too, in case the directory is ever opened up.
        std::fs::set_permissions(endpoint.path(), std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { endpoint, listener })
    }

    /// Create the first pipe instance. `first_pipe_instance` fails if
    /// another process already serves this name.
    #[cfg(windows)]
    pub(crate) fn bind(endpoint: Endpoint) -> io::Result<Self> {
        let next = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(endpoint.path())?;
        Ok(Self { endpoint, next })
    }

    pub(crate) const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    #[cfg(unix)]
    async fn accept(&mut self) -> io::Result<LocalStream> {
        self.listener
            .accept()
            .await
            .map(|(stream, _address)| stream)
    }

    /// Wait for a client on the current instance, then create the next one
    /// before handing this one out, so a client never finds no instance.
    #[cfg(windows)]
    async fn accept(&mut self) -> io::Result<LocalStream> {
        self.next.connect().await?;
        let fresh =
            tokio::net::windows::named_pipe::ServerOptions::new().create(self.endpoint.path())?;
        Ok(std::mem::replace(&mut self.next, fresh))
    }
}

/// What every local connection needs.
#[derive(Debug, Clone)]
pub(crate) struct LocalGateway {
    pub(crate) rpc: Arc<Server<NodeService>>,
    pub(crate) token: Arc<NodeToken>,
    pub(crate) connections: TaskTracker,
}

/// Accept local connections until `stop` is cancelled.
pub(crate) async fn serve(
    mut listener: LocalListener,
    gateway: LocalGateway,
    stop: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            () = stop.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok(stream) => {
                drop(
                    gateway
                        .connections
                        .spawn(connection(stream, gateway.clone())),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "cannot accept a local connection");
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
    #[cfg(unix)]
    if let Err(error) = std::fs::remove_file(listener.endpoint().path()) {
        tracing::debug!(%error, "local socket already gone");
    }
}

/// One local connection: the token preamble, then the protocol.
async fn connection<S>(stream: S, gateway: LocalGateway)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut framed = Framed::new(reader, writer, MAX_FRAME);
    let presented = tokio::time::timeout(PREAMBLE_TIMEOUT, framed.next()).await;
    let accepted = matches!(&presented, Ok(Some(Ok(token))) if gateway.token.verify(token));
    if !accepted {
        tracing::info!("refused a local connection without the right token");
        let rejected = ServerFrame::Rejected {
            error: WireError::new(ErrorCode::Unauthorized, "missing or wrong token"),
        };
        // Best effort: the peer may already be gone.
        if let Ok(text) = serde_json::to_string(&rejected)
            && framed.send(text).await.is_err()
        {
            tracing::debug!("refused local peer left before the answer");
        }
        return;
    }
    if let Err(error) = gateway.rpc.serve(framed, Connection::new).await {
        tracing::info!(error = %error, "local connection ended abnormally");
    }
}
