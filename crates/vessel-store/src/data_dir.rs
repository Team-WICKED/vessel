//! The node's data directory layout and its identity files.
//!
//! ```text
//! <root>/            mode 0700 on Unix
//!   node.id          the node's id (a UUID), created on first start
//!   token            the access token, created on first start
//!   vessel.db        the event log (plus `SQLite`'s -wal and -shm files)
//!   vesseld.lock     locked by the running node (`NodeLock`)
//!   vesseld.owner    which process holds that lock: pid, binary, version
//!   vesseld.sock     the node's local socket (Unix; ADR 0007)
//!   bin/             installed `vesseld-<version>` binaries (ADR 0008)
//!   logs/            the log of a node started by `vesseld attach`
//! ```
//!
//! Every file is owner-only (0600 on Unix); see `files` for why and for
//! Windows.
//!
//! Both identity files are created once and never rewritten. Deleting
//! `token` and restarting the node rotates the token (ADR 0005); deleting
//! `node.id` gives the machine a new identity.

use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::NodeId;

use crate::error::StoreError;
use crate::files::{self, Privacy};
use crate::token::NodeToken;

const NODE_ID_FILE: &str = "node.id";
const TOKEN_FILE: &str = "token";
const DATABASE_FILE: &str = "vessel.db";
const LOCK_FILE: &str = "vesseld.lock";
const START_LOCK_FILE: &str = "vesseld.start.lock";
const NODE_LOG_FILE: &str = "vesseld.log";
/// A node log above this size is rotated (to `vesseld.log.1`) when the
/// next background node starts, so the log stays bounded.
const NODE_LOG_ROTATE: u64 = 10 * 1024 * 1024;
const OWNER_FILE: &str = "vesseld.owner";
const SOCKET_FILE: &str = "vesseld.sock";
const BIN_DIR: &str = "bin";
const LOG_DIR: &str = "logs";

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

    /// The file the running node keeps locked (see `NodeLock`).
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_FILE)
    }

    /// Serialises `vesseld attach` processes that start a node (see
    /// `StartLock`).
    #[must_use]
    pub fn start_lock_path(&self) -> PathBuf {
        self.root.join(START_LOCK_FILE)
    }

    /// Which process holds the node lock (see `NodeOwner`).
    #[must_use]
    pub fn owner_path(&self) -> PathBuf {
        self.root.join(OWNER_FILE)
    }

    /// The node's local socket on Unix (ADR 0007). Windows uses a named
    /// pipe instead, named after the node id by `vessel-node`.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.root.join(SOCKET_FILE)
    }

    /// Installed `vesseld` binaries, side by side by version (ADR 0008).
    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join(BIN_DIR)
    }

    /// Logs of a node started in the background by `vesseld attach`.
    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.root.join(LOG_DIR)
    }

    /// Create the directory if it does not exist, owner-only on Unix. An
    /// existing directory that others can access is made owner-only (and
    /// the change logged), or refused if it belongs to another user.
    ///
    /// # Errors
    /// The directory cannot be created, or is accessible to other users and
    /// cannot be changed ([`StoreError::NotPrivate`]).
    pub fn prepare(&self) -> Result<(), StoreError> {
        files::create_private_dir(&self.root).map_err(StoreError::io(&self.root))?;
        match files::make_private(&self.root) {
            Ok(Privacy::Private) => Ok(()),
            Ok(Privacy::Tightened { from }) => {
                tracing::warn!(
                    path = %self.root.display(),
                    mode = %format!("{from:o}"),
                    "the data directory was accessible to other users; made it owner-only (0700)"
                );
                Ok(())
            }
            Err(source) if source.kind() == io::ErrorKind::PermissionDenied => {
                Err(StoreError::NotPrivate {
                    path: self.root.clone(),
                    source,
                })
            }
            Err(source) => Err(StoreError::Io {
                path: self.root.clone(),
                source,
            }),
        }
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
        if files::create_once(&path, text.as_bytes()).map_err(StoreError::io(&path))? {
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
        if files::create_once(&path, fresh.expose().as_bytes()).map_err(StoreError::io(&path))? {
            tracing::info!(path = %path.display(), "created access token");
            return Ok(fresh);
        }
        self.read_token()
    }

    /// The log file for a node started in the background, opened for
    /// appending (owner-only), after rotating a log that grew too large.
    /// Blocking.
    ///
    /// # Errors
    /// The log directory or file cannot be created or opened.
    pub fn open_node_log(&self) -> Result<(PathBuf, std::fs::File), StoreError> {
        let dir = self.log_dir();
        files::create_private_dir(&dir).map_err(StoreError::io(&dir))?;
        let path = dir.join(NODE_LOG_FILE);
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > NODE_LOG_ROTATE) {
            let rotated = dir.join(format!("{NODE_LOG_FILE}.1"));
            std::fs::rename(&path, &rotated).map_err(StoreError::io(&rotated))?;
        }
        let file = files::open_append_private(&path).map_err(StoreError::io(&path))?;
        Ok((path, file))
    }

    /// The node id, without creating one; `None` if no node has run here.
    ///
    /// # Errors
    /// The file cannot be read or holds something else.
    pub fn read_node_id(&self) -> Result<Option<NodeId>, StoreError> {
        read_node_id(&self.root.join(NODE_ID_FILE))
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
