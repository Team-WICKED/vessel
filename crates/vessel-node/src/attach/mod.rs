//! `vesseld attach`: the per-connection proxy that SSH runs (ADR 0007).
//!
//! It connects to this machine's node through the local socket, presents
//! the node's token as the first frame, and then copies bytes between its
//! own stdin/stdout and the socket without decoding them. If no node
//! answers, it starts one in the background first. Its exit status is a
//! [`ProxyExit`], the client's only side channel once the stream is gone.
//!
//! [Decision Log]
//! - Purpose: a client with nothing but SSH access reaches the node, which
//!   needs no open port and may not even be running yet (coordinator
//!   decision 2: a new machine is productive in minutes).
//! - Starting the node: under a start lock, so racing proxies start one
//!   node, and through a login shell on Unix, because SSH's non-interactive
//!   commands get a minimal `PATH` without the user's harness installs
//!   (`~/.local/bin`, `nvm`, Homebrew). The node detaches into its own
//!   process group with stdin from `/dev/null` and its log in
//!   `logs/vesseld.log`, and announces readiness with a marker line on a
//!   pipe; the proxy waits for that line instead of polling for a socket.
//! - Bytes, not frames: both sides use the same framing (PROTOCOL.md
//!   version 3), so the proxy decodes only the node's first frame, to tell a
//!   refused token (exit 82) from a working connection.
//! - Stdin is read on a plain thread: a blocking read on the runtime's pool
//!   would keep the process from exiting when the node side ends first.

mod bridge;
mod start;

use std::io;
use std::path::PathBuf;

use vessel_proto::ProxyExit;
use vessel_store::{DataDir, NodeToken, StoreError};

use crate::ipc::{self, Endpoint};
use bridge::bridge;
use start::start_node;

/// The line a node started by `attach` prints on stdout once it serves,
/// followed by its pid.
pub const READY: &str = "vesseld-ready";

/// Why `attach` could not bridge a connection.
#[derive(Debug, thiserror::Error)]
enum AttachError {
    #[error("the data directory is unusable")]
    DataDir(#[source] StoreError),
    #[error("cannot read the node's token")]
    Token(#[source] StoreError),
    #[error("cannot reach the node at {}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot start the node")]
    Spawn(#[source] io::Error),
    #[error("the node did not start; its log ({}) ends with:\n{tail}", log.display())]
    NotReady { log: PathBuf, tail: String },
}

impl AttachError {
    const fn exit(&self) -> ProxyExit {
        match self {
            Self::DataDir(_) | Self::Token(_) => ProxyExit::DataDirUnusable,
            Self::Connect { .. } | Self::Spawn(_) | Self::NotReady { .. } => {
                ProxyExit::NodeUnavailable
            }
        }
    }
}

/// Run the proxy for the node of `dir`; returns how it ended.
pub async fn run(dir: DataDir) -> ProxyExit {
    match attach(&dir).await {
        Ok(exit) => exit,
        Err(error) => {
            tracing::error!(error = %chain(&error), "vesseld attach failed");
            error.exit()
        }
    }
}

async fn attach(dir: &DataDir) -> Result<ProxyExit, AttachError> {
    let stream = match connect(dir).await {
        Ok(stream) => stream,
        Err(Unreachable::NoNode) => {
            start_node(dir).await?;
            connect(dir)
                .await
                .map_err(|unreachable| unreachable.into_error(dir))?
        }
        Err(other) => return Err(other.into_error(dir)),
    };
    let token = read_token(dir).await?;
    Ok(bridge(stream, &token).await)
}

/// Why the local socket could not be reached.
enum Unreachable {
    /// No node is serving it (no socket, a stale one, or no node yet).
    NoNode,
    /// Something else went wrong.
    Failed(AttachError),
}

impl Unreachable {
    fn into_error(self, dir: &DataDir) -> AttachError {
        match self {
            Self::NoNode => AttachError::Connect {
                path: dir.socket_path(),
                source: io::ErrorKind::NotFound.into(),
            },
            Self::Failed(error) => error,
        }
    }
}

async fn connect(dir: &DataDir) -> Result<ipc::LocalClient, Unreachable> {
    let lookup = dir.clone();
    let endpoint = tokio::task::spawn_blocking(move || Endpoint::find(&lookup))
        .await
        .map_err(|error| Unreachable::Failed(AttachError::Spawn(io::Error::other(error))))?
        .map_err(|error| Unreachable::Failed(AttachError::DataDir(error)))?;
    let Some(endpoint) = endpoint else {
        return Err(Unreachable::NoNode);
    };
    match ipc::connect(&endpoint).await {
        Ok(stream) => Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(Unreachable::NoNode)
        }
        Err(source) => Err(Unreachable::Failed(AttachError::Connect {
            path: endpoint.path().to_path_buf(),
            source,
        })),
    }
}

async fn read_token(dir: &DataDir) -> Result<NodeToken, AttachError> {
    let dir = dir.clone();
    tokio::task::spawn_blocking(move || dir.read_token())
        .await
        .map_err(|error| AttachError::Spawn(io::Error::other(error)))?
        .map_err(AttachError::Token)
}

/// An error and its causes on one line.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
