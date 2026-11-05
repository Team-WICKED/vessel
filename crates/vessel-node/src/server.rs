//! HTTP: `GET /health` (open, for probes) and `GET /rpc` (the protocol 2
//! websocket, token required).
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
use vessel_rpc::Server;
use vessel_store::NodeToken;

use crate::auth;
use crate::service::{Connection, NodeService};
use crate::transport::WsTransport;

/// Largest websocket message accepted from a client. Requests are small;
/// this only stops a broken or hostile client from making the node buffer
/// without bound.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// What every request handler needs.
#[derive(Debug, Clone)]
pub(crate) struct Gateway {
    pub(crate) rpc: Arc<Server<NodeService>>,
    pub(crate) token: Arc<NodeToken>,
    pub(crate) connections: TaskTracker,
}

pub(crate) fn router(gateway: Gateway) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/rpc", get(rpc))
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
                .serve(WsTransport::new(socket), Connection::new)
                .await
            {
                tracing::info!(error = %error, "connection ended abnormally");
            }
        })
}
