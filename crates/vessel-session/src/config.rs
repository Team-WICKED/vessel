//! [`RuntimeConfig`]: how the runtime finds harnesses and sizes its buffers.

/// Suggested [`RuntimeConfig::ring_capacity`]: enough recent records for a
/// reconnecting client to catch up without touching the store, small enough
/// that a session's cache stays in the low megabytes (PTY chunks are at most
/// 16 KiB).
pub const DEFAULT_RING_CAPACITY: usize = 1024;

/// Suggested [`RuntimeConfig::durable_capacity`]: absorbs a store write
/// batch's latency without pausing the session.
pub const DEFAULT_DURABLE_CAPACITY: usize = 256;

/// Configuration of a [`SessionRuntime`](crate::SessionRuntime).
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Where harness binaries are found.
    pub harness_lookup: vessel_harness::Lookup,
    /// In-memory records kept per session (a cache for live followers; the
    /// store is the source of truth). Also the live broadcast buffer: a
    /// follower further behind than this gets `Lagged`. Values below 1 are
    /// treated as 1.
    pub ring_capacity: usize,
    /// Bound of each session's durable channel. When it is full the session
    /// pauses instead of dropping records. Values below 1 are treated as 1.
    pub durable_capacity: usize,
}

impl RuntimeConfig {
    /// `ring_capacity`, made usable as a channel bound (tokio channels
    /// reject zero).
    pub(crate) fn ring(&self) -> usize {
        self.ring_capacity.max(1)
    }

    /// `durable_capacity`, made usable as a channel bound.
    pub(crate) fn durable(&self) -> usize {
        self.durable_capacity.max(1)
    }
}
