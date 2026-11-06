//! The node book: named nodes, and which node a command talks to.
//!
//! ```toml
//! # <config_dir>/vessel/nodes.toml (the user's book)
//! default_node = "gpu-box"
//!
//! [nodes.gpu-box]
//! ssh = "me@gpu-box"            # or "ssh://me@gpu-box:2222"
//! data_dir = "~/.vessel"        # optional, on that machine
//! id = "6f1c..."                # written on first contact
//!
//! [nodes.lab]
//! url = "ws://10.0.0.5:7077"
//! token = { file = "~/.vessel/tokens/lab" }
//!
//! [nodes.laptop-at-home]          # paired (milestone 3, ADR 0010)
//! key = "x25519:..."              # pinned at pairing; the channel checks it
//! relay = ["wss://relay.example/vessel"]
//! direct = ["ws://203.0.113.5:7077"]
//! credential = { file = "~/.config/vessel/credentials/<id>.secret" }
//! id = "6f1c..."
//! ```
//!
//! ```toml
//! # .vessel/config.toml in a project (or a parent directory)
//! default_node = "gpu-box"      # the only key a project may set
//! ```
//!
//! [Decision Log]
//! - Purpose: `vessel --node gpu-box run claude` instead of a URL and a
//!   token on every command (ORIGIN item 2), and a project that says
//!   which machine it runs on.
//! - Layering (Zed's settings model): every file is read into a "content"
//!   struct whose fields are all optional, and [`resolve`] picks the
//!   highest layer that says something: built-in default (the local node) <
//!   user book < project file < `VESSEL_NODE` < `--node` (the CLI hands the
//!   last two over as one value, since clap already lets the flag win).
//! - Trust: the project file comes with a cloned repository, so it may
//!   only choose among the user's own nodes. It cannot add a node, change
//!   a transport or name a token file, or it could steer commands (and
//!   the user's tokens) to a host of its choosing. Unknown keys in it are
//!   errors, not ignored: a project that tries more is told so.
//! - Format: TOML, which people expect from Rust tools and edit by hand.
//!   `toml_edit` both reads it (typed, through serde, every struct with
//!   `deny_unknown_fields`, so a typo is an error with a line number) and
//!   edits it in place (`vessel nodes add`, the id written on first
//!   contact), keeping the user's comments and layout. One crate instead of
//!   `toml` plus `toml_edit`: `toml` rewrites a file without its comments.
//! - Names are plain words (`[A-Za-z0-9._-]`, starting with a letter or a
//!   digit), so they are bare TOML keys and never look like an address. A
//!   name in the book wins over the same text read as an address
//!   (`gpu-box` is also a valid websocket host name).
//! - The pinned `id` is what makes `ssh gpu-box` safe to repeat: a
//!   different node answering at the same address (a reinstalled machine,
//!   a DNS change) is refused instead of being sent the user's commands
//!   ([`crate::Pinned`]).
//! - A node is reached exactly one way: `ssh`, `url`, or `key` with its
//!   routes (milestone 3). A `key` node needs a `credential` file and at
//!   least one route. The book never holds a secret: tokens and device
//!   credentials are files it points to, since the book is meant to be
//!   edited by hand and shared between machines (ADR 0010).

mod content;
mod file;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use vessel_proto::NodeId;

use crate::address::{InvalidAddress, InvalidRemotePath, InvalidSshTarget, NodeAddress};
use crate::credential::CredentialSource;
use crate::url::NodeUrl;

pub use file::{BookFile, ProjectConfig};

/// A node's name in the book.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeName(String);

/// Text that is not a node name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is not a node name (letters, digits, `.`, `_` and `-`, starting with a letter or digit)"
)]
pub struct InvalidNodeName(String);

impl NodeName {
    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NodeName {
    type Err = InvalidNodeName;

    fn from_str(text: &str) -> Result<Self, InvalidNodeName> {
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        let starts_plain = text.starts_with(|c: char| c.is_ascii_alphanumeric());
        if !starts_plain || !text.chars().all(plain) || text.len() > 64 {
            return Err(InvalidNodeName(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a websocket node's token is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// A file holding the token (`~/` is the local home directory).
    File(PathBuf),
}

impl TokenSource {
    /// The file to read, with a leading `~/` expanded against `home`.
    #[must_use]
    pub fn path(&self, home: Option<&Path>) -> PathBuf {
        let Self::File(path) = self;
        match (path.strip_prefix("~"), home) {
            (Ok(rest), Some(home)) => home.join(rest),
            _ => path.clone(),
        }
    }
}

/// One node in the book, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookEntry {
    /// How to reach it.
    pub address: NodeAddress,
    /// Where its token is (websocket nodes; an SSH node needs none).
    pub token: Option<TokenSource>,
    /// Where its device credential is (paired nodes only).
    pub credential: Option<CredentialSource>,
    /// The node's id, once it has been seen.
    pub id: Option<NodeId>,
}

/// The user's book (`<config_dir>/vessel/nodes.toml`), checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeBook {
    /// The node commands use when none is named.
    pub default_node: Option<NodeName>,
    /// Every node, by name.
    pub nodes: BTreeMap<NodeName, BookEntry>,
}

impl NodeBook {
    /// The book's file under a configuration directory
    /// (`dirs::config_dir()` in the CLI).
    #[must_use]
    pub fn path_in(config_dir: &Path) -> PathBuf {
        config_dir.join("vessel").join("nodes.toml")
    }

    /// The entry named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<(&NodeName, &BookEntry)> {
        let name = name.parse::<NodeName>().ok()?;
        self.nodes.get_key_value(&name)
    }
}

/// The node a command talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Its name, if it came from the book.
    pub name: Option<NodeName>,
    /// How to reach it.
    pub address: NodeAddress,
    /// The book's token source for it.
    pub token: Option<TokenSource>,
    /// The book's device credential for it (paired nodes).
    pub credential: Option<CredentialSource>,
    /// The id the book pinned for it.
    pub pinned: Option<NodeId>,
    /// Which layer chose it.
    pub origin: Origin,
}

/// Which layer chose the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `--node` or `VESSEL_NODE`.
    Named,
    /// The project file at this path.
    Project(PathBuf),
    /// The user book's `default_node`.
    User,
    /// Nothing chose: the node on this machine.
    Default,
}

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

/// The node to talk to: `named` (`--node`, or else `VESSEL_NODE`) if given,
/// else the project's `default_node`, else the book's, else this machine's
/// node at `local`.
///
/// `named` is looked up in the book first, then read as an address
/// ([`NodeAddress`]'s forms: `ssh://`, `user@host`, websocket URLs).
///
/// # Errors
/// `named` is neither a book name nor an address; the project's
/// `default_node` is not in the book.
pub fn resolve(
    book: &NodeBook,
    book_path: &Path,
    project: Option<&ProjectConfig>,
    named: Option<&str>,
    local: &NodeUrl,
) -> Result<Resolved, BookError> {
    if let Some(text) = named {
        if let Some((name, entry)) = book.get(text) {
            return Ok(from_entry(name, entry, Origin::Named));
        }
        let address = text.parse().map_err(|source| BookError::Unknown {
            text: text.to_owned(),
            source,
        })?;
        return Ok(Resolved {
            name: None,
            address,
            token: None,
            credential: None,
            pinned: None,
            origin: Origin::Named,
        });
    }
    if let Some(project) = project
        && let Some(name) = &project.default_node
    {
        let Some((name, entry)) = book.get(name.as_str()) else {
            return Err(BookError::UnknownDefault {
                path: project.path.clone(),
                name: name.to_string(),
                book: book_path.to_owned(),
            });
        };
        return Ok(from_entry(
            name,
            entry,
            Origin::Project(project.path.clone()),
        ));
    }
    if let Some(name) = &book.default_node
        && let Some((name, entry)) = book.get(name.as_str())
    {
        return Ok(from_entry(name, entry, Origin::User));
    }
    Ok(Resolved {
        name: None,
        address: NodeAddress::Ws(local.clone()),
        token: None,
        credential: None,
        pinned: None,
        origin: Origin::Default,
    })
}

fn from_entry(name: &NodeName, entry: &BookEntry, origin: Origin) -> Resolved {
    Resolved {
        name: Some(name.clone()),
        address: entry.address.clone(),
        token: entry.token.clone(),
        credential: entry.credential.clone(),
        pinned: entry.id,
        origin,
    }
}
