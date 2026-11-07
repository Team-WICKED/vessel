//! The server: loopback only, the guard on every request, the HTTP API,
//! the websocket and the files.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use vessel_client::Backoff;

use crate::guard::{Allowed, guard};
use crate::registry::Registry;
use crate::source::NodeSource;
use crate::token::LaunchToken;
use crate::wire::Status;
use crate::{files, socket};

/// How `vessel ui` serves.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UiConfig {
    /// The launch's one-time URL bootstrap.
    pub token: LaunchToken,
    /// Overall sockets, including upgraded websockets.
    pub max_connections: usize,
    /// Sockets not yet authenticated, counted before HTTP headers arrive.
    pub max_unauthenticated: usize,
    /// Time to receive an HTTP request's headers.
    pub header_deadline: Duration,
    /// How long a browser's websocket has to complete authentication.
    pub hello_deadline: Duration,
    /// How long to wait between dials of a node that is down (the CLI's
    /// default; tests shorten it).
    pub backoff: Backoff,
}

impl UiConfig {
    /// Serve with `token` and the default deadline (5 s).
    #[must_use]
    pub const fn new(token: LaunchToken) -> Self {
        Self {
            token,
            max_connections: 128,
            max_unauthenticated: 32,
            header_deadline: Duration::from_secs(5),
            hello_deadline: Duration::from_secs(5),
            backoff: Backoff {
                first: Duration::from_millis(500),
                max: Duration::from_secs(30),
            },
        }
    }
}

/// Why the UI could not be served.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The listener is not on a loopback address: the UI is for this
    /// machine's browser only (ADR 0017).
    #[error("the UI listens on loopback only, not {0}")]
    NotLoopback(SocketAddr),
    /// Limits must leave a bounded, nonempty admission budget.
    #[error(
        "UI connection limits must be nonzero and unauthenticated <= total <= Semaphore::MAX_PERMITS"
    )]
    InvalidLimits,
    /// Randomness for the session or launch id failed.
    #[error(transparent)]
    Mint(#[from] crate::MintError),
    /// The listener failed.
    #[error("serving the UI failed")]
    Io(#[from] io::Error),
}

/// What the handlers share.
#[derive(Debug)]
pub(crate) struct Ui<S> {
    pub(crate) auth: crate::auth::Auth,
    pub(crate) shutdown: CancellationToken,
    pub(crate) hello_deadline: Duration,
    pub(crate) registry: Arc<Registry<S>>,
}

/// Serve the UI on `listener` until `shutdown`, bridging to the nodes
/// `source` offers.
///
/// # Errors
/// The listener is not on loopback, or it failed.
pub async fn serve<S: NodeSource>(
    listener: TcpListener,
    config: UiConfig,
    source: S,
    shutdown: CancellationToken,
) -> Result<(), ServeError> {
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        return Err(ServeError::NotLoopback(address));
    }
    if config.max_unauthenticated == 0
        || config.max_connections == 0
        || config.max_unauthenticated > config.max_connections
        || config.max_connections > tokio::sync::Semaphore::MAX_PERMITS
    {
        return Err(ServeError::InvalidLimits);
    }
    // The nodes' connections stop with the server.
    let connections = shutdown.child_token();
    let _stop_connections = connections.clone().drop_guard();
    let allowed = Arc::new(Allowed::for_address(address, config.token.host()));
    let ui = Arc::new(Ui {
        auth: crate::auth::Auth::new(config.token)?,
        shutdown: connections.clone(),
        hello_deadline: config.hello_deadline,
        registry: Registry::new(source, config.backoff, connections),
    });
    let app = Router::new()
        .route("/api/status", get(status::<S>))
        .route("/api/ws", get(socket::upgrade::<S>))
        .fallback(files::serve)
        .with_state(ui)
        .layer(middleware::from_fn_with_state(allowed, guard));
    crate::transport::serve(
        listener,
        app,
        config.max_connections,
        config.max_unauthenticated,
        config.header_deadline,
        shutdown,
    )
    .await?;
    Ok(())
}

/// `GET /api/status`, with an issued session key as a bearer.
async fn status<S: NodeSource>(State(ui): State<Arc<Ui<S>>>, headers: HeaderMap) -> Response {
    if !authorized(&ui.auth, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(Status {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        bundled: files::bundled(),
    })
    .into_response()
}

/// Whether `headers` carry the issued session key as `Authorization: Bearer`.
fn authorized(auth: &crate::auth::Auth, headers: &HeaderMap) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| auth.authorized(presented))
}
