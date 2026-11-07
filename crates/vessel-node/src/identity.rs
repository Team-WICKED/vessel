//! Who this node is, as told to clients.

use chrono::Utc;
use vessel_proto::{NodeId, NodeInfo, NodePublicKey, NodeState, ProtocolRange};

/// The node's self-description for this run.
pub(crate) fn node_info(id: NodeId, name: Option<String>, key: NodePublicKey) -> NodeInfo {
    let hostname = hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "unknown".to_owned());
    NodeInfo {
        id,
        name: name.unwrap_or_else(|| hostname.clone()),
        hostname,
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: ProtocolRange::CURRENT,
        started_at: Utc::now(),
        state: NodeState::Serving,
        // Shown by `vessel node status`, and how a client already
        // connected (token or SSH) learns the key (ADR 0009).
        public_key: Some(key),
    }
}
