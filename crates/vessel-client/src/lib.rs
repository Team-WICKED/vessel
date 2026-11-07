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
//! A [`Connector`] opens connections to one node, again whenever asked:
//! [`WsConnector`] for a websocket, and `vessel-remote`'s SSH connector
//! (ADR 0007). [`follow_resuming`] builds on it: a [`ResumingStream`] that
//! dials again when the connection drops and continues from its cursor,
//! reporting the loss and the resume as non-fatal items; [`Pinned`] makes
//! sure it is the same node each time. [`NodeClient::connect_local`] reaches
//! a node on this machine through its local socket, with the node's token
//! or a session agent's credential (ADR 0013). Where a node is comes as a
//! [`NodeAddress`]: a websocket URL or an [`SshTarget`], by name from the
//! user's node [`book`].
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

mod address;
pub mod book;
mod chunks;
mod client;
mod connector;
mod credential;
mod error;
mod local;
pub mod pairing;
mod pinned;
mod resume;
pub mod secure;
mod stream;
#[cfg(test)]
mod tests;
mod token;
mod url;
mod ws;

pub use address::{
    InvalidAddress, InvalidRemotePath, InvalidSshTarget, NodeAddress, RemotePath, SecureTarget,
    SshTarget,
};
pub use chunks::{ChunkSink, FetchError, fetch_chunks};
pub use client::NodeClient;
pub use connector::{Connector, WsConnector};
pub use credential::{CredentialError, CredentialMismatch, CredentialSource, DeviceCredential};
pub use error::{ConnectError, StreamError};
pub use local::{LocalConnectError, LocalCredential};
pub use pinned::{Pinned, PinnedError};
pub use resume::{Backoff, ResumingStream, follow_resuming};
pub use stream::SessionStream;
pub use token::{BearerToken, InvalidToken};
pub use url::{InvalidNodeUrl, NodeUrl};
pub use vessel_rpc::{
    CallError, ClientConfig, NotUnary, Subscription, SubscriptionItem, Unary, UnaryCall,
};
/// The short fingerprint people compare to recognise a node's key.
pub use vessel_secure::fingerprint;
pub use ws::WsTransportError;
