//! [`SecureConnector`]: a paired node, reached over its routes (plan P3,
//! P5; decision 6).
//!
//! [Decision Log]
//! - Purpose: a paired node is reached like any other, through a
//!   [`Connector`], so [`Pinned`](crate::Pinned) and
//!   [`follow_resuming`](crate::follow_resuming) work over a secure channel
//!   unchanged (P5: a dropped relay resumes from the cursor; calls in
//!   flight fail with `Disconnected` and nothing is retried).
//! - Route order (plan decision 6): every direct address first, then every
//!   relay, in the order the book gives. Every connect starts again from
//!   the first route. Probing all routes and keeping the fastest (Paseo)
//!   is a later refinement; remembering the route that worked would skip
//!   a direct address that has come back.
//! - Timeouts: a direct dial gets 3 s. Direct addresses go stale (a laptop
//!   that left the LAN, a changed public address), and a dropped TCP
//!   connect would otherwise hold back the relays for the system's
//!   timeout, minutes. A relay dial gets 10 s: a relay is expected to be
//!   up, and its path may be slow. The plan sets only the direct bound;
//!   the relay one is ours, so a black-holed relay cannot stall a
//!   resuming stream. The secure handshake (10 s, which covers the relay's
//!   wait for the node to accept) and the preamble have their own.
//! - What a failure means:
//!   - before the handshake completes, a failure is about the route (the
//!     network, the relay, whoever holds that address now), so the next
//!     route is tried;
//!   - the node's `4401` means whoever answered does not hold the pinned
//!     key: final, pair again;
//!   - the relay's codes are as `RelayClose::is_transient` says (`4404`
//!     while the node re-registers, `4408`, `4429` transient; `4413`
//!     final);
//!   - once the handshake succeeds the node has proved its key, so its
//!     answer holds for every route: a refusal (`Revoked`, `UnknownDevice`
//!     final; `Unavailable` transient) or a rejected hello ends the search.
//! - All routes failed: final if any failure was final, else transient.
//!   Evidence of a wrong key, a refused device or tampering outweighs a
//!   route that was only down, since waiting would hide it and the remedy
//!   (pairing again, or fixing the book) is the same whichever route
//!   showed it. Costs: a stale direct address now held by another node,
//!   while the relay is briefly away, ends a resuming stream that waiting
//!   would have kept; the message names the route. And a re-keyed node
//!   registers at relays under a new routing id, so a client with only
//!   relay routes sees `4404` (transient) and keeps retrying; only a
//!   direct route can tell it the key changed.

use std::time::Duration;

use vessel_proto::{ClientInfo, NodeClose, RoutingId};
use vessel_rpc::ClientConfig;
use vessel_secure::SecureError;

use super::dial::{Dialer, PeerClose};
use super::route::{ReachError, Route, RouteError, RouteFailure};
use super::{SecureConnectError, open_with};
use crate::address::SecureTarget;
use crate::client::NodeClient;
use crate::connector::Connector;
use crate::credential::DeviceCredential;
use crate::error::chain;

/// How long a dial may take before the next route is tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteTimeouts {
    /// A direct address (plan decision 6: 3 s).
    pub direct: Duration,
    /// A relay.
    pub relay: Duration,
}

impl Default for RouteTimeouts {
    fn default() -> Self {
        Self {
            direct: Duration::from_secs(3),
            relay: Duration::from_secs(10),
        }
    }
}

/// Connects to a paired node: a route, the secure handshake to its pinned
/// key, the device's credential, then `hello`.
#[derive(Debug)]
pub struct SecureConnector<D> {
    dialer: D,
    target: SecureTarget,
    routing_id: RoutingId,
    credential: DeviceCredential,
    config: ClientConfig,
    secure: vessel_secure::Config,
    timeouts: RouteTimeouts,
}

impl<D> SecureConnector<D> {
    /// Reach the node at `target` through `dialer`, presenting
    /// `credential`, with default limits.
    #[must_use]
    pub fn new(dialer: D, target: SecureTarget, credential: DeviceCredential) -> Self {
        let routing_id = vessel_secure::routing_id(&target.key);
        Self {
            dialer,
            target,
            routing_id,
            credential,
            config: ClientConfig::default(),
            secure: vessel_secure::Config::default(),
            timeouts: RouteTimeouts::default(),
        }
    }

    /// The same, with explicit protocol limits.
    #[must_use]
    pub fn with_config(mut self, config: ClientConfig) -> Self {
        self.config = config;
        self
    }

    /// The same, with explicit secure channel limits.
    #[must_use]
    pub const fn with_secure_config(mut self, config: vessel_secure::Config) -> Self {
        self.secure = config;
        self
    }

    /// The same, with other dial timeouts.
    #[must_use]
    pub const fn with_timeouts(mut self, timeouts: RouteTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Where it connects.
    #[must_use]
    pub const fn target(&self) -> &SecureTarget {
        &self.target
    }

    /// The dialer.
    #[must_use]
    pub const fn dialer(&self) -> &D {
        &self.dialer
    }

    /// Every route, in the order they are tried.
    fn routes(&self) -> impl Iterator<Item = Route> + '_ {
        let direct = self.target.direct.iter().cloned().map(Route::Direct);
        let relays = self.target.relays.iter().cloned().map(Route::Relay);
        direct.chain(relays)
    }
}

impl<D: Dialer> SecureConnector<D> {
    /// One route, start to finish.
    async fn attempt(
        &self,
        route: &Route,
        client: ClientInfo,
    ) -> Result<NodeClient, RouteError<D::Error>> {
        let dialed = match route {
            Route::Direct(url) => {
                let limit = self.timeouts.direct;
                tokio::time::timeout(limit, self.dialer.direct(url))
                    .await
                    .map_err(|_elapsed| RouteError::DialTimeout(limit))?
            }
            Route::Relay(url) => {
                let limit = self.timeouts.relay;
                tokio::time::timeout(limit, self.dialer.relay(url, &self.routing_id))
                    .await
                    .map_err(|_elapsed| RouteError::DialTimeout(limit))?
            }
        };
        let carrier = dialed.map_err(|error| RouteError::Dial {
            transient: self.dialer.is_transient(&error),
            error,
        })?;
        open_with(
            carrier,
            &self.target.key,
            &self.credential,
            client,
            &self.config,
            &self.secure,
        )
        .await
        .map_err(|error| self.read_close(error))
    }

    /// A carrier that failed during the handshake may say why with a close
    /// code; the dialer knows how to read it.
    fn read_close(&self, error: SecureConnectError) -> RouteError<D::Error> {
        if let SecureConnectError::Channel(SecureError::Carrier(io)) = &error {
            match self.dialer.closed_with(io) {
                Some(PeerClose::Node(NodeClose::HandshakeFailed)) => return RouteError::WrongKey,
                Some(PeerClose::Relay(close)) => return RouteError::Relay(close),
                None => {}
            }
        }
        RouteError::Open(error)
    }
}

impl<D: Dialer> Connector for SecureConnector<D> {
    type Error = ReachError<D::Error>;

    async fn connect(&self, client: ClientInfo) -> Result<NodeClient, Self::Error> {
        let mut failures = Vec::new();
        for route in self.routes() {
            match self.attempt(&route, client.clone()).await {
                Ok(connection) => {
                    tracing::debug!(%route, "connected to the paired node");
                    return Ok(connection);
                }
                Err(error) => {
                    tracing::debug!(%route, error = %chain(&error), "route failed");
                    let answered = error.is_answer();
                    failures.push(RouteFailure { route, error });
                    if answered {
                        break;
                    }
                }
            }
        }
        Err(ReachError::new(failures))
    }

    fn is_transient(&self, error: &Self::Error) -> bool {
        !error.is_final()
    }
}
