//! A typed client for one Vessel node.
//!
//! [`NodeClient`] connects to a node's `/rpc` websocket with the node's
//! bearer token, then offers one async method per protocol call, typed end
//! to end. [`NodeClient::follow`] turns a session's event log into one
//! gap-free [`SessionStream`]: it subscribes, and when the subscription
//! reports that it fell behind, it pages the missed records with
//! `session.read` and subscribes again, so a caller never sees a skipped or
//! repeated record.
//!
//! No command-line parsing and no printing: the CLI, a web backend or an
//! editor plugin all build on this the same way.
//!
//! [Decision Log]
//! - Purpose: every client needs the same connection setup, the same typed
//!   calls, and the same catch-up logic for streams; written once here.
//! - Alternatives: let each client drive `vessel-rpc` directly (catch-up
//!   logic copied into each, and each would get it subtly wrong); hide lag
//!   by buffering without bound (memory grows with the slowest reader).
//! - Choice: thin typed methods over `vessel_rpc::Connection`, plus a stream
//!   that switches between live delivery and paged reads by sequence
//!   number. Lag costs a few reads, never memory.
//! - Transport: `tokio-tungstenite`, adapted to `vessel-rpc`'s frame
//!   transport in [`ws`]. Any other transport can be used through
//!   [`NodeClient::over`].

mod client;
mod error;
mod stream;
#[cfg(test)]
mod tests;
mod token;
mod url;
mod ws;

pub use client::NodeClient;
pub use error::{ConnectError, StreamError};
pub use stream::SessionStream;
pub use token::{BearerToken, InvalidToken};
pub use url::{InvalidNodeUrl, NodeUrl};
pub use vessel_rpc::{CallError, ClientConfig, Subscription, SubscriptionItem};
pub use ws::WsTransportError;
