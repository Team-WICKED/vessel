//! What a command can fail with; printed once, with its causes, by `main`.

use std::path::PathBuf;

use vessel_client::book::BookError;
use vessel_client::{CallError, ConnectError, InvalidAddress, NodeUrl, PinnedError, StreamError};
use vessel_node::NodeError;
use vessel_remote::{BuildError, InstallError, PlatformError, ReleaseError, SshError};

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
    /// A node on another machine and no `--token`: the local token file is
    /// only ever sent to this machine's node (see `connect`).
    #[error(
        "{0} is not on this machine: pass its token with --token or VESSEL_TOKEN \
         (the local token file is only sent to a node on this machine)"
    )]
    TokenRequired(NodeUrl),
    /// A relative `--cwd` for a node on another machine, where it would mean
    /// nothing.
    #[error(
        "--cwd {} is relative, but {node} is not on this machine: give an absolute path on that machine",
        cwd.display()
    )]
    RelativeCwd { cwd: PathBuf, node: String },
    /// A book entry's token file could not be read or holds no token.
    #[error("cannot use the token file {}", path.display())]
    TokenFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The node book or a project file.
    #[error(transparent)]
    Book(#[from] BookError),
    /// Text that names no node.
    #[error("`{text}` is not a node address")]
    Address {
        text: String,
        #[source]
        source: InvalidAddress,
    },
    /// A different node answered than the one the book pinned.
    #[error(transparent)]
    Changed(PinnedError<ConnectError>),
    /// A command that needs an SSH node got another kind.
    #[error("{node} is not an SSH node; {what} works only over SSH")]
    NotSsh { node: String, what: &'static str },
    /// Arguments that do not fit together.
    #[error("{0}")]
    Usage(&'static str),
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Release(#[from] ReleaseError),
    #[error(transparent)]
    Build(#[from] BuildError),
    /// SSH nodes are reached through `vessel-remote`'s connector, which
    /// comes with the node side's framed transport (milestone 2, C5).
    #[error("{0} is an SSH node; connecting to SSH nodes is not available in this build yet")]
    SshNotYet(String),
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
