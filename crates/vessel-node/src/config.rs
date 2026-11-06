//! How to run a node.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use vessel_harness::Lookup;
use vessel_proto::{DEFAULT_PORT, RouteUrl};
use vessel_relay::Keepalive;
use vessel_rpc::ServerConfig;
use vessel_session::{DEFAULT_DURABLE_CAPACITY, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RING_CAPACITY};
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
    /// Longest a structured session's agent may take to start (the ACP
    /// handshake) before the session fails. 60 s by default; raise it
    /// where adapters start slowly (`vesseld --agent-handshake-timeout`).
    pub agent_handshake_timeout: Duration,
    /// Per-connection RPC limits.
    pub rpc: ServerConfig,
    /// Relays this node registers with, in preference order (`--relay`);
    /// its pairing links name them (ADR 0009). Needs
    /// [`relay_token_file`](Self::relay_token_file).
    pub relays: Vec<RouteUrl>,
    /// The file holding the relays' registration token
    /// (`--relay-token-file`): a secret shared with the nodes that may use
    /// them (plan decision 4).
    pub relay_token_file: Option<PathBuf>,
    /// Direct addresses pairing links advertise (`--public`). Nothing is
    /// guessed: a node behind NAT has none, and is reached through a relay.
    pub public: Vec<RouteUrl>,
    /// How the node keeps its own `/secure` connections alive: a ping every
    /// 10 s, and a connection silent for 30 s is dropped, as a relay does
    /// for relayed ones. Clients give up after the same silence.
    pub keepalive: Keepalive,
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
            agent_handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            rpc: ServerConfig::default(),
            relays: Vec::new(),
            relay_token_file: None,
            public: Vec::new(),
            keepalive: Keepalive::DEFAULT,
        }
    }
}
