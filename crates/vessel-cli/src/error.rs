//! What a command can fail with; printed once, with its causes, by `main`.

use std::path::PathBuf;

use vessel_client::{CallError, ConnectError, StreamError};
use vessel_node::NodeError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// No `--token`, and the local token file could not be read.
    #[error(
        "no token: pass --token (or VESSEL_TOKEN), or run on the node's machine ({} could not be read)",
        path.display()
    )]
    Token {
        path: PathBuf,
        #[source]
        source: vessel_node::StoreError,
    },
    /// The local token file holds something a client cannot send.
    #[error("the token in {} cannot be sent as a bearer token", .0.display())]
    UnusableToken(PathBuf),
    /// No home directory to find the default data directory in.
    #[error("no home directory; pass --data-dir")]
    NoHome,
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Call(#[from] CallError),
    #[error(transparent)]
    Stream(#[from] StreamError),
    #[error("no session matches `{0}`")]
    NoSuchSession(String),
    #[error("`{prefix}` matches {matches} sessions; type more of the id")]
    AmbiguousSession { prefix: String, matches: usize },
    #[error(transparent)]
    Node(#[from] NodeError),
    #[error("terminal I/O failed")]
    Io(#[from] std::io::Error),
    #[error("an input task failed")]
    Task(#[from] tokio::task::JoinError),
}
