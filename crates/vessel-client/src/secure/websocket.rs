//! [`WsDialer`]: the real [`Dialer`], websockets through `vessel-relay`.
//!
//! Direct routes reach the node's `/secure` (`vessel_relay::connect`);
//! relay routes go through `vessel_relay::dial`. Close codes come back
//! inside carrier errors and are read with `vessel_relay::node_close` and
//! `relay_close`.
//!
//! [Decision Log]
//! - TLS (plan decision 1): rustls with `ring` and the system's root
//!   certificates, set up by `vessel-relay`'s [`TlsConfig`]. Only a
//!   `wss://` route needs them, so [`WsDialer::for_routes`] accepts a
//!   machine without roots as long as every route is `ws://` (the channel
//!   is end to end either way; TLS only gets a connection through a proxy
//!   on 443).

use std::io;

use vessel_proto::{RouteUrl, RoutingId};
use vessel_relay::{DialError, TlsConfig, TlsError, WsCarrier};

use super::dial::{Dialer, PeerClose};

/// The path of a node's secure endpoint, after a direct route's address.
const SECURE_PATH: &str = "/secure";

/// Opens websocket carriers to paired nodes and relays.
#[derive(Debug, Clone)]
pub struct WsDialer {
    tls: TlsConfig,
}

impl WsDialer {
    /// Verify `wss://` routes with `tls`.
    #[must_use]
    pub const fn new(tls: TlsConfig) -> Self {
        Self { tls }
    }

    /// A dialer for `routes`, with the system's root certificates. If none
    /// can be loaded, `ws://` routes still work and a `wss://` route is an
    /// error now, rather than at every dial.
    ///
    /// # Errors
    /// Some route is `wss://` and no root certificate could be loaded.
    pub fn for_routes<'a>(
        mut routes: impl Iterator<Item = &'a RouteUrl>,
    ) -> Result<Self, TlsError> {
        match TlsConfig::native_roots() {
            Ok(tls) => Ok(Self::new(tls)),
            Err(error) if routes.any(|route| route.as_str().starts_with("wss://")) => Err(error),
            Err(error) => {
                tracing::debug!(%error, "no TLS roots; every route is ws://");
                TlsConfig::with_roots(rustls::RootCertStore::empty()).map(Self::new)
            }
        }
    }
}

impl Dialer for WsDialer {
    type Carrier = WsCarrier;
    type Error = DialError;

    async fn direct(&self, route: &RouteUrl) -> Result<WsCarrier, DialError> {
        let url = format!("{}{SECURE_PATH}", route.as_str().trim_end_matches('/'));
        vessel_relay::connect(&url, &self.tls).await
    }

    async fn relay(&self, route: &RouteUrl, node: &RoutingId) -> Result<WsCarrier, DialError> {
        vessel_relay::dial(route, node, &self.tls).await
    }

    fn is_transient(&self, error: &DialError) -> bool {
        error.is_transient()
    }

    fn closed_with(&self, error: &io::Error) -> Option<PeerClose> {
        vessel_relay::node_close(error)
            .map(PeerClose::Node)
            .or_else(|| vessel_relay::relay_close(error).map(PeerClose::Relay))
    }
}
