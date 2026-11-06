//! Who this node is, as told to clients.

use chrono::Utc;
use vessel_proto::{NodeId, NodeInfo, NodeState, ProtocolRange};

/// The node's self-description for this run.
pub(crate) fn node_info(id: NodeId, name: Option<String>) -> NodeInfo {
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
    }
}
