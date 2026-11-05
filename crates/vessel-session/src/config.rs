//! [`RuntimeConfig`]: how the runtime finds harnesses and sizes its buffers.

use std::num::NonZeroUsize;

/// Suggested [`RuntimeConfig::ring_capacity`]: enough recent records for a
/// reconnecting client to catch up without touching the store, small enough
/// that a session's cache stays in the low megabytes (PTY chunks are at most
/// 16 KiB).
pub const DEFAULT_RING_CAPACITY: NonZeroUsize = nonzero(1024);

/// Suggested [`RuntimeConfig::durable_capacity`]: absorbs a store write
/// batch's latency without pausing the session.
pub const DEFAULT_DURABLE_CAPACITY: NonZeroUsize = nonzero(256);

/// Configuration of a [`SessionRuntime`](crate::SessionRuntime).
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Where harness binaries are found.
    pub harness_lookup: vessel_harness::Lookup,
    /// In-memory records kept per session (a cache for live followers; the
    /// store is the source of truth). Also the live broadcast buffer: a
    /// follower further behind than this gets `Lagged`.
    pub ring_capacity: NonZeroUsize,
    /// Bound of each session's durable channel. When it is full the session
    /// pauses instead of dropping records.
    pub durable_capacity: NonZeroUsize,
}

/// `value` as a `NonZeroUsize` in a constant; a zero would fall back to 1,
/// but only the literals above are passed.
const fn nonzero(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(value) => value,
        None => NonZeroUsize::MIN,
    }
}
