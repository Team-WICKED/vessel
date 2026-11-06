//! How to run a node.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;

use vessel_harness::Lookup;
use vessel_proto::DEFAULT_PORT;
use vessel_rpc::ServerConfig;
use vessel_session::{DEFAULT_DURABLE_CAPACITY, DEFAULT_RING_CAPACITY};
use vessel_store::{DataDir, StoreConfig};

/// Everything a node needs to start. Build with [`Config::new`] and
/// override fields as needed.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP address for the websocket (`/rpc`). Loopback by default;
    /// anything else is reachable by other machines and logs a warning
    /// naming the token file. `None` serves the local socket only (reached
    /// through `vesseld attach`, for example over SSH; ADR 0007).
    pub bind: Option<SocketAddr>,
    /// Name shown to clients; the hostname when `None`.
    pub name: Option<String>,
    /// Identity, token and event log.
    pub data_dir: DataDir,
    /// Where harness binaries are looked up (`PATH` by default).
    pub harness_lookup: Lookup,
    /// Event log limits and retention.
    pub store: StoreConfig,
    /// Records each session keeps in memory for live followers.
    pub ring_capacity: NonZeroUsize,
    /// Records a session may hand over before persistence catches up (the
    /// session pauses its output while this is full).
    pub durable_capacity: NonZeroUsize,
    /// Per-connection RPC limits.
    pub rpc: ServerConfig,
}

impl Config {
    /// Defaults for a node keeping its state in `data_dir`.
    #[must_use]
    pub fn new(data_dir: DataDir) -> Self {
        Self {
            bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT))),
            name: None,
            data_dir,
            harness_lookup: Lookup::from_env(),
            store: StoreConfig::default(),
            ring_capacity: DEFAULT_RING_CAPACITY,
            durable_capacity: DEFAULT_DURABLE_CAPACITY,
            rpc: ServerConfig::default(),
        }
    }
}
