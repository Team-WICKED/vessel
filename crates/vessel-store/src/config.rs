//! Store limits and retention policy.

use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};

/// A size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteCount(u64);

impl ByteCount {
    /// Wrap a byte count.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// `mebibytes` MiB, saturating at `u64::MAX`.
    #[must_use]
    pub const fn mebibytes(mebibytes: u64) -> Self {
        Self(mebibytes.saturating_mul(1024 * 1024))
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ByteCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} B", self.0)
    }
}

/// How much history the store keeps. Oldest goes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Stored event bytes kept per session. Above it, the session's oldest
    /// records are deleted (never its newest one, so the final status
    /// survives). `session.read` then starts at the oldest retained record.
    pub max_session_bytes: ByteCount,
    /// Ended sessions kept. Above it, the sessions that ended longest ago are
    /// deleted with their logs. Live sessions are never deleted.
    pub max_ended_sessions: u32,
}

impl Default for Retention {
    /// 64 MiB per session (a long terminal session with heavy output) and
    /// the 500 most recent ended sessions: about 32 GiB in the worst case,
    /// a few hundred MiB in practice.
    fn default() -> Self {
        Self {
            max_session_bytes: ByteCount::mebibytes(64),
            max_ended_sessions: 500,
        }
    }
}

/// `n` as a `NonZeroUsize` for constants.
const fn nonzero_usize(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

/// `n` as a `NonZeroU32` for constants.
const fn nonzero_u32(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(n) => n,
        None => NonZeroU32::MIN,
    }
}

/// Everything configurable about a [`Store`](crate::Store).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreConfig {
    /// What to keep.
    pub retention: Retention,
    /// Write operations queued for the writer thread. When full, writers
    /// wait; the node's persistence then stops draining sessions, which
    /// pause their output pumps rather than drop records.
    pub write_queue: NonZeroUsize,
    /// Most records one `read_events` returns, whatever the caller asks.
    pub max_page_records: NonZeroU32,
    /// Most stored bytes one `read_events` returns (at least one record is
    /// always returned), so a page of large terminal chunks stays a
    /// reasonable frame.
    pub max_page_bytes: ByteCount,
    /// Idle read connections kept open for reuse.
    pub idle_readers: NonZeroUsize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            retention: Retention::default(),
            write_queue: nonzero_usize(1024),
            max_page_records: nonzero_u32(1000),
            max_page_bytes: ByteCount::mebibytes(1),
            idle_readers: nonzero_usize(4),
        }
    }
}
