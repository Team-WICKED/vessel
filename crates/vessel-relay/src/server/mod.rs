//! The relay server: plain HTTP and websockets, for running behind an HTTPS
//! reverse proxy (`vesseld relay`).
//!
//! State is in memory only: which nodes are registered, which clients wait
//! for which node, the per-address rate limiter and counts, and which
//! nodes registered before. A restart forgets it all; nodes register again
//! on their own (`NodeLink`), and clients reconnect.
//!
//! Every websocket task also watches the server's shutdown token: the
//! accept loop (`accept`) stops accepting and drops what is left after
//! `shutdown_grace`, but upgraded connections run in tasks of their own.

mod accept;
mod balance;
mod control;
mod pairing;
mod rate;
mod throttle;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use futures_util::SinkExt as _;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{ConnId, RelayClose, RelayToken, RoutingId};
use vessel_secure::MAX_RECORD;

pub use self::balance::Unbalanced;
use self::rate::RateLimiter;
use self::throttle::{Refusal, RefusalLog};
use crate::address::{
    BadAddressHeader, Block, BlockCounts, Client, ClientAddress, TrustedProxy, blocks,
};
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
/// - Round 8: one host, or one IPv6 `/48`, must not be able to deny the
///   relay, and nodes registered here before must get back in under a
///   flood (many hosts, a botnet, are out of scope: README). From
///   `accept`, an address's new sockets are rate-limited
///   (`connect_burst`, `connect_refill`) and capped at 16 not
///   authenticated yet; sockets not authenticated yet fill only
///   `max_unauthenticated` of `max_connections`, the rest kept for
///   registered nodes and paired clients (`accept`'s Decision Log). Every
///   per-address count is kept per IPv4 address and per IPv6 `/64`, `/56`
///   and `/48`, the wider blocks with two and four times the limit
///   (`address`). A place kept for known nodes is taken only by a
///   connection whose first message names one, when no open place is
///   left; until then the connection holds no place, for at most
///   `first_message_timeout`. The client address header is believed only
///   from `trusted_proxies`.
/// - After round 8: `Limits::check` refuses limits under which one `/48`
///   could hold half of a pool or more (`balance`), so the bar holds
///   whatever the operator sets; the defaults pass (registering per
///   address 7, `max_unauthenticated` 400).
/// - Round 9: a client spliced to its node is not yet one the node
///   admitted (the review held a node's whole budget of unadmitted
///   channels with one `/48`'s clients). Clients of one node from one
///   address count against `max_node_clients_per_address` until
///   `client_settle`, after which a node that keeps `RELAYED_ADMISSION`
///   has admitted them; only then do they count as authenticated and
///   give back their rate token (`pairing`).
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
    /// New connections an address may open at once (an IPv4 address or an
    /// IPv6 `/64`; a `/56` twice this, a `/48` four times), counted from
    /// `accept`, or at the upgrade by the header's address through a
    /// trusted proxy; a connection that authenticates gives its token
    /// back...
    pub connect_burst: u32,
    /// ...and then one more every this long (a `/56` twice as often, a
    /// `/48` four times).
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
    /// Of `max_connections`, the sockets that have not authenticated yet
    /// (a registered node's control connection and a client paired with a
    /// node have): the rest is kept for those that have. Less than half of
    /// `max_connections` ([`Limits::check`]).
    pub max_unauthenticated: usize,
    /// Sockets not authenticated yet from one address (an IPv4 address, an
    /// IPv6 `/64`; a `/56` twice this, a `/48` four times), counted from
    /// `accept`. Not applied to a trusted proxy's connections. A `/48`'s
    /// share less than half of `max_unauthenticated` ([`Limits::check`]).
    pub max_connections_per_address: usize,
    /// Control connections still registering, or waiting for a place, at
    /// once from one address (scaled as `max_connections_per_address`). A
    /// `/48`'s share less than half of `max_registering`
    /// ([`Limits::check`]).
    pub max_registering_per_address: usize,
    /// Of `max_registering`, the places kept for nodes that registered
    /// here before: taken only when no open place is left, by a
    /// connection whose first message names such a node; anyone else is
    /// refused then. At most half of `max_registering`: a larger value
    /// counts as half.
    pub reserved_registering: usize,
    /// Relayed client connections at once from one address (scaled as
    /// `max_connections_per_address`). A `/48`'s share less than half of
    /// `max_clients` ([`Limits::check`]).
    pub max_clients_per_address: usize,
    /// How long a registering node has for its first message, and a
    /// spliced client for its first record.
    pub first_message_timeout: Duration,
    /// How long a stopping relay waits for open connections to finish
    /// before it drops them.
    pub shutdown_grace: Duration,
    /// Relayed clients to one node at once from one address (scaled as
    /// `max_connections_per_address`) that the node may not have admitted
    /// yet: waiting, or spliced for less than `client_settle`.
    pub max_node_clients_per_address: usize,
    /// How long a spliced client counts as being admitted by its node: a
    /// splice still open after this was admitted (a node drops one it has
    /// not within `RELAYED_ADMISSION`). Then the client leaves
    /// `max_node_clients_per_address`, moves into the share kept for
    /// sockets that authenticated, and gives back its rate token.
    pub client_settle: Duration,
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
            max_unauthenticated: 400,
            max_connections_per_address: 16,
            max_registering_per_address: 7,
            reserved_registering: 16,
            max_clients_per_address: 16,
            first_message_timeout: Duration::from_secs(2),
            shutdown_grace: Duration::from_secs(5),
            max_node_clients_per_address: 4,
            client_settle: crate::RELAYED_ADMISSION.saturating_add(Duration::from_secs(2)),
        }
    }
}

impl Limits {
    /// Whether these limits keep the bar: one IPv6 `/48` (at four times
    /// each per-address limit) holds less than half of the registering
    /// places, of the client places, of the unauthenticated share and of a
    /// node's relayed admissions (`RELAYED_UNADMITTED`), and that share is
    /// less than half of `max_connections` (`balance`).
    /// The defaults pass; `serve` and `vesseld relay` refuse limits that
    /// do not.
    ///
    /// # Errors
    /// The first rule broken, naming both limits by their `vesseld relay`
    /// flags, the largest share that would pass and the smallest pool.
    pub fn check(&self) -> Result<(), Unbalanced> {
        balance::check(self)
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
    /// `CF-Connecting-IP`) so the limits are per client, not global. Only
    /// taken from `trusted_proxies`, which must overwrite it.
    pub client_address_header: Option<HeaderName>,
    /// The peers whose `client_address_header` is believed; needed with
    /// it (`serve` refuses a header without them, and ignores them without
    /// a header). Their sockets are not counted per address at `accept`;
    /// anyone else is counted by its socket address throughout.
    pub trusted_proxies: Vec<TrustedProxy>,
    /// Whether `serve` refuses limits that break the bar
    /// ([`Limits::check`]). Only tests that exercise one cap at a tiny
    /// scale turn it off; `vesseld relay` cannot.
    pub check_limits: bool,
}

impl RelayConfig {
    /// A relay with the default limits, rate limiting by peer address.
    #[must_use]
    pub fn new(token: RelayToken) -> Self {
        Self {
            token,
            limits: Limits::default(),
            client_address_header: None,
            trusted_proxies: Vec::new(),
            check_limits: true,
        }
    }
}

/// Serve the relay on `listener` until `shutdown`.
///
/// # Errors
/// A client address header without a trusted proxy, or limits that break
/// the bar ([`Limits::check`]), both `InvalidInput`; or the listener
/// failed.
pub async fn serve(
    listener: TcpListener,
    config: RelayConfig,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let clients = ClientAddress::new(
        config.client_address_header.clone(),
        config.trusted_proxies.clone(),
    )
    .map_err(|missing| io::Error::new(io::ErrorKind::InvalidInput, missing))?;
    if config.check_limits
        && let Err(unbalanced) = config.limits.check()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, unbalanced));
    }
    if config.client_address_header.is_none() && !config.trusted_proxies.is_empty() {
        // Without the header a proxy cannot say who its clients are: it is
        // counted as any peer is (`ClientAddress::new`).
        tracing::warn!("trusted proxies are ignored without a client address header");
    }
    let relay = Arc::new(Relay::new(config, clients, shutdown.clone()));
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
    /// Where a request counts (`client_address_header`, `trusted_proxies`).
    clients: ClientAddress,
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

/// What the relay holds at once, against `Limits`. Per-address counts are
/// kept for every block an address counts in (`address::blocks`); a block
/// with nothing counted has no entry.
#[derive(Debug, Default)]
struct Load {
    /// Sockets that have authenticated (a registered node's control
    /// connection, a client paired with a node and that node's data
    /// connection)...
    authenticated: usize,
    /// ...and those that have not, yet.
    unauthenticated: usize,
    /// Sockets not authenticated yet, per block (not a trusted proxy's).
    unauthenticated_per_block: BlockCounts,
    /// Control connections registering in the open places...
    registering: usize,
    /// ...and in the places kept for nodes that registered before.
    registering_reserved: usize,
    /// Control connections registering or waiting for a place, per block.
    registering_per_block: BlockCounts,
    /// Relayed clients in all.
    clients: usize,
    /// Relayed clients per node.
    clients_per_node: HashMap<RoutingId, usize>,
    /// Relayed clients per block.
    clients_per_block: BlockCounts,
    /// Relayed clients per node and block that their node may not have
    /// admitted yet (`ClientSlot::settle`).
    node_clients_per_block: HashMap<(RoutingId, Block), usize>,
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

/// Whether one more client of `node` fits in every one of `blocks` under
/// `limit` (scaled per block).
fn fits_node(
    counts: &HashMap<(RoutingId, Block), usize>,
    node: RoutingId,
    blocks: &[Block],
    limit: usize,
) -> bool {
    blocks
        .iter()
        .all(|block| counts.get(&(node, *block)).copied().unwrap_or(0) < block.limit(limit))
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

/// Where a registering control connection stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seat {
    /// In an open place.
    Open,
    /// In no place yet: every open place was taken when it came. Its first
    /// message decides: a node registered here before takes a kept place
    /// if one is free, anyone else is refused.
    Waiting,
    /// In a place kept for nodes registered here before.
    Kept,
}

/// A control connection's place while it registers, given back on drop
/// (see `Seat`).
pub(crate) struct RegisteringSlot<'a> {
    relay: &'a Relay,
    blocks: Vec<Block>,
    address: IpAddr,
    seat: Mutex<Seat>,
}

impl RegisteringSlot<'_> {
    /// Whether the connection may go on registering as `node`: always from
    /// an open place; from none, only a node registered here before, and
    /// only while a kept place is free (it takes it).
    pub(crate) fn confirm(&self, node: &RoutingId) -> bool {
        let mut seat = lock(&self.seat);
        match *seat {
            Seat::Open | Seat::Kept => return true,
            Seat::Waiting => {}
        }
        let known = lock(&self.relay.known).ids.contains(node);
        if known {
            let mut load = lock(&self.relay.load);
            if load.registering_reserved < self.relay.kept_registering() {
                load.registering_reserved = load.registering_reserved.saturating_add(1);
                *seat = Seat::Kept;
                return true;
            }
        }
        drop(seat);
        self.relay
            .refused(Refusal::NotKnown, Some(*node), Some(self.address));
        false
    }
}

impl Drop for RegisteringSlot<'_> {
    fn drop(&mut self) {
        let seat = *lock(&self.seat);
        let mut load = lock(&self.relay.load);
        match seat {
            Seat::Open => load.registering = load.registering.saturating_sub(1),
            Seat::Kept => {
                load.registering_reserved = load.registering_reserved.saturating_sub(1);
            }
            Seat::Waiting => {}
        }
        load.registering_per_block.remove(&self.blocks);
    }
}

/// A relayed client's place, to its node, from its address and in all,
/// given back on drop.
pub(crate) struct ClientSlot<'a> {
    relay: &'a Relay,
    node: RoutingId,
    blocks: Vec<Block>,
    /// Out of `max_node_clients_per_address` (`settle`).
    settled: AtomicBool,
}

impl ClientSlot<'_> {
    /// The client's node admitted it, as far as the relay can tell: it is
    /// no longer counted against `max_node_clients_per_address`. Once.
    pub(crate) fn settle(&self) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut load = lock(&self.relay.load);
        for block in &self.blocks {
            count_down(&mut load.node_clients_per_block, &(self.node, *block));
        }
    }
}

impl Drop for ClientSlot<'_> {
    fn drop(&mut self) {
        self.settle();
        let mut load = lock(&self.relay.load);
        load.clients = load.clients.saturating_sub(1);
        count_down(&mut load.clients_per_node, &self.node);
        load.clients_per_block.remove(&self.blocks);
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
    fn new(config: RelayConfig, clients: ClientAddress, shutdown: CancellationToken) -> Self {
        let rate = RateLimiter::new(config.limits.connect_burst, config.limits.connect_refill);
        Self {
            token: config.token,
            limits: config.limits,
            clients,
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

    /// A place for a socket just accepted from `peer`, or `None`. A trusted
    /// proxy's sockets are not limited per address (the proxy's own limits
    /// stand in, and the client's address is known only from the request);
    /// anyone else's take a token of the address's rate
    /// (`connect_burst`, `connect_refill`) and a place for its blocks
    /// (`max_connections_per_address`). Every socket counts against
    /// `max_unauthenticated` until it authenticates
    /// (`accept::Authenticated::promote`), and against `max_connections`.
    fn connection(self: &Arc<Self>, peer: IpAddr) -> Option<accept::ConnectionPlace> {
        let counted = (!self.clients.is_trusted_proxy(peer)).then(|| blocks(peer));
        let refusal = 'refuse: {
            let limits = &self.limits;
            if counted.is_some() && !lock(&self.rate).admit(peer, tokio::time::Instant::now()) {
                break 'refuse Refusal::AcceptRate;
            }
            let mut load = lock(&self.load);
            let all = load.authenticated.saturating_add(load.unauthenticated);
            if all >= limits.max_connections {
                break 'refuse Refusal::Connections;
            }
            if load.unauthenticated >= limits.max_unauthenticated {
                break 'refuse Refusal::Unauthenticated;
            }
            if let Some(blocks) = &counted {
                if !load
                    .unauthenticated_per_block
                    .fits(blocks, limits.max_connections_per_address)
                {
                    break 'refuse Refusal::AddressConnections;
                }
                load.unauthenticated_per_block.add(blocks);
            }
            load.unauthenticated = load.unauthenticated.saturating_add(1);
            return Some(accept::ConnectionPlace::new(
                Arc::clone(self),
                peer,
                counted,
            ));
        };
        self.refused(refusal, None, Some(peer));
        None
    }

    /// A socket authenticated: out of the unauthenticated counts, into the
    /// authenticated one, and the token it took from its address's rate
    /// given back (only sockets that never authenticate spend the rate).
    fn authenticate(&self, peer: IpAddr, counted: Option<&[Block]>) {
        {
            let mut load = lock(&self.load);
            load.unauthenticated = load.unauthenticated.saturating_sub(1);
            load.authenticated = load.authenticated.saturating_add(1);
            if let Some(blocks) = counted {
                load.unauthenticated_per_block.remove(blocks);
            }
        }
        if counted.is_some() {
            lock(&self.rate).refund(peer);
        }
    }

    /// A socket's places given back when it closes.
    fn release_connection(&self, authenticated: bool, counted: Option<&[Block]>) {
        let mut load = lock(&self.load);
        if authenticated {
            load.authenticated = load.authenticated.saturating_sub(1);
        } else {
            load.unauthenticated = load.unauthenticated.saturating_sub(1);
            if let Some(blocks) = counted {
                load.unauthenticated_per_block.remove(blocks);
            }
        }
    }

    /// Where a request counts ([`ClientAddress::of`]): through a trusted
    /// proxy, its header's one bare address, or the proxy itself without
    /// the header; otherwise the peer.
    fn client_address(
        &self,
        peer: SocketAddr,
        headers: &HeaderMap,
    ) -> Result<Client, BadAddressHeader> {
        self.clients.of(peer.ip(), headers)
    }

    /// The answer to a request whose trusted proxy sent a client address
    /// header that is not one bare address (`client_address`).
    fn bad_address_header(&self, peer: SocketAddr) -> Response {
        self.refused(Refusal::AddressHeader, None, Some(peer.ip()));
        (
            StatusCode::BAD_REQUEST,
            "the client address header is not one address",
        )
            .into_response()
    }

    /// Whether a request from `client` is admitted by the rate of new
    /// connections: one through a trusted proxy is charged here, by its
    /// header's address or, without the header, the proxy's own (its
    /// socket was not); any other was at `accept`.
    fn admit(&self, client: Client) -> bool {
        if !client.proxied {
            return true;
        }
        if lock(&self.rate).admit(client.address, tokio::time::Instant::now()) {
            return true;
        }
        self.refused(Refusal::AcceptRate, None, Some(client.address));
        false
    }

    /// A request through a trusted proxy authenticated: the token its
    /// client's address paid back.
    fn refund(&self, client: Client) {
        if client.proxied {
            lock(&self.rate).refund(client.address);
        }
    }

    /// The registering places kept for nodes registered here before: at
    /// most half, so a node new here always has room.
    fn kept_registering(&self) -> usize {
        self.limits
            .reserved_registering
            .min(self.limits.max_registering >> 1)
    }

    /// A place for a control connection from `address` to register in: an
    /// open one if any is free; else, while places are kept for known
    /// nodes, none yet (`Seat::Waiting`, decided by its first message).
    /// `None` past `max_registering_per_address` (scaled per block), or
    /// when there is neither.
    fn registering(&self, address: IpAddr) -> Option<RegisteringSlot<'_>> {
        let blocks = blocks(address);
        let refusal = 'refuse: {
            let limits = &self.limits;
            let kept = self.kept_registering();
            let open = limits.max_registering.saturating_sub(kept);
            let mut load = lock(&self.load);
            if !load
                .registering_per_block
                .fits(&blocks, limits.max_registering_per_address)
            {
                break 'refuse Refusal::AddressRegistering;
            }
            let seat = if load.registering < open {
                load.registering = load.registering.saturating_add(1);
                Seat::Open
            } else if kept > 0 {
                Seat::Waiting
            } else {
                break 'refuse Refusal::Registering;
            };
            load.registering_per_block.add(&blocks);
            return Some(RegisteringSlot {
                relay: self,
                blocks,
                address,
                seat: Mutex::new(seat),
            });
        };
        self.refused(refusal, None, Some(address));
        None
    }

    /// A place for a client of `node` from `address`, or `None` when the
    /// node, the address (any of its blocks) or the relay carries its most
    /// clients already.
    fn client(&self, node: RoutingId, address: IpAddr) -> Option<ClientSlot<'_>> {
        let blocks = blocks(address);
        let refusal = {
            let mut load = lock(&self.load);
            let limits = &self.limits;
            let of_node = load.clients_per_node.get(&node).copied().unwrap_or(0);
            if load.clients >= limits.max_clients {
                Refusal::Clients
            } else if of_node >= limits.max_clients_per_node {
                Refusal::NodeClients
            } else if !load
                .clients_per_block
                .fits(&blocks, limits.max_clients_per_address)
            {
                Refusal::AddressClients
            } else if !fits_node(
                &load.node_clients_per_block,
                node,
                &blocks,
                limits.max_node_clients_per_address,
            ) {
                Refusal::NodeAddressClients
            } else {
                load.clients = load.clients.saturating_add(1);
                count_up(&mut load.clients_per_node, node);
                load.clients_per_block.add(&blocks);
                for block in &blocks {
                    count_up(&mut load.node_clients_per_block, (node, *block));
                }
                return Some(ClientSlot {
                    relay: self,
                    node,
                    blocks,
                    settled: AtomicBool::new(false),
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
                "registration refused: no open place, and the places kept for nodes registered \
                 before are not for this one, or all taken (reserved_registering)"
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
            Refusal::Unauthenticated => tracing::info!(
                limit = limits.max_unauthenticated,
                more,
                "connection refused at accept: the share of connections not authenticated yet \
                 is full (max_unauthenticated)"
            ),
            Refusal::AddressConnections => tracing::info!(
                %address, limit = limits.max_connections_per_address, more,
                "connection refused at accept: this address holds its most connections not \
                 authenticated yet (max_connections_per_address)"
            ),
            Refusal::NodeAddressClients => tracing::info!(
                %node, %address, limit = limits.max_node_clients_per_address, more,
                "client refused: as many clients as allowed come to this node from this address \
                 while it admits them (max_node_clients_per_address)"
            ),
            Refusal::AddressHeader => tracing::info!(
                proxy = %address, more,
                "request refused: the trusted proxy's client address header is not one address"
            ),
            Refusal::AcceptRate => tracing::info!(
                %address, burst = limits.connect_burst, more,
                "connection refused: this address opens new connections too fast \
                 (connect_burst, connect_refill)"
            ),
        }
    }

    /// The websocket upgrade with the relay's message limit.
    fn limit(&self, upgrade: WebSocketUpgrade) -> WebSocketUpgrade {
        upgrade
            .max_message_size(self.limits.max_message)
            .max_frame_size(self.limits.max_message)
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
