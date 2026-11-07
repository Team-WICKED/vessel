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
    /// The launch's token.
    pub token: LaunchToken,
    /// How long a browser's websocket has to present the token.
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
    /// The listener failed.
    #[error("serving the UI failed")]
    Io(#[from] io::Error),
}

/// What the handlers share.
#[derive(Debug)]
pub(crate) struct Ui<S> {
    pub(crate) token: LaunchToken,
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
    // The nodes' connections stop with the server.
    let connections = shutdown.child_token();
    let _stop_connections = connections.clone().drop_guard();
    let ui = Arc::new(Ui {
        token: config.token,
        hello_deadline: config.hello_deadline,
        registry: Registry::new(source, config.backoff, connections),
    });
    let allowed = Arc::new(Allowed::for_address(address));
    let app = Router::new()
        .route("/api/status", get(status::<S>))
        .route("/api/ws", get(socket::upgrade::<S>))
        .fallback(files::serve)
        .with_state(ui)
        .layer(middleware::from_fn_with_state(allowed, guard));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    Ok(())
}

/// `GET /api/status`, with the token as a bearer.
async fn status<S: NodeSource>(State(ui): State<Arc<Ui<S>>>, headers: HeaderMap) -> Response {
    if !authorized(&ui.token, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(Status {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        bundled: files::bundled(),
    })
    .into_response()
}

/// Whether `headers` carry the token as `Authorization: Bearer`.
fn authorized(token: &LaunchToken, headers: &HeaderMap) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| token.matches(presented))
}
