//! The node's links to relays (ADR 0009): the ones it was started with
//! (`--relay`), and, on a fleet member with no route of its own, its
//! home's, added while it runs (`fleet.callback`).
//!
//! [Decision Log]
//! - Purpose: a member's home calls it through the routes its grant's link
//!   names (ADR 0013, "across nodes through the home"). A member started
//!   plainly (`vessel node start`, or the service `vessel init --service`
//!   installs, neither with `--relay` or `--public`) named none, so the
//!   home refused its grant and could never call it (`--to`, `vessel mcp`'s
//!   `node`, `secret.copy`), although it synced (Track S's fresh-machine
//!   smoke).
//! - Constraint: nothing is guessed about a node's addresses (ADR 0009); a
//!   relay needs its registration token; and the link tasks need the
//!   secure gateway, which needs the node's service, which holds the
//!   fleet's links that ask for the relays.
//! - Alternatives: the member taking routes at `vessel init --join` and
//!   keeping them in a file (a second copy of the home's relay token on
//!   disk, stale once the home changes relays, and still nothing for a
//!   running node that joins); the token in the invitation (a long-lived
//!   secret in a link that is shown as a QR code and pasted); the home
//!   relaying for its members itself, or calling them back over the
//!   connection they keep to it (a relay inside every home, or the
//!   connection's roles reversed: a larger change, for later).
//! - Choice: a member with no route of its own asks its home on every
//!   connection (`fleet.callback`, `sync`, only to a member device) and
//!   registers with the home's relays with the home's token, kept in
//!   memory only; they join the routes pairing links name. The links are
//!   made at once and run when the node serves, so a relay asked for
//!   while the node is still starting is not lost.
//! - Lifetime (as before this module): one task per relay, so a slow relay
//!   does not hold the others back; links stop with the node's stop
//!   signal, so no new client arrives while it shuts down, and they and the
//!   connections they deliver are tracked with the node's connections, so
//!   shutdown waits for them. Links are not started lazily on a first
//!   pairing: a node paired before a restart would be unreachable until
//!   then.
//! - Trade-offs: every member holds the home's relay token (registering
//!   still needs a proof of the node's own key, so a member can register
//!   its own nodes there, not take another's slot); a home with no relay
//!   has none to share, and its members say so and how to add a route; a
//!   relay, once linked, stays linked until the node stops, a membership
//!   that ended included (nothing it holds lets the former home in).

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_proto::{RelayToken, RouteUrl};
use vessel_relay::{LinkState, NodeLink, TlsConfig, WsCarrier};
use vessel_secure::NodeKeypair;

use crate::devices::Devices;
use crate::error::NodeError;
use crate::secure::SecureGateway;

/// The node's relay links, and the routes pairing links name for them.
#[derive(Debug)]
pub(crate) struct Relays {
    keypair: Arc<NodeKeypair>,
    devices: Arc<Devices>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Every relay linked, or waiting for the node to serve, with where its
    /// link stands.
    links: Vec<(RouteUrl, watch::Receiver<LinkState>)>,
    /// Links made before the node served; they run once it does.
    waiting: Vec<NodeLink>,
    /// What links run with, once the node serves.
    serving: Option<Serving>,
}

/// What a link needs to run: the gateway its clients reach, the tracker
/// shutdown waits on, and the node's stop signal.
#[derive(Debug)]
struct Serving {
    gateway: SecureGateway,
    connections: TaskTracker,
    stop: CancellationToken,
}

impl Serving {
    /// Run `link` until the node stops: each client the relay splices to
    /// the node takes the same path as a direct `/secure` connection.
    fn run(&self, link: NodeLink) {
        let gateway = self.gateway.clone();
        let tracker = self.connections.clone();
        let accept = move |carrier: WsCarrier| {
            let gateway = gateway.clone();
            drop(tracker.spawn(async move {
                gateway.accept_logged(carrier, "relay").await;
            }));
        };
        drop(self.connections.spawn(link.run(accept, self.stop.clone())));
    }
}

impl Relays {
    pub(crate) fn new(keypair: Arc<NodeKeypair>, devices: Arc<Devices>) -> Self {
        Self {
            keypair,
            devices,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run the links, those made so far and any made from now on, with the
    /// node's gateway, until `stop`. Once, when the node serves.
    pub(crate) fn serve(
        &self,
        gateway: SecureGateway,
        connections: TaskTracker,
        stop: CancellationToken,
    ) {
        let serving = Serving {
            gateway,
            connections,
            stop,
        };
        let mut state = self.state();
        for link in std::mem::take(&mut state.waiting) {
            serving.run(link);
        }
        state.serving = Some(serving);
    }

    /// Link to each of `relays` not linked yet, registering with `token`
    /// (TLS for `wss://` ones from the system's roots), and add them to the
    /// routes pairing links name; where each of `relays` stands.
    ///
    /// # Errors
    /// A `wss://` relay and no TLS roots on this machine.
    pub(crate) fn link(
        &self,
        relays: &[RouteUrl],
        token: &RelayToken,
    ) -> Result<Vec<watch::Receiver<LinkState>>, NodeError> {
        let tls = relay_tls(relays)?;
        Ok(self.link_with(relays, token, &tls))
    }

    /// [`link`](Self::link) with `tls` given (the node's start-up checks
    /// it before anything starts).
    pub(crate) fn link_with(
        &self,
        relays: &[RouteUrl],
        token: &RelayToken,
        tls: &TlsConfig,
    ) -> Vec<watch::Receiver<LinkState>> {
        let mut states = Vec::with_capacity(relays.len());
        let mut added = Vec::new();
        {
            let mut state = self.state();
            for relay in relays {
                if let Some((_, linked)) = state.links.iter().find(|(url, _)| url == relay) {
                    states.push(linked.clone());
                    continue;
                }
                let link = NodeLink::new(
                    relay.clone(),
                    Arc::clone(&self.keypair),
                    token.clone(),
                    tls.clone(),
                );
                let linked = link.state();
                state.links.push((relay.clone(), linked.clone()));
                states.push(linked);
                added.push(relay.clone());
                tracing::info!(%relay, "linking to a relay");
                match &state.serving {
                    Some(serving) => serving.run(link),
                    None => state.waiting.push(link),
                }
            }
        }
        self.devices.add_relays(&added);
        states
    }

    /// Each relay linked and where its link stands.
    pub(crate) fn states(&self) -> Vec<(RouteUrl, watch::Receiver<LinkState>)> {
        self.state().links.clone()
    }
}

/// TLS for `wss://` relays: the system's root certificates (plan, decision
/// 1). Only a `wss://` relay needs them, so a machine without any can
/// still use `ws://` relays.
pub(crate) fn relay_tls(relays: &[RouteUrl]) -> Result<TlsConfig, NodeError> {
    match TlsConfig::native_roots() {
        Ok(tls) => Ok(tls),
        Err(error)
            if relays
                .iter()
                .any(|relay| relay.as_str().starts_with("wss://")) =>
        {
            Err(NodeError::RelayTls(error))
        }
        Err(error) => {
            tracing::debug!(%error, "no TLS roots; only ws:// relays are configured");
            TlsConfig::with_roots(rustls::RootCertStore::empty()).map_err(NodeError::RelayTls)
        }
    }
}
