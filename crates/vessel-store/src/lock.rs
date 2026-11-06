//! [`NodeLock`]: one node per data directory, and who that node is.
//!
//! [Decision Log]
//! - Purpose: two nodes on one data directory would both open the event log
//!   and both claim the local socket. `vesseld attach` also needs to know
//!   whether a node is running, and the installer's cleanup needs to know
//!   which binary it runs (ADRs 0007 and 0008).
//! - Alternatives: a pid file (Zed): a pid says nothing once the process is
//!   gone and its number reused, so it can name an unrelated process; asking
//!   the socket (says "no node" while one is still starting).
//! - Choice: the node holds an exclusive OS lock on `vesseld.lock` for its
//!   whole life (std `File::try_lock`: `flock` on Unix, `LockFileEx` on
//!   Windows). The OS releases it when the process ends, however it ends,
//!   so the lock can never be stale. Who holds it is written to a separate
//!   file, `vesseld.owner`, because Windows forbids reading a locked range;
//!   that record is trusted only while the lock is held.

use std::fs::{File, TryLockError};
use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data_dir::DataDir;
use crate::error::StoreError;
use crate::files;

/// The process that holds a data directory's node lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeOwner {
    /// Its process id.
    pub pid: u32,
    /// The binary it runs (cleanup keeps this one, ADR 0008).
    pub executable: PathBuf,
    /// Its version.
    pub version: String,
    /// When it took the lock.
    pub started_at: DateTime<Utc>,
}

/// The node lock of one data directory, held until dropped (or until the
/// process ends).
#[derive(Debug)]
pub struct NodeLock {
    /// Kept open: closing it releases the lock.
    _file: File,
}

impl NodeLock {
    /// Take the lock and record `owner` as its holder.
    ///
    /// # Errors
    /// [`StoreError::Locked`] when another node holds it (with that node's
    /// record if readable); [`StoreError::Io`] when the files cannot be
    /// opened or written.
    pub fn acquire(dir: &DataDir, owner: &NodeOwner) -> Result<Self, StoreError> {
        let path = dir.lock_path();
        let file = files::open_private(&path).map_err(StoreError::io(&path))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(StoreError::Locked {
                    path,
                    holder: read_owner(dir).map(Box::new),
                });
            }
            Err(TryLockError::Error(source)) => return Err(StoreError::Io { path, source }),
        }
        let owner_path = dir.owner_path();
        let record = serde_json::to_vec(owner).map_err(StoreError::Encode)?;
        files::replace(&owner_path, &record).map_err(StoreError::io(&owner_path))?;
        Ok(Self { _file: file })
    }

    /// Who holds the lock right now; `None` when no node does (any owner
    /// record left behind is then stale and ignored).
    ///
    /// # Errors
    /// The lock file cannot be opened or tested.
    pub fn holder(dir: &DataDir) -> Result<Option<NodeOwner>, StoreError> {
        let path = dir.lock_path();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        match file.try_lock() {
            // Nobody holds it; dropping `file` releases it again at once.
            Ok(()) => Ok(None),
            Err(TryLockError::WouldBlock) => Ok(read_owner(dir)),
            Err(TryLockError::Error(source)) => Err(StoreError::Io { path, source }),
        }
    }
}

/// The owner record, if readable. Only meaningful while the lock is held.
fn read_owner(dir: &DataDir) -> Option<NodeOwner> {
    let path = dir.owner_path();
    let text = std::fs::read(&path)
        .inspect_err(
            |error| tracing::debug!(path = %path.display(), %error, "no node owner record"),
        )
        .ok()?;
    serde_json::from_slice(&text)
        .inspect_err(
            |error| tracing::warn!(path = %path.display(), %error, "unreadable node owner record"),
        )
        .ok()
}
