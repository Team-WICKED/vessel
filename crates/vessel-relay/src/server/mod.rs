//! The relay server: plain HTTP and websockets, for running behind an HTTPS
//! reverse proxy (`vesseld relay`).
//!
//! State is in memory only: which nodes are registered, which clients wait
//! for which node, and the per-address rate limiter. A restart forgets it
//! all; nodes register again on their own (`NodeLink`), and clients
//! reconnect.
//!
//! Every websocket task also watches the server's shutdown token: axum's
//! graceful shutdown stops accepting, but upgraded connections run in tasks
//! of their own.

mod accept;
mod control;
mod pairing;
mod rate;
mod throttle;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName};
use axum::routing::get;
use futures_util::SinkExt as _;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{ConnId, RelayClose, RelayToken, RoutingId};
use vessel_secure::MAX_RECORD;

use self::rate::RateLimiter;
use self::throttle::{Refusal, RefusalLog};
use crate::carrier::Keepalive;
use crate::{ACCEPT_PATH, CONNECT_PATH, HEALTH_PATH, NODE_PATH};

/// The relay's limits (ADR 0009). The defaults are the protocol's, and
/// the resource caps leave a fleet of tens of nodes plenty of room.
///
/// [Decision Log] The resource caps (the security review, round 6).
/// - Purpose: one token holder, or anyone opening connections, must not
///   be able to exhaust a relay's memory, file descriptors or bandwidth.
///   Before, only new connections per address were limited.
/// - Choice: counts, each checked and taken under one lock where it is
///   claimed, and given back by a guard when the connection ends:
///   registered nodes (a node registering again under its key replaces
///   itself and takes no second place), control connections still
///   registering, relayed clients per node and in all (waiting or
///   spliced); and a byte rate per direction of each relayed connection.
///   A refused node gets `rejected {reason: "full"}`, a refused client or
///   control connection the close code `4503` (`RelayClose::Full`), both
///   transient; the relay logs each kind at info at most every 10 s, with
///   the count in between, and never a token.
/// - Defaults: each relayed client holds two sockets here (its own and its
///   node's data connection) and each node one, so 128 nodes and 384
///   clients stay under the common open-file limit of 1024. Raise them
///   together with that limit (`ulimit -n`, systemd's `LimitNOFILE`).
/// - Alternatives: a relay-wide bandwidth budget (one lock on every
///   message; the per-connection rate times `max_clients` bounds it
///   already); refusing over the address limit only (many addresses get
///   around it).
/// - Round 7: places must not be held by doing nothing. Every socket is
///   counted from `accept` in all and per address (`server::accept`, with
///   a deadline on the request's headers); registering places and clients
///   are counted per address as well; a registering node and a spliced
///   client have `first_message_timeout` (2 s) to speak; and
///   `reserved_registering` places are kept for nodes that registered
///   before, so they get back in while strangers hold the rest.
///   `max_connections` (1000) keeps every socket under the 1024 budget,
///   whatever the other limits are set to.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Limits {
    /// The longest websocket message, checked from the frame header before
    /// the payload is read: one record.
    pub max_message: usize,
    /// Clients that may wait for one node at once.
    pub waiting_per_node: usize,
    /// How long a client waits for the node's data connection.
    pub accept_timeout: Duration,
    /// How long each registration step may take.
    pub registration_timeout: Duration,
    /// How often every connection is pinged.
    pub ping_interval: Duration,
    /// Silence after which a connection is dropped.
    pub idle_timeout: Duration,
    /// New connections an address may open at once...
    pub connect_burst: u32,
    /// ...and then one more every this long.
    pub connect_refill: Duration,
    /// Nodes registered at once. A node registering again takes its own
    /// place back; another is refused (`full`) until one leaves.
    pub max_nodes: usize,
    /// Control connections still registering (token and proof) at once.
    pub max_registering: usize,
    /// Relayed client connections at once, waiting or spliced, in all.
    pub max_clients: usize,
    /// Relayed client connections to one node at once.
    pub max_clients_per_node: usize,
    /// Bytes a second through each direction of a relayed connection after
    /// a burst of one second's worth; 0 for no limit. At least one largest
    /// message a second (`max_message`).
    pub max_bytes_per_second: u64,
    /// How long a connection may take to send its HTTP request's headers.
    pub header_read_timeout: Duration,
    /// Connections open at once, of every kind, counted from `accept`.
    pub max_connections: usize,
    /// Connections open at once from one address (an IPv4 address, an
    /// IPv6 `/64`), counted from `accept`. Not applied behind a proxy
    /// (`client_address_header`), where every connection is the proxy's.
    pub max_connections_per_address: usize,
    /// Control connections still registering at once from one address.
    pub max_registering_per_address: usize,
    /// Of `max_registering`, the places only a node that registered here
    /// before may keep (named by its first message), so known nodes get
    /// back in while strangers hold the rest. At most half of
    /// `max_registering`: a larger value counts as half.
    pub reserved_registering: usize,
    /// Relayed client connections at once from one address.
    pub max_clients_per_address: usize,
    /// How long a registering node has for its first message, and a
    /// spliced client for its first record.
    pub first_message_timeout: Duration,
    /// How long a stopping relay waits for open connections to finish
    /// before it drops them.
    pub shutdown_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message: MAX_RECORD,
            waiting_per_node: 64,
            accept_timeout: Duration::from_secs(10),
            registration_timeout: Duration::from_secs(10),
            ping_interval: Keepalive::DEFAULT.ping,
            idle_timeout: Keepalive::DEFAULT.silence,
            connect_burst: 30,
            connect_refill: Duration::from_secs(1),
            max_nodes: 128,
            max_registering: 64,
            max_clients: 384,
            max_clients_per_node: 128,
            max_bytes_per_second: 16 * 1024 * 1024,
            header_read_timeout: Duration::from_secs(10),
            max_connections: 1000,
            max_connections_per_address: 128,
            max_registering_per_address: 8,
            reserved_registering: 16,
            max_clients_per_address: 32,
            first_message_timeout: Duration::from_secs(2),
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// How a relay runs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RelayConfig {
    /// The secret nodes present to register (one per relay; plan,
    /// decision 4).
    pub token: RelayToken,
    /// The limits.
    pub limits: Limits,
    /// Behind a reverse proxy every connection comes from the proxy: name
    /// the header in which it passes the client's address (`X-Real-IP`,
    /// `CF-Connecting-IP`) so the rate limit is per client, not global.
    /// Only set it when every request goes through that proxy, which must
    /// overwrite the header: otherwise clients choose their own address.
    pub client_address_header: Option<HeaderName>,
}

impl RelayConfig {
    /// A relay with the default limits, rate limiting by peer address.
    #[must_use]
    pub fn new(token: RelayToken) -> Self {
        Self {
            token,
            limits: Limits::default(),
            client_address_header: None,
        }
    }
}

/// Serve the relay on `listener` until `shutdown`.
///
/// # Errors
/// The listener failed.
pub async fn serve(
    listener: TcpListener,
    config: RelayConfig,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let relay = Arc::new(Relay::new(config, shutdown.clone()));
    let app = Router::new()
        .route(HEALTH_PATH, get(|| async { "ok" }))
        .route(NODE_PATH, get(control::upgrade))
        .route(CONNECT_PATH, get(pairing::connect))
        .route(ACCEPT_PATH, get(pairing::accept))
        .with_state(Arc::clone(&relay));
    accept::run(listener, app, relay, shutdown).await
}

/// The relay's shared state.
pub(crate) struct Relay {
    token: RelayToken,
    limits: Limits,
    client_address_header: Option<HeaderName>,
    shutdown: CancellationToken,
    /// Registered nodes by routing id.
    nodes: Mutex<HashMap<RoutingId, Registration>>,
    /// Clients waiting for a node's data connection.
    pending: Mutex<HashMap<ConnId, oneshot::Sender<WebSocket>>>,
    rate: Mutex<RateLimiter>,
    generations: AtomicU64,
    /// Connections counted against the limits.
    load: Mutex<Load>,
    /// Nodes that registered here, for the places kept for them.
    known: Mutex<Known>,
    refusals: RefusalLog,
}

/// What the relay holds at once, against `Limits`. Addresses are as the
/// rate limiter keys them (an IPv4 address, an IPv6 `/64`); an address
/// with nothing counted has no entry.
#[derive(Debug, Default)]
struct Load {
    /// Sockets open, from `accept`.
    connections: usize,
    /// Sockets open per address (not counted behind a proxy).
    connections_per_address: HashMap<IpAddr, usize>,
    /// Control connections that have not registered yet, in the open
    /// places...
    registering: usize,
    /// ...and in the places kept for nodes that registered before.
    registering_reserved: usize,
    /// Control connections that have not registered yet, per address.
    registering_per_address: HashMap<IpAddr, usize>,
    /// Relayed clients in all.
    clients: usize,
    /// Relayed clients per node.
    clients_per_node: HashMap<RoutingId, usize>,
    /// Relayed clients per address.
    clients_per_address: HashMap<IpAddr, usize>,
}

/// One more for `key`.
fn count_up<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, usize>, key: K) {
    let count = counts.entry(key).or_insert(0);
    *count = count.saturating_add(1);
}

/// One less for `key`, forgetting it at zero.
fn count_down<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            let _gone = counts.remove(key);
        }
    }
}

/// The routing ids that registered here, at most `capacity`, the oldest
/// forgotten first. In memory: a restarted relay knows none.
#[derive(Debug, Default)]
struct Known {
    ids: std::collections::HashSet<RoutingId>,
    order: std::collections::VecDeque<RoutingId>,
}

impl Known {
    fn insert(&mut self, node: RoutingId, capacity: usize) {
        if !self.ids.insert(node) {
            return;
        }
        self.order.push_back(node);
        while self.order.len() > capacity {
            if let Some(oldest) = self.order.pop_front() {
                let _forgotten = self.ids.remove(&oldest);
            }
        }
    }
}

/// The node's registration would take a place the relay does not have.
#[derive(Debug)]
pub(crate) struct Full;

/// A control connection's place among those registering, given back on
/// drop. A place kept for known nodes is provisional: the connection keeps
/// it only by naming a node that registered here before (`confirm`).
pub(crate) struct RegisteringSlot<'a> {
    relay: &'a Relay,
    address: IpAddr,
    reserved: bool,
}

impl RegisteringSlot<'_> {
    /// Whether the connection may go on registering as `node`.
    pub(crate) fn confirm(&self, node: &RoutingId) -> bool {
        if !self.reserved || lock(&self.relay.known).ids.contains(node) {
            return true;
        }
        self.relay
            .refused(Refusal::NotKnown, Some(*node), Some(self.address));
        false
    }
}

impl Drop for RegisteringSlot<'_> {
    fn drop(&mut self) {
        let mut load = lock(&self.relay.load);
        if self.reserved {
            load.registering_reserved = load.registering_reserved.saturating_sub(1);
        } else {
            load.registering = load.registering.saturating_sub(1);
        }
        count_down(&mut load.registering_per_address, &self.address);
    }
}

/// A relayed client's place, to its node, from its address and in all,
/// given back on drop.
pub(crate) struct ClientSlot<'a> {
    relay: &'a Relay,
    node: RoutingId,
    address: IpAddr,
}

impl Drop for ClientSlot<'_> {
    fn drop(&mut self) {
        let mut load = lock(&self.relay.load);
        load.clients = load.clients.saturating_sub(1);
        count_down(&mut load.clients_per_node, &self.node);
        count_down(&mut load.clients_per_address, &self.address);
    }
}

/// One node's current registration.
struct Registration {
    /// Distinguishes this registration from one that replaced it.
    generation: u64,
    /// Announces waiting clients to the node's control connection.
    incoming: mpsc::Sender<ConnId>,
    /// Clients waiting for this node.
    waiting: Arc<AtomicUsize>,
    /// Cancelled when another registration replaces this one.
    replaced: CancellationToken,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every critical section is a few map operations that cannot panic
    // halfway; a poisoned lock still holds consistent data.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Relay {
    fn new(config: RelayConfig, shutdown: CancellationToken) -> Self {
        let rate = RateLimiter::new(config.limits.connect_burst, config.limits.connect_refill);
        Self {
            token: config.token,
            limits: config.limits,
            client_address_header: config.client_address_header,
            shutdown,
            nodes: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            rate: Mutex::new(rate),
            generations: AtomicU64::new(0),
            load: Mutex::new(Load::default()),
            known: Mutex::new(Known::default()),
            refusals: RefusalLog::default(),
        }
    }

    /// A place for a socket just accepted from `peer`, or `None` past
    /// `max_connections` or, unless behind a proxy,
    /// `max_connections_per_address`.
    fn connection(self: &Arc<Self>, peer: IpAddr) -> Option<accept::ConnectionPlace> {
        let address = accept::counted_address(self, peer);
        let refusal = {
            let mut load = lock(&self.load);
            let of_address = address
                .and_then(|address| load.connections_per_address.get(&address).copied())
                .unwrap_or(0);
            if load.connections >= self.limits.max_connections {
                accept::refusal(false)
            } else if of_address >= self.limits.max_connections_per_address {
                accept::refusal(true)
            } else {
                load.connections = load.connections.saturating_add(1);
                if let Some(address) = address {
                    count_up(&mut load.connections_per_address, address);
                }
                return Some(accept::ConnectionPlace {
                    relay: Arc::clone(self),
                    address,
                });
            }
        };
        self.refused(refusal, None, address);
        None
    }

    /// A socket's place given back (`accept::ConnectionPlace`).
    fn release_connection(&self, address: Option<IpAddr>) {
        let mut load = lock(&self.load);
        load.connections = load.connections.saturating_sub(1);
        if let Some(address) = address {
            count_down(&mut load.connections_per_address, &address);
        }
    }

    /// The address a request counts under: the proxy's header when one is
    /// configured and present, else the peer; an IPv6 address by its `/64`.
    fn client_address(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let address = self
            .client_address_header
            .as_ref()
            .and_then(|name| headers.get(name))
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<IpAddr>().ok())
            .unwrap_or_else(|| peer.ip());
        rate::bucket_key(address)
    }

    /// A place for a control connection from `address` to register in, or
    /// `None` past `max_registering_per_address`, or when the open places
    /// and those kept for known nodes are all taken.
    fn registering(&self, address: IpAddr) -> Option<RegisteringSlot<'_>> {
        let refusal = {
            let mut load = lock(&self.load);
            let limits = &self.limits;
            // At most half are kept, so a node new here always has room.
            let kept = limits.reserved_registering.min(limits.max_registering >> 1);
            let open = limits.max_registering.saturating_sub(kept);
            let of_address = load
                .registering_per_address
                .get(&address)
                .copied()
                .unwrap_or(0);
            let reserved = if of_address >= limits.max_registering_per_address {
                None
            } else if load.registering < open {
                load.registering = load.registering.saturating_add(1);
                Some(false)
            } else if load.registering_reserved < kept {
                load.registering_reserved = load.registering_reserved.saturating_add(1);
                Some(true)
            } else {
                None
            };
            match reserved {
                Some(reserved) => {
                    count_up(&mut load.registering_per_address, address);
                    return Some(RegisteringSlot {
                        relay: self,
                        address,
                        reserved,
                    });
                }
                None if of_address >= limits.max_registering_per_address => {
                    Refusal::AddressRegistering
                }
                None => Refusal::Registering,
            }
        };
        self.refused(refusal, None, Some(address));
        None
    }

    /// A place for a client of `node` from `address`, or `None` when the
    /// node, the address or the relay carries its most clients already.
    fn client(&self, node: RoutingId, address: IpAddr) -> Option<ClientSlot<'_>> {
        let refusal = {
            let mut load = lock(&self.load);
            let limits = &self.limits;
            let of_node = load.clients_per_node.get(&node).copied().unwrap_or(0);
            let of_address = load.clients_per_address.get(&address).copied().unwrap_or(0);
            if load.clients >= limits.max_clients {
                Refusal::Clients
            } else if of_node >= limits.max_clients_per_node {
                Refusal::NodeClients
            } else if of_address >= limits.max_clients_per_address {
                Refusal::AddressClients
            } else {
                load.clients = load.clients.saturating_add(1);
                count_up(&mut load.clients_per_node, node);
                count_up(&mut load.clients_per_address, address);
                return Some(ClientSlot {
                    relay: self,
                    node,
                    address,
                });
            }
        };
        self.refused(refusal, Some(node), Some(address));
        None
    }

    /// Log a refusal at info (throttled, `RefusalLog`). Never a token.
    fn refused(&self, refusal: Refusal, node: Option<RoutingId>, address: Option<IpAddr>) {
        let Some(more) = self.refusals.note(refusal, tokio::time::Instant::now()) else {
            return;
        };
        let node = node.map(|node| node.to_string()).unwrap_or_default();
        let address = address
            .map(|address| address.to_string())
            .unwrap_or_default();
        let limits = &self.limits;
        match refusal {
            Refusal::Nodes => tracing::info!(
                %node, limit = limits.max_nodes, more,
                "registration refused: the relay holds its most nodes (max_nodes)"
            ),
            Refusal::Registering => tracing::info!(
                limit = limits.max_registering,
                more,
                "connection refused: as many nodes as allowed are registering (max_registering)"
            ),
            Refusal::AddressRegistering => tracing::info!(
                %address, limit = limits.max_registering_per_address, more,
                "connection refused: as many nodes as allowed are registering from this address \
                 (max_registering_per_address)"
            ),
            Refusal::NotKnown => tracing::info!(
                %node, %address, more,
                "registration refused: only nodes registered before may take the places kept \
                 for them (reserved_registering)"
            ),
            Refusal::Clients => tracing::info!(
                %node, limit = limits.max_clients, more,
                "client refused: the relay carries its most clients (max_clients)"
            ),
            Refusal::NodeClients => tracing::info!(
                %node, limit = limits.max_clients_per_node, more,
                "client refused: the node has its most clients through the relay \
                 (max_clients_per_node)"
            ),
            Refusal::AddressClients => tracing::info!(
                %node, %address, limit = limits.max_clients_per_address, more,
                "client refused: as many clients as allowed come from this address \
                 (max_clients_per_address)"
            ),
            Refusal::Connections => tracing::info!(
                limit = limits.max_connections,
                more,
                "connection refused at accept: the relay holds its most connections \
                 (max_connections)"
            ),
            Refusal::AddressConnections => tracing::info!(
                %address, limit = limits.max_connections_per_address, more,
                "connection refused at accept: this address holds its most connections \
                 (max_connections_per_address)"
            ),
        }
    }

    /// The websocket upgrade with the relay's message limit.
    fn limit(&self, upgrade: WebSocketUpgrade) -> WebSocketUpgrade {
        upgrade
            .max_message_size(self.limits.max_message)
            .max_frame_size(self.limits.max_message)
    }

    /// Whether a new connection from `address` (`client_address`) is
    /// admitted by the rate of new connections.
    fn admit(&self, address: IpAddr) -> bool {
        lock(&self.rate).admit(address, tokio::time::Instant::now())
    }

    /// Make `registration` the node's, replacing any earlier one; a new
    /// node only while fewer than `max_nodes` are registered. A node
    /// registered is known from then on (`reserved_registering`).
    fn register(&self, node: RoutingId, registration: Registration) -> Result<(), Full> {
        let previous = {
            let mut nodes = lock(&self.nodes);
            if !nodes.contains_key(&node) && nodes.len() >= self.limits.max_nodes {
                drop(nodes);
                self.refused(Refusal::Nodes, Some(node), None);
                return Err(Full);
            }
            nodes.insert(node, registration)
        };
        let remembered = self.limits.max_nodes.saturating_mul(4).max(64);
        lock(&self.known).insert(node, remembered);
        if let Some(previous) = previous {
            previous.replaced.cancel();
        }
        Ok(())
    }

    /// Forget the node's registration if it is still `generation`.
    fn unregister(&self, node: &RoutingId, generation: u64) {
        let mut nodes = lock(&self.nodes);
        if nodes
            .get(node)
            .is_some_and(|current| current.generation == generation)
        {
            let _gone = nodes.remove(node);
        }
    }

    fn next_generation(&self) -> u64 {
        self.generations.fetch_add(1, Ordering::Relaxed)
    }
}

/// Close `socket` with one of the relay's codes.
async fn close_with(socket: &mut (impl futures_util::Sink<Message> + Unpin), close: RelayClose) {
    let frame = CloseFrame {
        code: close.code(),
        reason: "".into(),
    };
    let sent = tokio::time::timeout(Duration::from_secs(1), async {
        let _sent = socket.send(Message::Close(Some(frame))).await;
        let _closed = socket.close().await;
    })
    .await;
    if sent.is_err() {
        tracing::debug!(?close, "closing a connection timed out");
    }
}

/// Whether a websocket error is a message over the size limit.
fn too_large(error: &axum::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::error::CapacityError;
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<Error>())
        .is_some_and(|error| matches!(error, Error::Capacity(CapacityError::MessageTooLong { .. })))
}
