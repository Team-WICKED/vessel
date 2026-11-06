//! The client's side: a carrier to a node through a relay.

use std::io;
use std::time::Duration;

use vessel_proto::{RouteUrl, RoutingId};

use crate::carrier::WsCarrier;
use crate::tls::TlsConfig;
use crate::{CONNECT_PATH, endpoint, socket};

/// How long a relay connection may stay silent: the relay pings every 10 s.
pub(crate) const IDLE: Duration = Duration::from_secs(30);

/// Why a relay could not be reached.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DialError {
    /// The connection, TLS, or the websocket upgrade failed.
    #[error("could not reach the relay")]
    Connect(#[source] io::Error),
    /// The relay answered the upgrade with an HTTP error.
    #[error("the relay answered HTTP {status}")]
    Http {
        /// The status code.
        status: u16,
    },
}

impl DialError {
    /// Whether trying again later may succeed: the network failed or the
    /// relay had a server error. A 4xx answer means the request itself is
    /// wrong (a bad routing id, a wrong path prefix).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connect(_) => true,
            Self::Http { status } => *status >= 500,
        }
    }
}

/// A carrier to the node with `node` as routing id, through `relay`.
///
/// The relay may still refuse after the upgrade: the carrier's first read
/// then fails with a [`RelayClosed`](crate::RelayClosed) close code (no such
/// node, not accepted in time, rate limited). Run
/// [`vessel_secure::initiate`] over the carrier to the node's key.
///
/// # Errors
/// The relay could not be reached.
pub async fn dial(
    relay: &RouteUrl,
    node: &RoutingId,
    tls: &TlsConfig,
) -> Result<WsCarrier, DialError> {
    let url = endpoint(relay, &format!("{CONNECT_PATH}?node={node}"));
    let socket = socket::open(&url, tls).await?;
    Ok(WsCarrier::new(socket, Some(IDLE)))
}

/// A carrier to a node's own secure endpoint (`ws://host:port/secure`, or
/// `wss://` behind a proxy), for direct routes.
///
/// Without a relay nobody pings, so the carrier has no idle limit; a node
/// that pings its `/secure` connections can be given one later.
///
/// # Errors
/// The node could not be reached.
pub async fn connect(url: &str, tls: &TlsConfig) -> Result<WsCarrier, DialError> {
    let socket = socket::open(url, tls).await?;
    Ok(WsCarrier::new(socket, None))
}
