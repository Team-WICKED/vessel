//! The store's error type.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use vessel_proto::{DeviceId, ErrorCode, EventSeq, NodeId, SessionId};

/// A value that does not fit the on-disk representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantity {
    /// A sequence number above `i64::MAX`.
    Seq,
    /// A timestamp outside 1677..2262 (nanoseconds in an `i64`).
    Time,
    /// A byte count above `i64::MAX`.
    Bytes,
}

/// A stored column whose value failed to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// `sessions.spec` (a `SessionSpec`).
    Spec,
    /// `sessions.status` (a `SessionStatus`).
    Status,
    /// `events.event` (a `SessionEvent`).
    Event,
    /// `sessions.profile` (a `ProfileName`, schema 3).
    Profile,
    /// `sessions.created_by` (a `Creator`, schema 3).
    CreatedBy,
    /// `sessions.via` (a `NodeId`, schema 4).
    Via,
    /// `sessions.origin` (a `SessionOrigin`, schema 4).
    Origin,
}

/// Anything that can go wrong in the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A file in the data directory could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// What failed.
        #[source]
        source: std::io::Error,
    },
    /// The data directory is accessible to other users and is not ours to
    /// change.
    #[error(
        "{} is accessible to other users and cannot be made owner-only (is it another \
         user's?); fix its permissions or choose another data directory",
        path.display()
    )]
    NotPrivate {
        /// The directory.
        path: PathBuf,
        /// Why its mode could not be changed.
        #[source]
        source: std::io::Error,
    },
    /// Another node holds the data directory's lock.
    #[error(
        "{} is held by another node{}",
        path.display(),
        holder.as_deref().map(|owner| format!(" (pid {}, version {})", owner.pid, owner.version)).unwrap_or_default()
    )]
    Locked {
        /// The lock file.
        path: PathBuf,
        /// That node's record, when it could be read.
        holder: Option<Box<crate::lock::NodeOwner>>,
    },
    /// The operating system's random source failed.
    #[error("no randomness available")]
    Random(#[source] getrandom::Error),
    /// `node.id` does not hold a node id.
    #[error("{} does not hold a node id", path.display())]
    InvalidNodeId {
        /// The file.
        path: PathBuf,
        /// Why it did not parse.
        #[source]
        source: <NodeId as FromStr>::Err,
    },
    /// `node.key` does not hold a key. It is never replaced automatically:
    /// a new key would unpair every device (ADR 0009).
    #[error(
        "{} does not hold a node key (expected 32 bytes, found {len}); restore it from a \
         backup, or delete it to make a new key (every paired device must then pair again)",
        path.display()
    )]
    InvalidNodeKey {
        /// The file.
        path: PathBuf,
        /// Its size.
        len: usize,
    },
    /// The token file does not hold a token this node could have written.
    #[error("{} does not hold a valid token", path.display())]
    InvalidToken {
        /// The file.
        path: PathBuf,
    },
    /// The database is from a newer Vessel; refusing to touch it.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    NewerSchema {
        /// Version found on disk.
        found: i64,
        /// Newest version this build knows.
        supported: i64,
    },
    /// `SQLite` failed.
    #[error("database error")]
    Sqlite(#[from] rusqlite::Error),
    /// Committing a batch failed, so every operation in it failed. Shared
    /// because one failure answers several callers.
    #[error("committing to the database failed")]
    Commit(#[source] Arc<rusqlite::Error>),
    /// A stored value did not decode; the database was edited or corrupted.
    #[error("stored {column:?} of session {session} is invalid")]
    Corrupt {
        /// Session the row belongs to.
        session: SessionId,
        /// Column that failed.
        column: Column,
        /// Why.
        #[source]
        source: serde_json::Error,
    },
    /// A stored session id did not parse.
    #[error("stored session id {text:?} is invalid")]
    CorruptId {
        /// The stored text.
        text: String,
        /// Why.
        #[source]
        source: <SessionId as FromStr>::Err,
    },
    /// A stored device row did not decode; the database was edited or
    /// corrupted.
    #[error("stored device {id:?} has an invalid {what}")]
    CorruptDevice {
        /// The stored id text.
        id: String,
        /// Which column.
        what: &'static str,
    },
    /// No device with this id is stored.
    #[error("device {0} is not known to this node")]
    UnknownDevice(DeviceId),
    /// A device with this id is already stored.
    #[error("device {0} already exists")]
    DeviceExists(DeviceId),
    /// A value could not be encoded for storage.
    #[error("could not encode a value for storage")]
    Encode(#[source] serde_json::Error),
    /// A number does not fit its column.
    #[error("{0:?} out of the storable range")]
    OutOfRange(Quantity),
    /// No session with this id is stored.
    #[error("session {0} is not in the store")]
    UnknownSession(SessionId),
    /// A session with this id is already stored.
    #[error("session {0} is already in the store")]
    SessionExists(SessionId),
    /// Records must be appended gap-free, in order.
    #[error("session {session}: expected record {expected}, got {got}")]
    OutOfOrder {
        /// The session.
        session: SessionId,
        /// The sequence number the log needed next.
        expected: EventSeq,
        /// The one it was given.
        got: EventSeq,
    },
    /// The store was closed, or its writer thread is gone.
    #[error("the store is closed")]
    Closed,
    /// The writer thread could not be started.
    #[error("could not start the store writer thread")]
    Thread(#[source] std::io::Error),
    /// A blocking read task failed.
    #[error("a store read task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl StoreError {
    /// The wire error code for this failure.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownSession(_) | Self::UnknownDevice(_) => ErrorCode::NotFound,
            Self::SessionExists(_) | Self::DeviceExists(_) | Self::OutOfOrder { .. } => {
                ErrorCode::Conflict
            }
            Self::Closed => ErrorCode::Unavailable,
            Self::Io { .. }
            | Self::NotPrivate { .. }
            | Self::Locked { .. }
            | Self::Random(_)
            | Self::InvalidNodeId { .. }
            | Self::InvalidToken { .. }
            | Self::InvalidNodeKey { .. }
            | Self::NewerSchema { .. }
            | Self::Sqlite(_)
            | Self::Commit(_)
            | Self::Corrupt { .. }
            | Self::CorruptId { .. }
            | Self::CorruptDevice { .. }
            | Self::Encode(_)
            | Self::OutOfRange(_)
            | Self::Thread(_)
            | Self::Task(_) => ErrorCode::Internal,
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }
}
