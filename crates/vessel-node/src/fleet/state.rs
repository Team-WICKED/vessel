//! What `vessel fleet status` shows: the node's role ([`Role`]) and the
//! last thing its fleet loop did ([`FleetState`], `fleet/state.json`).
//!
//! [Decision Log]
//! - Purpose: status must work whether or not the node runs, and must
//!   show conflicts with their kind and place, which the protocol does not
//!   carry (`fleet.report` sends bundle paths only). So the node writes
//!   what it did to a file after every apply and every contact, and the
//!   CLI reads the files directly.
//! - One writer: the node, through [`StateFile`], which keeps the state in
//!   memory behind a lock and writes the whole file atomically on each
//!   change. The CLI only reads it.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use vessel_fleet::{Applied, ConflictKind};
use vessel_proto::{BundlePath, Generation, NodeId};

use super::files::{self, FileError, FleetFiles};
use super::member::Membership;

/// What the fleet loop last did on this node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetState {
    /// The generation last applied here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<Generation>,
    /// When.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    /// Files left as they are here, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictEntry>,
    /// Places that could not be written, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<FailureEntry>,
    /// Places left alone (in a source, behind a symlink, unreadable), by
    /// number.
    #[serde(default)]
    pub skipped: usize,
    /// On the home: what the bundle left out of the sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// On a member: when the home last answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contacted_at: Option<DateTime<Utc>>,
    /// The last error (reaching the home, reading the sources, applying),
    /// cleared by the next success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A file left as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntry {
    /// Its bundle path.
    pub bundle: BundlePath,
    /// The file.
    pub path: PathBuf,
    /// Why.
    pub kind: ConflictWhy,
    /// The bundle's version beside it, if written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming: Option<PathBuf>,
}

/// Why a file was left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictWhy {
    /// Edited here since Vessel wrote it.
    Edited,
    /// A file Vessel never wrote.
    NotWritten,
    /// Deleted here; `vessel fleet restore` brings it back.
    Deleted,
}

impl From<ConflictKind> for ConflictWhy {
    fn from(kind: ConflictKind) -> Self {
        match kind {
            ConflictKind::Edited => Self::Edited,
            ConflictKind::NotWritten => Self::NotWritten,
            ConflictKind::Deleted => Self::Deleted,
        }
    }
}

/// A place that could not be written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureEntry {
    /// The place.
    pub path: PathBuf,
    /// Why.
    pub error: String,
}

impl FleetState {
    /// Record an apply of `generation`.
    pub(crate) fn applied(&mut self, generation: Generation, applied: &Applied, skipped: usize) {
        self.generation = Some(generation);
        self.applied_at = Some(Utc::now());
        self.conflicts = applied
            .conflicts
            .iter()
            .map(|conflict| ConflictEntry {
                bundle: conflict.bundle.clone(),
                path: conflict.path.clone(),
                kind: conflict.kind.into(),
                incoming: conflict.incoming.clone(),
            })
            .collect();
        self.failures = applied
            .failures
            .iter()
            .map(|(path, error)| FailureEntry {
                path: path.clone(),
                error: error.to_string(),
            })
            .collect();
        self.skipped = skipped;
    }
}

/// The node's `state.json`, kept in memory and written on each change.
#[derive(Debug)]
pub(crate) struct StateFile {
    path: PathBuf,
    state: Mutex<FleetState>,
}

impl StateFile {
    /// The file in `files`, starting from what it holds (a damaged or
    /// missing file starts empty: it is only a report). Blocks.
    pub(crate) fn open(files: &FleetFiles) -> Self {
        let path = files.state();
        let state = files::read_json(&path).ok().flatten().unwrap_or_default();
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// A copy of the state.
    pub(crate) fn get(&self) -> FleetState {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Change the state and write it. Blocks on the file system (small,
    /// and called from blocking tasks or rarely).
    pub(crate) fn update(&self, change: impl FnOnce(&mut FleetState)) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut state);
        if let Err(error) = files::write_json(&self.path, &*state) {
            tracing::warn!(%error, "could not write the fleet state");
        }
    }
}

/// What this node is in a fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// Neither a home nor a member.
    None,
    /// The fleet's home: `fleet.toml` is here.
    Home {
        /// `fleet.toml`.
        sources: PathBuf,
    },
    /// A member of the fleet whose home is `home`.
    Member {
        /// The home node.
        home: NodeId,
        /// When it joined.
        joined_at: DateTime<Utc>,
    },
}

/// The node's fleet status, from its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetStatus {
    /// Its role.
    pub role: Role,
    /// What it last did.
    pub state: FleetState,
}

/// The fleet status of the node keeping its state in `files`' data
/// directory, running or not. Blocks.
///
/// # Errors
/// A file is there but cannot be read.
pub fn status(files: &FleetFiles) -> Result<FleetStatus, FileError> {
    let role = if files.sources().exists() {
        Role::Home {
            sources: files.sources().to_owned(),
        }
    } else if let Some(membership) = files::read_json::<Membership>(&files.membership())? {
        Role::Member {
            home: membership.home,
            joined_at: membership.joined_at,
        }
    } else {
        Role::None
    };
    let state = files::read_json(&files.state())?.unwrap_or_default();
    Ok(FleetStatus { role, state })
}
