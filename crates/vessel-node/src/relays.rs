//! The node's links to relays (ADR 0009): the ones it was started with
//! (`--relay`, with the relays' token), and, on a fleet member with no
//! route of its own, its current home's (`fleet.callback`, with tickets for
//! its own key), kept apart.
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
//!   relay registers a node with its token or a ticket for the node's key;
//!   and the link tasks need the secure gateway, which needs the node's
//!   service, which holds the fleet's links that ask for the relays.
//! - First built (1c1ae14) with the home's relay token handed to members
//!   and the home's relays added to every pairing link; the review's round
//!   6 showed any `sync` device got the token, kept it after revocation,
//!   and could register any key, and that a member moving homes kept the
//!   old home's relay for good. Now:
//! - Choice: the configured links run for good with the token. The
//!   callback links belong to one home at a time: they register with a
//!   ticket for this node's key that the member replaces before it expires
//!   (each link registers again with the newer one), they are named only in
//!   the grant's link to that home, never in the node's own pairing links,
//!   and a callback from another home, or none (the node left), stops them.
//!   Links made before the node serves run once it does, so a relay asked
//!   for while the node is still starting is not lost.
//! - Lifetime (as before this module): one task per relay, so a slow relay
//!   does not hold the others back; links stop with the node's stop
//!   signal (a callback link also with its own), so no new client arrives
//!   while it shuts down, and they and the connections they deliver are
//!   tracked with the node's connections, so shutdown waits for them. Links
//!   are not started lazily on a first pairing: a node paired before a
//!   restart would be unreachable until then.
//! - Alternatives: the routes in a file from `vessel init --join` (stale,
//!   and nothing for a node that joins while it runs); the token in the
//!   invitation (a long-lived secret in a link shown as a QR code); the
//!   home calling members back over their connection to it (the plan says
//!   why not yet).

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_proto::{NodeId, RelayTicket, RelayToken, RouteUrl};
use vessel_relay::{LinkState, NodeLink, Reach, TlsConfig, WsCarrier};
use vessel_secure::NodeKeypair;

use crate::error::NodeError;
use crate::secure::{Route, SecureGateway, relay_budget};

/// The node's relay links.
#[derive(Debug)]
pub(crate) struct Relays {
    keypair: Arc<NodeKeypair>,
    /// The node's stop signal; each link stops on a child of it.
    stop: CancellationToken,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The configured relays, linked for good, with where each stands.
    configured: Vec<(RouteUrl, watch::Receiver<LinkState>)>,
    /// The current home's relays, on a member with no route of its own.
    callback: Option<CallbackLinks>,
    /// Links made before the node served, with what stops each (a child
    /// of the node's stop); they run once it does.
    waiting: Vec<(NodeLink, CancellationToken)>,
    /// What links run with, once the node serves.
    serving: Option<Serving>,
}

/// A home's relays, as a member links to them.
#[derive(Debug)]
struct CallbackLinks {
    home: NodeId,
    /// The newest ticket, which every link registers with.
    tickets: watch::Sender<RelayTicket>,
    /// Each relay, where its link stands, and what stops it.
    links: Vec<(RouteUrl, watch::Receiver<LinkState>, CancellationToken)>,
}

impl CallbackLinks {
    fn stop(self) {
        for (relay, _state, stop) in self.links {
            tracing::info!(%relay, home = %self.home, "unlinking from a home's relay");
            stop.cancel();
        }
    }
}

/// What a link needs to run: the gateway its clients reach, and the
/// tracker shutdown waits on.
#[derive(Debug)]
struct Serving {
    gateway: SecureGateway,
    connections: TaskTracker,
}

impl Serving {
    /// Run `link` until `stop` (a child of the node's stop): each client
    /// the relay splices to the node takes the same path as a direct
    /// `/secure` connection, waiting for admission in this link's own
    /// budget (gone with the link; `secure`'s round-10 Decision Log).
    fn run(&self, link: NodeLink, stop: CancellationToken) {
        let gateway = self.gateway.clone();
        let tracker = self.connections.clone();
        let budget = relay_budget();
        let accept = move |carrier: WsCarrier| {
            let gateway = gateway.clone();
            let route = Route::Relayed(Arc::clone(&budget));
            drop(tracker.spawn(async move {
                gateway.accept_logged(carrier, route).await;
            }));
        };
        drop(self.connections.spawn(link.run(accept, stop)));
    }
}

impl State {
    /// Run `link` now, or once the node serves.
    fn start(&mut self, link: NodeLink, stop: CancellationToken) {
        match &self.serving {
            Some(serving) => serving.run(link, stop),
            None => self.waiting.push((link, stop)),
        }
    }
}

impl Relays {
    pub(crate) fn new(keypair: Arc<NodeKeypair>, stop: CancellationToken) -> Self {
        Self {
            keypair,
            stop,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run the links, those made so far and any made from now on, with the
    /// node's gateway. Once, when the node serves.
    pub(crate) fn serve(&self, gateway: SecureGateway, connections: TaskTracker) {
        let serving = Serving {
            gateway,
            connections,
        };
        let mut state = self.state();
        for (link, stop) in std::mem::take(&mut state.waiting) {
            serving.run(link, stop);
        }
        state.serving = Some(serving);
    }

    /// Link to the configured `relays` for good, registering with `token`.
    pub(crate) fn link_configured(&self, relays: &[RouteUrl], token: &RelayToken, tls: &TlsConfig) {
        let mut state = self.state();
        for relay in relays {
            let link = NodeLink::new(
                relay.clone(),
                Arc::clone(&self.keypair),
                token.clone(),
                tls.clone(),
            );
            state.configured.push((relay.clone(), link.state()));
            tracing::info!(%relay, "linking to a relay");
            state.start(link, self.stop.child_token());
        }
    }

    /// Link to `home`'s `relays` with `ticket` (for this node's key), as a
    /// member with no route of its own, each reaching only the addresses
    /// its [`Reach`] allows: a newer ticket for the relays already linked
    /// (each registers again with it), links for new ones, and none any
    /// more for relays the home no longer names, or for another home's.
    /// Where each of `relays` stands.
    ///
    /// # Errors
    /// A `wss://` relay and no TLS roots on this machine.
    pub(crate) fn link_callback(
        &self,
        home: NodeId,
        relays: &[(RouteUrl, Reach)],
        ticket: RelayTicket,
    ) -> Result<Vec<watch::Receiver<LinkState>>, NodeError> {
        let urls: Vec<RouteUrl> = relays.iter().map(|(relay, _reach)| relay.clone()).collect();
        let tls = relay_tls(&urls)?;
        let mut state = self.state();
        let mut callback = match state.callback.take() {
            Some(current) if current.home == home => {
                let _previous = current.tickets.send_replace(ticket);
                current
            }
            other => {
                if let Some(other) = other {
                    other.stop();
                }
                let (tickets, _receiver) = watch::channel(ticket);
                CallbackLinks {
                    home,
                    tickets,
                    links: Vec::new(),
                }
            }
        };
        let (kept, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut callback.links)
            .into_iter()
            .partition(|(relay, _, _)| urls.contains(relay));
        for (relay, _state, stop) in dropped {
            tracing::info!(%relay, %home, "unlinking from a relay the home no longer names");
            stop.cancel();
        }
        callback.links = kept;
        let mut states = Vec::with_capacity(relays.len());
        for (relay, reach) in relays {
            if let Some((_, linked, _)) = callback.links.iter().find(|(url, _, _)| url == relay) {
                states.push(linked.clone());
                continue;
            }
            let link = NodeLink::with_tickets(
                relay.clone(),
                Arc::clone(&self.keypair),
                callback.tickets.subscribe(),
                tls.clone(),
            )
            .reaching(*reach);
            let linked = link.state();
            let stop = self.stop.child_token();
            callback
                .links
                .push((relay.clone(), linked.clone(), stop.clone()));
            states.push(linked);
            tracing::info!(%relay, %home, "linking to the home's relay, to be called back");
            state.start(link, stop);
        }
        state.callback = Some(callback);
        Ok(states)
    }

    /// The relays this node is linked to for `home`'s calls back, if any.
    pub(crate) fn callback_relays(&self, home: NodeId) -> Vec<RouteUrl> {
        self.state()
            .callback
            .as_ref()
            .filter(|callback| callback.home == home)
            .map(|callback| {
                callback
                    .links
                    .iter()
                    .map(|(relay, _, _)| relay.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Stop the callback links unless they are `home`'s (`None`: the node
    /// follows no home any more).
    pub(crate) fn keep_callback_of(&self, home: Option<NodeId>) {
        let mut state = self.state();
        if state
            .callback
            .as_ref()
            .is_some_and(|callback| Some(callback.home) != home)
            && let Some(callback) = state.callback.take()
        {
            callback.stop();
        }
    }

    /// Each relay linked, configured ones first, and where its link stands.
    pub(crate) fn states(&self) -> Vec<(RouteUrl, watch::Receiver<LinkState>)> {
        let state = self.state();
        let mut states = state.configured.clone();
        if let Some(callback) = &state.callback {
            states.extend(
                callback
                    .links
                    .iter()
                    .map(|(relay, linked, _)| (relay.clone(), linked.clone())),
            );
        }
        states
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
