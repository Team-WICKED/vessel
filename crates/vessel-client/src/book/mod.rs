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
//!
//! Files: `name` (what a node may be called), `content` (each file as read,
//! and the checks that turn it into entries), `file` (reading and editing
//! the files), `resolve` (which node a command talks to), `error`.

mod content;
mod error;
mod file;
mod name;
mod resolve;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vessel_proto::NodeId;

use crate::address::NodeAddress;
use crate::credential::CredentialSource;

pub use error::{BookError, EntryProblem};
pub use file::{BookFile, ProjectConfig};
pub use name::{InvalidNodeName, NodeName};
pub use resolve::{Origin, Resolved, resolve};

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
