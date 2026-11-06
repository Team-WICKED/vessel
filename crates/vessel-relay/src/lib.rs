//! Vessel's relay (ADR 0009): a dumb server that splices a client's
//! websocket to a node's, the node's link that keeps it registered, and the
//! client's dial.
//!
//! The relay forwards sealed records ([`vessel_secure`]) and never holds a
//! key: what it sees is routing ids, addresses, timing and sizes.
//!
//! ```text
//! GET /v1/node                     the node's control connection (JSON text)
//! GET /v1/accept?conn=<hex>        the node's data connection for one client
//! GET /v1/connect?node=<hex>       a client
//! GET /health
//! ```
//!
//! [Decision Log]
//! - Purpose: reach a node behind NAT without trusting the machine in the
//!   middle, and without a second security mode (ADR 0009).
//! - Constraint: the relay must not be able to read, forge or reorder
//!   anything unnoticed (the channel guarantees that), nor be made to buffer
//!   for strangers, nor let anyone take a node's slot.
//! - Alternatives: a multiplexed control connection carrying every client
//!   (stream ids, windows and buffers of its own); buffering a client's
//!   first messages until the node answers (Paseo keeps 200 and drops the
//!   oldest); no registration check (Paseo: anyone with a server id can
//!   replace the daemon's socket).
//! - Choice: one data websocket per client, spliced message by message, so
//!   TCP backpressure works end to end; nothing is read from a client until
//!   the node's data connection is there; registration needs the relay's
//!   token and a possession proof of the node's key
//!   ([`vessel_secure::proof_challenge`]).
//! - Trade-offs: a node opens one connection per client (cheap next to the
//!   isolation it buys); the relay keeps per-node and per-address state in
//!   memory, bounded by the limits ([`Limits`]).
//! - Edges: `-> proto` (the control messages), `-> secure` (the proof and
//!   routing ids). TLS (`wss://`) is a client-side matter for the node and
//!   the client; the relay speaks plain HTTP behind a reverse proxy.
//!
//! Layout: `server` is the relay; `link` is the node's side (`NodeLink`),
//! `dial` the client's (also for a node's `/secure`); both open websockets
//! through `socket` (with `tls`) and get a [`WsCarrier`] to run
//! [`vessel_secure`] over. `carrier` adapts websockets to the channel,
//! including the node's own `/secure` ([`AxumCarrier`]) and its handshake
//! ([`respond`], which tells a client with the wrong key so).

mod backoff;
mod carrier;
mod dial;
mod link;
mod reach;
mod server;
mod socket;
mod ticket;
mod tls;
mod token;

use vessel_proto::RouteUrl;

pub use carrier::{
    AxumCarrier, Keepalive, NodeCarrier, NodeClosed, RelayClosed, WsCarrier, node_close,
    relay_close, respond,
};
pub use dial::{DialError, connect, connect_with, dial, dial_with};
pub use link::{LinkState, NodeLink};
pub use reach::{Reach, is_public, numeric_host};
pub use server::{BadProxy, Limits, RelayConfig, TrustedProxy, serve};
pub use ticket::{MAX_TICKET_LIFETIME, TicketError, check_ticket, issue_ticket, unix_now};
pub use tls::{TlsConfig, TlsError};
pub use token::{TokenFileError, read_token_file};

/// The node's control connection.
pub const NODE_PATH: &str = "/v1/node";
/// The node's data connection for one waiting client.
pub const ACCEPT_PATH: &str = "/v1/accept";
/// A client's connection.
pub const CONNECT_PATH: &str = "/v1/connect";
/// Liveness for load balancers and monitors.
pub const HEALTH_PATH: &str = "/health";

/// The URL of `path` (with its query) on `relay`, which may carry a path
/// prefix of its own (a relay behind a reverse proxy at
/// `wss://example.com/vessel`).
fn endpoint(relay: &RouteUrl, path: &str) -> String {
    format!("{}{path}", relay.as_str().trim_end_matches('/'))
}
