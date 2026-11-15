//! `vesseld`: one node per machine, serving its harness sessions over
//! the protocol (versions 2 to 7, `vessel_proto::ProtocolRange`).
//!
//! This crate assembles the others; it holds wiring and node policy, not
//! mechanisms:
//!
//! ```text
//!  client --ws /rpc (bearer token)-----------------> server ---+
//!  ssh -> vesseld attach --local socket (token)----> ipc ------+
//!  agent --local socket (agent credential)---------> ipc ------+--> vessel-rpc Server
//!  device --/secure or a relay (Noise, preamble)---> secure ---+         | Handler
//!                                                     guard (principal, scope, ownership)
//!                                                     service (routing only), lifecycle
//!                                                                    |
//!            sessions: live runtime sessions + stored ones (ended, handed over)
//!              |  create ---> vessel-session ---durable records---> persist ---> vessel-store
//!              |  subscribe -> follow (ring) + store gap fill ---> Outbound
//!              |  read ------> wait until durable, then the store
//! ```
//!
//! - `config`, `identity`: what to run and who this node is.
//! - `server`, `auth`, `transport`: HTTP (`GET /health`) and the `/rpc`
//!   websocket, token check before the upgrade.
//! - `principal`, `guard`: who a connection is (owner, device, agent,
//!   delegated) and the check every call passes (its method's scope, and
//!   ownership for an agent) before `service` sees it.
//! - `service`: the `Handler` implementation; routes each call, maps typed
//!   errors to wire errors once.
//! - `devices`, `secure`, `relays`: paired devices (ADR 0010), the Noise
//!   gateway for `/secure` and relayed channels, and the node's relay links
//!   (ADR 0009).
//! - `agents`: agent principals, their credentials and limits (ADR 0013).
//! - `envelope`, `waits`: calls across nodes through the fleet's home
//!   (protocol 6) and the shares of waiting `session.read`s.
//! - `profiles`: `profiles.toml` and what a session launches with (ADR
//!   0011).
//! - `transfers`, `secret_copy`: sessions and memory moved between nodes,
//!   and copying a secret to another node (protocol 6).
//! - `models`: model routes and the loopback proxy (ADR 0015).
//! - `lsp`, `lsp_config`: language context for agents (ADR 0016).
//! - `sessions`: the node's view of sessions, live and stored.
//! - `persist`: drains each session's durable channel into the store and
//!   publishes how far it is committed; once a log is complete, `sessions`
//!   removes the session from the runtime and the store serves it.
//! - `subscribe`: forwards a session's records to one subscription.
//! - `ipc`: the local socket (or named pipe) `vesseld attach` reaches.
//! - `attach`: that proxy, which also starts the node when none runs.
//! - `lifecycle`: `node.stop` and draining (protocol 3).
//! - `fleet`: fleet sync (ADR 0012): the home publishing `fleet.toml`'s
//!   sources and serving `fleet.*`, members pulling and applying into every
//!   profile's home; `init`, `join`, `leave`, `status`, `restore` for the
//!   CLI.
//! - `cleanup`: `vesseld cleanup`, removing installed binaries nothing
//!   needs (ADR 0008).
//! - `node`, `shutdown`: start (taking the data directory's node lock), and
//!   stop in an order that loses nothing.
//!
//! [Decision Log]
//! - Purpose: the node is the product (ARCHITECTURE "Design rules"); it must
//!   keep every record, survive slow and vanishing clients, and stop
//!   without leaving work half done.
//! - Persistence policy lives here, not in the runtime (ADR 0001): the
//!   runtime hands over records on a bounded channel, and this crate
//!   decides how they are committed and when a reader may rely on them.
//! - "Durable through N": `persist` publishes, per session, the sequence
//!   number below which every record is committed. Gap fills and
//!   `session.read` wait on it, because a record can leave the runtime's
//!   ring before the store has committed it; reading the store without that
//!   wait could skip it.
//! - Shutdown order: stop accepting; end sessions (they publish
//!   `Interrupted`); wait until everything is committed; let connections
//!   flush their last frames; close the store. Subscribers therefore see
//!   the final status, and a restart finds every log complete.

mod agents;
pub mod attach;
mod auth;
pub mod cleanup;
mod config;
mod devices;
mod envelope;
mod error;
pub mod fleet;
mod guard;
mod identity;
mod ipc;
mod lifecycle;
mod lsp;
mod lsp_config;
mod models;
mod node;
mod persist;
mod principal;
mod profiles;
mod relays;
mod secret_copy;
mod secure;
mod server;
mod service;
mod sessions;
mod shutdown;
mod subscribe;
mod transfers;
mod transport;
mod waits;

pub use config::{Config, VesselMcp};

/// This binary's version and target triple, as `vesseld version` prints
/// them: what the remote installer checks (ADR 0008).
pub const VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), " ", env!("VESSEL_TARGET"));
pub use error::NodeError;
pub use fleet::{FleetConfig, FleetTargets};
pub use lsp::LspRoot;
pub use lsp_config::{LspConfigError, load_lsp_config};
pub use models::ModelsError;
pub use node::Node;
pub use profiles::ProfilesFileError;
pub use shutdown::signal;
// The store types named by `Config`'s public fields, so callers can build
// a `Config` without depending on `vessel-store` themselves.
pub use vessel_store::{ByteCount, DataDir, NodeToken, Retention, StoreConfig, StoreError};
