//! Protocol 2 frames over any transport.
//!
//! This crate moves `vessel-proto`'s `ClientFrame`/`ServerFrame` over a
//! [`FrameTransport`]: a websocket, any byte stream through [`Framed`] (SSH
//! stdio, the node's local socket), an in-memory pair in tests, and the
//! relay later. It owns the mechanics every transport shares:
//! the `Hello`/`Welcome` handshake, request/response correlation,
//! cancellation, a bound on concurrent calls, one bounded outbound queue per
//! connection, and the lifecycle of event subscriptions.
//!
//! It does not know what a session is. The node's [`Handler`] decides what a
//! call means; this crate only guarantees how frames flow.
//!
//! [Decision Log]
//! - Purpose: one place that implements the protocol's flow rules, so the
//!   node, the client library and every future transport cannot disagree on
//!   them.
//! - Constraints: no untyped JSON (frames decode straight into the proto
//!   enums); every queue bounded; a slow or broken client must never stall
//!   other connections or take the node down.
//! - Alternatives: let the node and the client each talk to the websocket
//!   directly (two implementations of the same rules, and the SSH transport
//!   would need a third); a boxed `dyn` transport (a vtable call per frame,
//!   for a set of transports that is known at compile time).
//! - Choice: generic over the transport (static dispatch). Each connection is
//!   two cooperating futures in one task, a reader and a writer, so reading
//!   never waits for writing (otherwise two peers that both block on a full
//!   socket deadlock). Long-lived per-connection state (pending calls,
//!   subscription tables) is owned by exactly one of those futures and
//!   reached through channels, never through a shared lock.
//! - Subscriptions are first class here, not in the node or the client,
//!   because their ordering guarantee needs the writer: the server never
//!   writes an `Event` for a subscription before the `Subscribed` reply that
//!   announced it, and the client registers a subscription's route in the
//!   same step that reads that reply. Without both, the first events of a
//!   subscription can be lost to a race (see `server::outbound` and
//!   `client::driver`).
//!
//! [`Handler`]: vessel_proto::Handler

mod client;
mod error;
mod framed;
pub mod memory;
mod server;
#[cfg(test)]
mod tests;
mod transport;

pub use client::{ClientConfig, Connection, Subscription, SubscriptionItem, Unary};
pub use error::{CallError, ConnectError, OutboundError, ProtocolViolation, ServeError};
pub use framed::{Framed, FramedError, MAX_FRAME};
pub use server::{Outbound, Peer, Server, ServerConfig, SubscriptionClosed, SubscriptionSink};
pub use transport::FrameTransport;
