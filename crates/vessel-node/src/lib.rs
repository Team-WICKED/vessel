//! `vesseld`: one node per machine, serving its harness sessions over
//! the protocol (versions 2 and 3).
//!
//! This crate assembles the others; it holds wiring and node policy, not
//! mechanisms:
//!
//! ```text
//!  client --ws /rpc (bearer token)-----------------> server ---+
//!  ssh -> vesseld attach --local socket (token)----> ipc ------+--> vessel-rpc Server
//!                                                                    | Handler
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
//! - `service`: the `Handler` implementation; routes each call, maps typed
//!   errors to wire errors once.
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
mod node;
mod persist;
mod principal;
mod profiles;
mod secure;
mod server;
mod service;
mod sessions;
mod shutdown;
mod subscribe;
mod transfers;
mod transport;

pub use config::{Config, VesselMcp};

/// This binary's version and target triple, as `vesseld version` prints
/// them: what the remote installer checks (ADR 0008).
pub const VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), " ", env!("VESSEL_TARGET"));
pub use error::NodeError;
pub use fleet::{FleetConfig, FleetTargets};
pub use node::Node;
pub use profiles::ProfilesFileError;
pub use shutdown::signal;
// The store types named by `Config`'s public fields, so callers can build
// a `Config` without depending on `vessel-store` themselves.
pub use vessel_store::{ByteCount, DataDir, NodeToken, Retention, StoreConfig, StoreError};
