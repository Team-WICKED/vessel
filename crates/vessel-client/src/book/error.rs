//! Why the book, a project file, or a choice of node is unusable.

use std::path::PathBuf;

use vessel_proto::NodeId;

use super::{InvalidNodeName, NodeName};
use crate::address::{InvalidAddress, InvalidRemotePath, InvalidSshTarget};

/// Why the book, a project file, or a choice of node is unusable.
#[derive(Debug, thiserror::Error)]
pub enum BookError {
    /// The file could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// Not TOML, or a key this file may not have (the message has the
    /// line).
    #[error("{}: {message}", path.display())]
    Syntax {
        /// Which file.
        path: PathBuf,
        /// What, where.
        message: String,
    },
    /// An entry that does not describe a node.
    #[error("{}: node `{name}`: {problem}", path.display())]
    Entry {
        /// Which file.
        path: PathBuf,
        /// Which entry.
        name: String,
        /// What is wrong.
        problem: EntryProblem,
    },
    /// `default_node` names a node the user's book does not have.
    #[error(
        "{}: default_node `{name}` is not a node in your book ({})",
        path.display(),
        book.display()
    )]
    UnknownDefault {
        /// The file that says so (the book itself, or a project file).
        path: PathBuf,
        /// The name.
        name: String,
        /// The user's book.
        book: PathBuf,
    },
    /// `--node` / `VESSEL_NODE` is neither a name in the book nor an
    /// address.
    #[error("`{text}` is not a node in your book, nor an address: {source}")]
    Unknown {
        /// What was given.
        text: String,
        /// Why it is not an address.
        #[source]
        source: InvalidAddress,
    },
    /// `vessel nodes add` for a name the book already has.
    #[error("{}: there is already a node `{name}`", path.display())]
    Exists {
        /// The book.
        path: PathBuf,
        /// The name.
        name: NodeName,
    },
    /// The book has no such node.
    #[error("{}: no node `{name}`", path.display())]
    Missing {
        /// The book.
        path: PathBuf,
        /// The name.
        name: String,
    },
    /// Pinning an id on a node that has a different one.
    #[error("{}: node `{name}` is pinned to id {pinned}, not {found}", path.display())]
    PinnedElsewhere {
        /// The book.
        path: PathBuf,
        /// The node.
        name: NodeName,
        /// The id in the book.
        pinned: NodeId,
        /// The id offered.
        found: NodeId,
    },
}

/// What is wrong with one entry of the book.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntryProblem {
    /// The key is not a usable name.
    #[error(transparent)]
    Name(#[from] InvalidNodeName),
    /// None of `ssh`, `url` and `key`.
    #[error(
        "needs `ssh = \"[user@]host\"`, `url = \"ws://...\"`, or a paired node's `key` \
         (`vessel nodes add NAME --pair LINK` writes one)"
    )]
    NoTransport,
    /// More than one of `ssh`, `url` and `key`.
    #[error("has more than one of `ssh`, `url` and `key`; a node is reached one way")]
    ManyTransports,
    /// A paired node without its credential file.
    #[error("a `key` node needs `credential = {{ file = \"...\" }}`, written when it was paired")]
    NoCredential,
    /// A paired node without a route.
    #[error("a `key` node needs at least one `relay` or `direct` route")]
    NoRoute,
    /// `credential`, `relay` or `direct` on a node that is not paired.
    #[error("`credential`, `relay` and `direct` belong to a paired node's `key`")]
    SecureFieldsWithoutKey,
    /// A token or remote data directory on a paired node.
    #[error(
        "a `key` node authenticates with its device credential; it has no `token` or `data_dir`"
    )]
    WsFieldsWithKey,
    /// `key` is not a node key.
    #[error("key: {0}")]
    Key(String),
    /// A route that is not a `ws://` or `wss://` URL.
    #[error("route: {0}")]
    Route(String),
    /// A credential file path that is not UTF-8 (cannot be written to TOML).
    #[error("credential: the file path is not UTF-8")]
    CredentialPath,
    /// A token for an SSH node.
    #[error("an SSH node needs no `token`: the node reads its own (ADR 0007)")]
    TokenOverSsh,
    /// A remote data directory for a websocket node.
    #[error("`data_dir` is for SSH nodes; a websocket node runs its own")]
    DataDirOverWs,
    /// `ssh` is not a usable target.
    #[error("ssh: {0}")]
    Ssh(#[from] InvalidSshTarget),
    /// `url` is not a websocket URL.
    #[error("url: {0}")]
    Url(String),
    /// `data_dir` is not a remote path.
    #[error("data_dir: {0}")]
    DataDir(#[from] InvalidRemotePath),
    /// `id` is not a node id.
    #[error("id: `{0}` is not a node id")]
    Id(String),
    /// A token file path that is not UTF-8 (cannot be written to TOML).
    #[error("token: the file path is not UTF-8")]
    TokenPath,
}
