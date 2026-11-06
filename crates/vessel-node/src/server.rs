//! HTTP: `GET /health` (open, for probes), `GET /rpc` (the protocol
//! websocket, token required) and `GET /secure` (a paired device's secure
//! channel, ADR 0009: no token; the device proves itself inside the
//! channel, after the handshake proved the node).
//!
//! Each websocket connection runs `vessel-rpc`'s server until it ends. The
//! connections are tracked so shutdown can wait for them to flush their last
//! frames; axum's own graceful shutdown does not cover upgraded
//! connections.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::header::WWW_AUTHENTICATE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use tokio_util::task::TaskTracker;
use vessel_relay::AxumCarrier;
use vessel_rpc::Server;
use vessel_store::NodeToken;

use crate::auth;
use crate::guard::Guarded;
use crate::secure::SecureGateway;
use crate::service::Connection;
use crate::transport::WsTransport;

/// Largest websocket message accepted from a client. Requests are small;
/// this only stops a broken or hostile client from making the node buffer
/// without bound.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// What every request handler needs.
#[derive(Debug, Clone)]
pub(crate) struct Gateway {
    pub(crate) rpc: Arc<Server<Guarded>>,
    pub(crate) token: Arc<NodeToken>,
    pub(crate) secure: SecureGateway,
    pub(crate) connections: TaskTracker,
}

pub(crate) fn router(gateway: Gateway) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/rpc", get(rpc))
        .route(SECURE_PATH, get(secure))
        .with_state(gateway)
}

async fn health() -> &'static str {
    "ok"
}

async fn rpc(
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !auth::authorized(&headers, &gateway.token) {
        tracing::info!("refused a connection without the right token");
        return (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, "Bearer")],
            "missing or wrong token",
        )
            .into_response();
    }
    let connection = gateway.connections.token();
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _tracked = connection;
            if let Err(error) = gateway
                .rpc
                .serve(WsTransport::new(socket), Connection::owner)
                .await
            {
                tracing::info!(error = %error, "connection ended abnormally");
            }
        })
}

/// Where paired devices reach the node directly; clients add it to a
/// direct route (`ws://host:port`).
pub(crate) const SECURE_PATH: &str = "/secure";

/// A secure channel straight to the node. Messages and frames are capped
/// at one record ([`vessel_secure::MAX_RECORD`]), so an oversized message
/// is refused from its header, before the node buffers it.
async fn secure(State(gateway): State<Gateway>, upgrade: WebSocketUpgrade) -> Response {
    let connection = gateway.connections.token();
    upgrade
        .max_message_size(vessel_secure::MAX_RECORD)
        .max_frame_size(vessel_secure::MAX_RECORD)
        .on_upgrade(move |socket| async move {
            let _tracked = connection;
            gateway
                .secure
                .accept_logged(AxumCarrier::new(socket, None), "direct")
                .await;
        })
}
