//! [`Dialer`]: where the carriers to a paired node's routes come from.
//!
//! [Decision Log]
//! - Purpose: [`SecureConnector`](super::SecureConnector) decides which
//!   route to try, for how long, and what a failure means. Opening the
//!   carrier itself is not its job: the client's websocket dial and the
//!   relay's dial are `vessel-relay`'s (`dial`, `WsCarrier`), and tests use
//!   in-memory carriers.
//! - Constraint: the route order and the final-or-transient rules have to
//!   be written and tested before `vessel-relay` lands, and must not change
//!   when it does.
//! - Alternatives: call `vessel-relay` directly (not merged yet, and every
//!   connector test would need a websocket server); closures (three of
//!   them, with no name for their error type).
//! - Choice: a trait shaped like [`Connector`](crate::Connector): an
//!   associated carrier and error, `impl Future` methods, generic rather
//!   than `dyn`. Besides dialing it answers two questions only the carrier
//!   can: whether a failed dial is worth repeating, and which close code a
//!   carrier failure carries (the node's `4401`, a relay's `44xx`), since a
//!   websocket close code travels inside the carrier's `io::Error` and only
//!   its owner knows how (`vessel_relay::node_close`, `relay_close`).
//! - Cost: the relay's implementation is a few lines of glue in this crate
//!   once `vessel-relay` lands.

use std::future::Future;
use std::io;
use std::sync::Arc;

use vessel_proto::{NodeClose, RelayClose, RouteUrl, RoutingId};
use vessel_secure::Carrier;

/// Opens carriers along a paired node's routes.
///
/// Each call opens a new carrier. Neither method has to bound its own
/// time: the connector puts a timeout around every dial.
pub trait Dialer: Send + Sync {
    /// What a dial opens: binary messages to and from the node (a
    /// websocket, or a relay's spliced websocket).
    type Carrier: Carrier;
    /// Why a dial failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// A carrier to the node's `/secure` at a direct address, given as the
    /// pairing link or the book has it (`ws://host:port`); the dialer adds
    /// the path.
    fn direct(
        &self,
        route: &RouteUrl,
    ) -> impl Future<Output = Result<Self::Carrier, Self::Error>> + Send;

    /// A carrier to the node registered as `node` at the relay `route`.
    fn relay(
        &self,
        route: &RouteUrl,
        node: &RoutingId,
    ) -> impl Future<Output = Result<Self::Carrier, Self::Error>> + Send;

    /// Whether a later dial may succeed where this one failed (the network
    /// or the relay is down), rather than meet the same answer (an address
    /// that is not a websocket, a TLS certificate that does not verify).
    fn is_transient(&self, error: &Self::Error) -> bool {
        let _ = error;
        true
    }

    /// The close code a failed carrier ended with, if the far end closed
    /// it with one of ours. Carriers without close codes (in-memory ones)
    /// keep the default, `None`.
    fn closed_with(&self, error: &io::Error) -> Option<PeerClose> {
        let _ = error;
        None
    }
}

/// A close code from the far end of a carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClose {
    /// The node's (`4401`: the secure handshake failed).
    Node(NodeClose),
    /// The relay's (`4404`, `4408`, `4413`, `4429`).
    Relay(RelayClose),
}

impl<D: Dialer> Dialer for Arc<D> {
    type Carrier = D::Carrier;
    type Error = D::Error;

    fn direct(
        &self,
        route: &RouteUrl,
    ) -> impl Future<Output = Result<Self::Carrier, Self::Error>> + Send {
        D::direct(self, route)
    }

    fn relay(
        &self,
        route: &RouteUrl,
        node: &RoutingId,
    ) -> impl Future<Output = Result<Self::Carrier, Self::Error>> + Send {
        D::relay(self, route, node)
    }

    fn is_transient(&self, error: &Self::Error) -> bool {
        D::is_transient(self, error)
    }

    fn closed_with(&self, error: &io::Error) -> Option<PeerClose> {
        D::closed_with(self, error)
    }
}
