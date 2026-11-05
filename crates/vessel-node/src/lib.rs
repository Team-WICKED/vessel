//! `vesseld`: one node per machine, serving its harness sessions over
//! protocol 2.
//!
//! This crate assembles the others; it holds wiring and node policy, not
//! mechanisms:
//!
//! ```text
//!  client --ws /rpc (bearer token)--> server ---> vessel-rpc Server
//!                                                    | Handler
//!                                                 service (routing only)
//!                                                    |
//!            sessions: live runtime sessions + stored ones from earlier runs
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
//!   publishes how far it is committed.
//! - `subscribe`: forwards a session's records to one subscription.
//! - `node`, `shutdown`: start, and stop in an order that loses nothing.
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

mod auth;
mod config;
mod error;
mod identity;
mod node;
mod persist;
mod server;
mod service;
mod sessions;
mod shutdown;
mod subscribe;
mod transport;

pub use config::Config;
pub use error::NodeError;
pub use node::Node;
pub use shutdown::signal;
pub use vessel_store::{DataDir, NodeToken, StoreError};
