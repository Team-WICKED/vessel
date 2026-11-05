//! The node's data directory layout and its identity files.
//!
//! ```text
//! <root>/            mode 0700 on Unix
//!   node.id          the node's id (a UUID), created on first start
//!   token            the access token, mode 0600, created on first start
//!   vessel.db        the event log (plus `SQLite`'s -wal and -shm files)
//! ```
//!
//! Both identity files are created once and never rewritten. Deleting
//! `token` and restarting the node rotates the token (ADR 0005); deleting
//! `node.id` gives the machine a new identity.

use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::NodeId;

use crate::error::StoreError;
use crate::files::{self, Access};
use crate::token::NodeToken;

const NODE_ID_FILE: &str = "node.id";
const TOKEN_FILE: &str = "token";
const DATABASE_FILE: &str = "vessel.db";

/// A node's data directory. Constructing one does no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// The directory at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `~/.vessel`, if the home directory is known.
    #[must_use]
    pub fn default_root() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".vessel"))
    }

    /// The directory itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the event log lives.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.root.join(DATABASE_FILE)
    }

    /// Where the token lives (named in the warning a node logs when it
    /// listens beyond loopback).
    #[must_use]
    pub fn token_path(&self) -> PathBuf {
        self.root.join(TOKEN_FILE)
    }

    /// Create the directory if it does not exist (owner-only on Unix).
    ///
    /// # Errors
    /// The directory cannot be created.
    pub fn prepare(&self) -> Result<(), StoreError> {
        files::create_private_dir(&self.root).map_err(StoreError::io(&self.root))
    }

    /// The node id, created on first use.
    ///
    /// # Errors
    /// The file cannot be read or written, or holds something else.
    pub fn load_or_create_node_id(&self) -> Result<NodeId, StoreError> {
        let path = self.root.join(NODE_ID_FILE);
        if let Some(id) = read_node_id(&path)? {
            return Ok(id);
        }
        let fresh = NodeId::random();
        let text = format!("{fresh}\n");
        if files::create_once(&path, text.as_bytes(), Access::Shared)
            .map_err(StoreError::io(&path))?
        {
            tracing::info!(node = %fresh, path = %path.display(), "created node id");
            return Ok(fresh);
        }
        // Another process created it first; use theirs.
        read_node_id(&path)?.ok_or_else(|| StoreError::Io {
            path,
            source: io::ErrorKind::NotFound.into(),
        })
    }

    /// The access token, created on first use (owner-only).
    ///
    /// # Errors
    /// The file cannot be read or written, holds something else, or the
    /// system has no randomness.
    pub fn load_or_create_token(&self) -> Result<NodeToken, StoreError> {
        let path = self.token_path();
        match self.read_token() {
            Ok(token) => return Ok(token),
            Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let fresh = NodeToken::generate().map_err(StoreError::Random)?;
        if files::create_once(&path, fresh.expose().as_bytes(), Access::Private)
            .map_err(StoreError::io(&path))?
        {
            tracing::info!(path = %path.display(), "created access token");
            return Ok(fresh);
        }
        self.read_token()
    }

    /// The access token, without creating one (for local clients).
    ///
    /// # Errors
    /// There is no token file (the node never started here), it cannot be
    /// read, or it holds something else.
    pub fn read_token(&self) -> Result<NodeToken, StoreError> {
        let path = self.token_path();
        let text = std::fs::read_to_string(&path).map_err(StoreError::io(&path))?;
        let token = text
            .parse::<NodeToken>()
            .map_err(|_invalid| StoreError::InvalidToken { path: path.clone() })?;
        match files::readable_by_others(&path) {
            Ok(false) => {}
            Ok(true) => tracing::warn!(
                path = %path.display(),
                "the token file is readable by other users; run `chmod 600` on it"
            ),
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "could not check token file mode");
            }
        }
        Ok(token)
    }
}

/// `None` if the file does not exist.
fn read_node_id(path: &Path) -> Result<Option<NodeId>, StoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<NodeId>()
            .map(Some)
            .map_err(|source| StoreError::InvalidNodeId {
                path: path.to_owned(),
                source,
            }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}
