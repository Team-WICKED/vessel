//! HTTP and websocket API.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::broadcast::error::RecvError;
use vessel_core::{
    Harness, NodeInfo, SessionEvent, SessionId, SessionInfo, SessionInput, SessionSpec,
};

use crate::error::NodeError;
use crate::harness;
use crate::session::{Session, SessionManager};

/// Shared state behind every handler.
#[derive(Debug)]
pub struct AppState {
    /// This node.
    pub node: NodeInfo,
    /// Sessions on this node.
    pub sessions: SessionManager,
}

type Shared = Arc<AppState>;

/// Build the router.
pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/node", get(node))
        .route("/harnesses", get(harnesses))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(kill_session))
        .route("/sessions/{id}/input", axum::routing::post(send_input))
        .route("/sessions/{id}/stream", get(stream_session))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn node(State(state): State<Shared>) -> Json<NodeInfo> {
    Json(state.node.clone())
}

async fn harnesses() -> Json<Vec<Harness>> {
    Json(harness::discover().await)
}

async fn list_sessions(State(state): State<Shared>) -> Json<Vec<SessionInfo>> {
    Json(state.sessions.list().await)
}

async fn create_session(
    State(state): State<Shared>,
    Json(spec): Json<SessionSpec>,
) -> Result<(StatusCode, Json<SessionInfo>), NodeError> {
    let session = state.sessions.spawn(spec).await?;
    Ok((StatusCode::CREATED, Json(session.info())))
}

async fn lookup(state: &AppState, id: SessionId) -> Result<Arc<Session>, NodeError> {
    state
        .sessions
        .get(id)
        .await
        .ok_or(NodeError::SessionNotFound(id))
}

async fn get_session(
    State(state): State<Shared>,
    Path(id): Path<SessionId>,
) -> Result<Json<SessionInfo>, NodeError> {
    Ok(Json(lookup(&state, id).await?.info()))
}

async fn kill_session(
    State(state): State<Shared>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, NodeError> {
    lookup(&state, id).await?.kill();
    Ok(StatusCode::ACCEPTED)
}

async fn send_input(
    State(state): State<Shared>,
    Path(id): Path<SessionId>,
    Json(input): Json<SessionInput>,
) -> Result<StatusCode, NodeError> {
    lookup(&state, id).await?.write_input(&input.text).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stream_session(
    ws: WebSocketUpgrade,
    State(state): State<Shared>,
    Path(id): Path<SessionId>,
) -> Result<Response, NodeError> {
    let session = lookup(&state, id).await?;
    Ok(ws.on_upgrade(move |socket| pump_socket(socket, session)))
}

async fn send_event(socket: &mut WebSocket, event: &SessionEvent) -> Result<(), ()> {
    let text = serde_json::to_string(event).map_err(|_err| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_err| ())
}

async fn pump_socket(mut socket: WebSocket, session: Arc<Session>) {
    let (backlog, mut rx) = session.subscribe().await;
    let mut status = session.status();

    for chunk in backlog {
        if send_event(&mut socket, &SessionEvent::Output(chunk))
            .await
            .is_err()
        {
            return;
        }
    }

    // If the session already ended, say so and stop.
    if !status.borrow_and_update().is_live() {
        let final_status = status.borrow().clone();
        if send_event(
            &mut socket,
            &SessionEvent::Status {
                status: final_status,
            },
        )
        .await
        .is_err()
        {
            tracing::debug!("client went away before final status");
        }
        return;
    }

    loop {
        tokio::select! {
            next = rx.recv() => match next {
                Ok(chunk) => {
                    if send_event(&mut socket, &SessionEvent::Output(chunk)).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(session = %session.info().id, skipped, "websocket reader lagged");
                }
                Err(RecvError::Closed) => break,
            },
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
                let current = status.borrow_and_update().clone();
                if current.is_live() {
                    continue;
                }
                // The supervisor only flips status after the pumps drained,
                // so whatever is still queued is the tail of the output.
                while let Ok(chunk) = rx.try_recv() {
                    if send_event(&mut socket, &SessionEvent::Output(chunk)).await.is_err() {
                        return;
                    }
                }
                if send_event(&mut socket, &SessionEvent::Status { status: current })
                    .await
                    .is_err()
                {
                    tracing::debug!("client went away before final status");
                }
                break;
            }
        }
    }
    // Dropping the socket sends the close frame.
}
