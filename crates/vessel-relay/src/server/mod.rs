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

mod control;
mod pairing;
mod rate;

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
use crate::carrier::Keepalive;
use crate::{ACCEPT_PATH, CONNECT_PATH, HEALTH_PATH, NODE_PATH};

/// The relay's limits (ADR 0009). The defaults are the protocol's.
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
        .with_state(relay);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.cancelled_owned())
    .await
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
        }
    }

    /// The websocket upgrade with the relay's message limit.
    fn limit(&self, upgrade: WebSocketUpgrade) -> WebSocketUpgrade {
        upgrade
            .max_message_size(self.limits.max_message)
            .max_frame_size(self.limits.max_message)
    }

    /// Whether a new connection from this client is admitted.
    fn admit(&self, peer: SocketAddr, headers: &HeaderMap) -> bool {
        let address = self
            .client_address_header
            .as_ref()
            .and_then(|name| headers.get(name))
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<IpAddr>().ok())
            .unwrap_or_else(|| peer.ip());
        lock(&self.rate).admit(address, tokio::time::Instant::now())
    }

    /// Make `registration` the node's, replacing any earlier one.
    fn register(&self, node: RoutingId, registration: Registration) {
        let previous = lock(&self.nodes).insert(node, registration);
        if let Some(previous) = previous {
            previous.replaced.cancel();
        }
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
