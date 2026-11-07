//! Connecting to a node on this machine through its local socket (ADR
//! 0007): a Unix socket, or a named pipe on Windows. The first frame says
//! who connects: the node's token, or a session agent's credential (ADR
//! 0013, `AgentAuth`). Then the protocol runs as on any transport.
//!
//! [Decision Log]
//! - Purpose: `vessel mcp` runs inside a session and reaches its own node
//!   with its session's credential, which only the local socket accepts.
//! - Alternatives: the websocket with the token (the agent would hold the
//!   machine owner's authority, and a node may have no TCP port); going
//!   through `vesseld attach` (a process per connection, and it presents
//!   the token).
//! - Choice: open the endpoint the node names in the `vessel` server's
//!   environment, send the credential frame, and hand the framed stream to
//!   [`NodeClient::over`]. A refused credential is the handshake's
//!   `Rejected` (`unauthorized`).
//! - The Windows pipe's busy retry repeats `vessel-node`'s (`ipc`), which
//!   this crate cannot depend on.

use std::io;
use std::path::{Path, PathBuf};

use futures_util::SinkExt as _;
use vessel_proto::{AgentAuth, ClientInfo};
use vessel_rpc::{ClientConfig, Framed, FramedError, MAX_FRAME};

use crate::client::NodeClient;
use crate::token::BearerToken;

/// Who connects on the local socket.
#[derive(Debug, Clone)]
pub enum LocalCredential {
    /// The node's token: the machine's owner.
    Token(BearerToken),
    /// A session's agent (ADR 0013).
    Agent(AgentAuth),
}

/// Why a local connection failed.
#[derive(Debug, thiserror::Error)]
pub enum LocalConnectError {
    /// No node answers at the endpoint.
    #[error("cannot reach the node's local socket {}", path.display())]
    Unreachable {
        /// The socket path or pipe name.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The credential could not be encoded or sent.
    #[error("cannot present the credential")]
    Present(#[source] FramedError),
    /// The credential could not be encoded.
    #[error("cannot encode the credential")]
    Encode(#[source] serde_json::Error),
    /// The protocol handshake failed; a refused credential is `Rejected`
    /// with `unauthorized`.
    #[error("the handshake failed")]
    Handshake(#[source] vessel_rpc::ConnectError<FramedError>),
}

impl NodeClient {
    /// Connect to the node whose local endpoint is `endpoint` (a socket
    /// path, or a pipe name on Windows), as `credential`.
    ///
    /// # Errors
    /// See [`LocalConnectError`].
    pub async fn connect_local(
        endpoint: &Path,
        credential: &LocalCredential,
        client: ClientInfo,
    ) -> Result<Self, LocalConnectError> {
        let stream = open(endpoint)
            .await
            .map_err(|source| LocalConnectError::Unreachable {
                path: endpoint.to_owned(),
                source,
            })?;
        let (reader, writer) = tokio::io::split(stream);
        let mut framed = Framed::new(reader, writer, MAX_FRAME);
        let first = match credential {
            LocalCredential::Token(token) => token.expose().to_owned(),
            LocalCredential::Agent(auth) => {
                serde_json::to_string(auth).map_err(LocalConnectError::Encode)?
            }
        };
        framed
            .send(first)
            .await
            .map_err(LocalConnectError::Present)?;
        Self::over(framed, client, &ClientConfig::default())
            .await
            .map_err(LocalConnectError::Handshake)
    }
}

#[cfg(unix)]
async fn open(endpoint: &Path) -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(endpoint).await
}

/// Open the pipe, retrying `ERROR_PIPE_BUSY` for about a second: between a
/// client taking the waiting instance and the node creating the next, every
/// instance is busy.
#[cfg(windows)]
async fn open(endpoint: &Path) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::ClientOptions;
    /// `winerror.h`; not worth a `windows-sys` dependency for one number.
    const ERROR_PIPE_BUSY: i32 = 231;
    const ATTEMPTS: u32 = 50;
    const PAUSE: Duration = Duration::from_millis(20);
    for _ in 0..ATTEMPTS {
        match ClientOptions::new().open(endpoint) {
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(PAUSE).await;
            }
            opened => return opened,
        }
    }
    ClientOptions::new().open(endpoint)
}
