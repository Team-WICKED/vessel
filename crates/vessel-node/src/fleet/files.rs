//! Where the fleet keeps its state ([`FleetFiles`]), how its JSON files are
//! read and written, and the lock around applying ([`FleetLock`]).
//!
//! [Decision Log]
//! - Purpose: fleet state is files in the data directory, not rows in the
//!   event log (vessel-store belongs to sessions; the coordinator's split).
//!   The CLI reads and writes some of them while the node runs (`vessel
//!   fleet join`, `status`, `restore`), so each file has one writer at a
//!   time and every write is atomic.
//! - Layout: `<data_dir>/fleet.toml` (the home's sources, edited by
//!   people), `<data_dir>/secrets.toml` (this node's secrets, owner-only)
//!   and `<data_dir>/fleet/` (owner-only, Vessel's): `blobs/`,
//!   `manifest.json`, `managed.json`, `state.json`, `members.json` (home),
//!   `member.json` and `member.credential` (member), `apply` (a request to
//!   apply again) and `lock`.
//! - The lock: `std::fs::File::lock` on `fleet/lock`, held while planning,
//!   applying and saving `managed.json`, by the node and by `vessel fleet
//!   restore` alike, so neither saves over the other's record. An OS lock
//!   is released when its holder dies, so a crash never leaves it held.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use vessel_store::DataDir;

/// The fleet's files in one data directory.
#[derive(Debug, Clone)]
pub struct FleetFiles {
    sources: PathBuf,
    secrets: PathBuf,
    dir: PathBuf,
}

impl FleetFiles {
    /// The files of the node keeping its state in `data_dir`.
    #[must_use]
    pub fn new(data_dir: &DataDir) -> Self {
        Self {
            sources: data_dir.root().join("fleet.toml"),
            secrets: data_dir.root().join("secrets.toml"),
            dir: data_dir.root().join("fleet"),
        }
    }

    /// `fleet.toml`: the home's sources. It makes this node the home.
    #[must_use]
    pub fn sources(&self) -> &Path {
        &self.sources
    }

    /// `secrets.toml`: this node's secrets for the fleet's MCP servers.
    #[must_use]
    pub fn secrets(&self) -> &Path {
        &self.secrets
    }

    /// `fleet/`: Vessel's own fleet state, owner-only.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn blobs(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    /// The manifest this node publishes (home) or last fetched (member).
    pub(crate) fn manifest(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    pub(crate) fn managed(&self) -> PathBuf {
        self.dir.join("managed.json")
    }

    pub(crate) fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// What each member last reported (home).
    pub(crate) fn members(&self) -> PathBuf {
        self.dir.join("members.json")
    }

    /// The home this node is a member of.
    pub(crate) fn membership(&self) -> PathBuf {
        self.dir.join("member.json")
    }

    /// This node's credential on its home.
    pub(crate) fn credential(&self) -> PathBuf {
        self.dir.join("member.credential")
    }

    /// Present when someone asked for the bundle to be applied again
    /// (`vessel fleet restore`); the node removes it and applies.
    pub(crate) fn apply_request(&self) -> PathBuf {
        self.dir.join("apply")
    }

    fn lock(&self) -> PathBuf {
        self.dir.join("lock")
    }
}

/// A fleet file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// Reading or writing failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The file is not what this version writes there.
    #[error("{}: {message}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

impl FileError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

/// The JSON file at `path`; `None` if there is none.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, FileError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FileError::io(path, error)),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| FileError::Invalid {
            path: path.to_owned(),
            message: error.to_string(),
        })
}

/// Write `value` as JSON at `path`, owner-only and atomically.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), FileError> {
    let text = serde_json::to_vec_pretty(value).map_err(|error| FileError::Invalid {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    vessel_fleet::write_private(path, &text).map_err(|error| FileError::io(path, error))
}

/// Remove the file at `path`; a missing one is no error.
pub(crate) fn remove(path: &Path) -> Result<(), FileError> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(FileError::io(path, error)),
        Ok(()) | Err(_) => Ok(()),
    }
}

/// The fleet lock, held until dropped. Blocks to take it.
#[derive(Debug)]
pub(crate) struct FleetLock {
    _file: File,
}

impl FleetLock {
    pub(crate) fn acquire(files: &FleetFiles) -> Result<Self, FileError> {
        let path = files.lock();
        let mut options = OpenOptions::new();
        let _same = options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let _same = options.mode(0o600);
        }
        if let Some(dir) = path.parent() {
            create_private_dir(dir).map_err(|error| FileError::io(dir, error))?;
        }
        let file = options
            .open(&path)
            .map_err(|error| FileError::io(&path, error))?;
        file.lock().map_err(|error| FileError::io(&path, error))?;
        Ok(Self { _file: file })
    }
}

/// Create `dir` (and its parents) owner-only where created.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
