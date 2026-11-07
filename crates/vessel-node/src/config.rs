//! How to run a node.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use vessel_harness::{HarnessEnv, Lookup};
use vessel_proto::{DEFAULT_PORT, RouteUrl};
use vessel_relay::Keepalive;
use vessel_rpc::ServerConfig;
use vessel_session::{DEFAULT_DURABLE_CAPACITY, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RING_CAPACITY};
use vessel_store::{DataDir, StoreConfig};

use crate::fleet::FleetConfig;

/// Everything a node needs to start. Build with [`Config::new`] and
/// override fields as needed.
#[derive(Debug, Clone)]
pub struct Config {
    /// Language-server selection and bounded lifecycle. Library defaults find no real tools.
    pub lsp: vessel_lsp::Config,
    /// Destination roots allowed to Read callers; local agents also have their own cwd.
    pub lsp_roots: Vec<crate::LspRoot>,
    /// TCP address for the websocket (`/rpc`). Loopback by default;
    /// anything else is reachable by other machines and logs a warning
    /// naming the token file. `None` serves the local socket only (reached
    /// through `vesseld attach`, for example over SSH; ADR 0007).
    pub bind: Option<SocketAddr>,
    /// Name shown to clients; the hostname when `None`.
    pub name: Option<String>,
    /// Identity, token and event log.
    pub data_dir: DataDir,
    /// The account harnesses run as: the user's home and the harness home
    /// variables (`Config::new` says why it is required).
    pub harness_env: HarnessEnv,
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
    /// The command that runs `vessel mcp`, attached to every session as its
    /// `vessel` MCP server so the session's agent can reach this node with
    /// a credential of its own (ADR 0013). `None` (the default): sessions
    /// get neither. `vessel node start` sets its own executable; `vesseld`
    /// a `vessel` beside it.
    pub vessel_mcp: Option<VesselMcp>,
    /// Fleet sync (ADR 0012): where bundles are written and how often the
    /// node looks; the defaults suit a real node.
    pub fleet: FleetConfig,
    /// Behind a reverse proxy every direct client comes from the proxy:
    /// the header in which it passes the client's address (`X-Real-IP`),
    /// so `/secure`'s per-address budget is per client
    /// (`vesseld --client-address-header`). Believed only from
    /// [`trusted_proxies`](Self::trusted_proxies), which it needs.
    pub client_address_header: Option<axum::http::HeaderName>,
    /// The peers whose [`client_address_header`](Self::client_address_header)
    /// is believed (`vesseld --trusted-proxy`); anyone else is counted by
    /// its own address.
    pub trusted_proxies: Vec<vessel_relay::TrustedProxy>,
}

/// A command that runs `vessel mcp` ([`Config::vessel_mcp`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VesselMcp {
    /// The program, an absolute path.
    pub program: PathBuf,
    /// Its arguments (`["mcp"]` for `vessel`).
    pub args: Vec<String>,
}

impl VesselMcp {
    /// `program mcp`.
    #[must_use]
    pub fn vessel(program: PathBuf) -> Self {
        Self {
            program,
            args: vec!["mcp".to_owned()],
        }
    }
}

impl Config {
    /// Defaults for a node keeping its state in `data_dir`, running
    /// harnesses as `harness_env`'s account.
    ///
    /// `harness_env` has no default on purpose: `vesseld` and the CLI make
    /// it from their own environment (`HarnessEnv::from_vars`), and a test
    /// gives a temporary home (`HarnessEnv::new`), so no in-process node
    /// reaches the machine's real harness homes by an argument forgotten.
    #[must_use]
    pub fn new(data_dir: DataDir, harness_env: HarnessEnv) -> Self {
        Self {
            lsp: vessel_lsp::Config {
                registry: vessel_lsp::registry::Registry {
                    home: Some(harness_env.user_home().to_owned()),
                    ..vessel_lsp::registry::Registry::default()
                },
                ..vessel_lsp::Config::default()
            },
            lsp_roots: Vec::new(),
            bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT))),
            name: None,
            data_dir,
            harness_env,
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
            vessel_mcp: None,
            fleet: FleetConfig::default(),
            client_address_header: None,
            trusted_proxies: Vec::new(),
        }
    }
}
