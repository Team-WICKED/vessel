//! What the materialiser wrote, and the hash it wrote: `managed.json`
//! (`<data_dir>/fleet/managed.json`, owner-only).
//!
//! A file whose content still hashes to its record is Vessel's to update
//! or remove; any other content is somebody's edit. Files never recorded
//! are never touched. Incoming versions written beside edited files are
//! recorded too, so a stale one can be cleaned up once the conflict is
//! settled, and a file of that name the user made is left alone.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vessel_proto::BlobHash;

use crate::files::{self, Mode};

/// The record's format version.
const VERSION: u32 = 1;

/// Files the materialiser wrote, by path, with the hash it wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Managed {
    version: u32,
    /// Files written into targets.
    #[serde(default)]
    files: BTreeMap<PathBuf, BlobHash>,
    /// Incoming versions written beside locally edited files.
    #[serde(default)]
    incoming: BTreeMap<PathBuf, BlobHash>,
}

impl Default for Managed {
    fn default() -> Self {
        Self {
            version: VERSION,
            files: BTreeMap::new(),
            incoming: BTreeMap::new(),
        }
    }
}

/// `managed.json` could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ManagedError {
    /// The file could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The file is not a record this version reads.
    #[error("{}: not a managed-files record: {message}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

impl Managed {
    /// The record at `path`; an empty one if there is none yet.
    ///
    /// # Errors
    /// The file cannot be read, or is not a record (a damaged record is
    /// an error, not an empty one: forgetting would make Vessel treat its
    /// own files as the user's, which is safe but leaves them stale).
    pub fn load(path: &Path) -> Result<Self, ManagedError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ManagedError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let managed: Self = serde_json::from_str(&text).map_err(|error| ManagedError::Invalid {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        if managed.version != VERSION {
            return Err(ManagedError::Invalid {
                path: path.to_owned(),
                message: format!("version {} (this build reads {VERSION})", managed.version),
            });
        }
        Ok(managed)
    }

    /// Write the record to `path`, owner-only, replacing it atomically.
    ///
    /// # Errors
    /// The directory or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), ManagedError> {
        let io = |source| ManagedError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            files::private_dir(dir).map_err(io)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|error| ManagedError::Invalid {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        files::replace(path, Mode::Private, text.as_bytes()).map_err(io)
    }

    /// The hash Vessel wrote at `path`, if it wrote one.
    #[must_use]
    pub fn file(&self, path: &Path) -> Option<&BlobHash> {
        self.files.get(path)
    }

    /// Every file Vessel wrote.
    pub fn files(&self) -> impl Iterator<Item = (&Path, &BlobHash)> {
        self.files.iter().map(|(path, hash)| (path.as_path(), hash))
    }

    /// The incoming version Vessel wrote at `path`, if it wrote one.
    #[must_use]
    pub fn incoming(&self, path: &Path) -> Option<&BlobHash> {
        self.incoming.get(path)
    }

    pub(crate) fn record(&mut self, path: PathBuf, hash: BlobHash) {
        let _previous = self.files.insert(path, hash);
    }

    pub(crate) fn forget(&mut self, path: &Path) {
        let _previous = self.files.remove(path);
    }

    pub(crate) fn record_incoming(&mut self, path: PathBuf, hash: BlobHash) {
        let _previous = self.incoming.insert(path, hash);
    }

    pub(crate) fn forget_incoming(&mut self, path: &Path) {
        let _previous = self.incoming.remove(path);
    }

    pub(crate) fn incoming_paths(&self) -> impl Iterator<Item = (&Path, &BlobHash)> {
        self.incoming
            .iter()
            .map(|(path, hash)| (path.as_path(), hash))
    }
}
