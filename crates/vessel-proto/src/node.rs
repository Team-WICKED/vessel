//! Node identity and protocol versions.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::NodeId;

/// A wire protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(u32);

impl ProtocolVersion {
    /// Wrap a raw version number.
    #[must_use]
    pub const fn new(version: u32) -> Self {
        Self(version)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The protocol this build speaks. Bump on any breaking change to a wire type
/// and record the change in `docs/PROTOCOL.md`.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(3);

/// The oldest protocol this build still accepts. Version 3 only adds a
/// method and a defaulted field, so version 2 peers keep working.
pub const MIN_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(2);

/// An inclusive range of protocol versions a peer can speak.
///
/// [Decision Log]
/// - Purpose: fleet nodes, the CLI and future editor plugins will run
///   different versions at the same time.
/// - Alternatives: exact-match versions (Zed's collab server answers 426 on
///   any mismatch, workable only because Zed ships client and server
///   together); no envelope version at all (Paseo, which now carries hundreds
///   of per-field compatibility shims).
/// - Choice: both sides state a range in the handshake and use the highest
///   common version, or refuse with `ErrorCode::ProtocolMismatch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRange {
    /// Oldest supported version.
    pub min: ProtocolVersion,
    /// Newest supported version.
    pub max: ProtocolVersion,
}

impl ProtocolRange {
    /// The range this build supports.
    pub const CURRENT: Self = Self {
        min: MIN_PROTOCOL_VERSION,
        max: PROTOCOL_VERSION,
    };

    /// The highest version both ranges contain, if any.
    #[must_use]
    pub fn negotiate(self, other: Self) -> Option<ProtocolVersion> {
        let high = self.max.min(other.max);
        let low = self.min.max(other.min);
        (low <= high).then_some(high)
    }
}

/// Whether a node is taking new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Accepting new sessions.
    #[default]
    Serving,
    /// Asked to stop once idle (`node.stop` with `when_idle`): live sessions
    /// continue, new ones are refused with `ErrorCode::Unavailable`.
    Draining,
}

/// Facts about a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Stable id for this node.
    pub id: NodeId,
    /// Human-readable name (defaults to the hostname).
    pub name: String,
    /// OS hostname.
    pub hostname: String,
    /// `std::env::consts::OS`.
    pub os: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
    /// Version of the node binary.
    pub version: String,
    /// Protocols the node speaks.
    pub protocol: ProtocolRange,
    /// When the node process started.
    pub started_at: DateTime<Utc>,
    /// Whether it takes new sessions (protocol 3; absent means serving).
    #[serde(default)]
    pub state: NodeState,
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn range(min: u32, max: u32) -> ProtocolRange {
        ProtocolRange {
            min: ProtocolVersion::new(min),
            max: ProtocolVersion::new(max),
        }
    }

    #[test]
    fn negotiation_picks_highest_common_version() {
        assert_eq!(
            range(2, 5).negotiate(range(3, 4)),
            Some(ProtocolVersion::new(4))
        );
        assert_eq!(
            range(2, 3).negotiate(range(3, 9)),
            Some(ProtocolVersion::new(3))
        );
        assert_eq!(range(2, 3).negotiate(range(4, 9)), None);
    }
}
