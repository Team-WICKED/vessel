//! Node error type and its HTTP mapping.

use std::net::SocketAddr;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use vessel_core::{HarnessKind, SessionId};

/// Anything that can go wrong inside the node.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The requested harness binary is not on `PATH`.
    #[error("harness `{0}` is not installed on this node")]
    HarnessNotInstalled(HarnessKind),
    /// The harness process could not be started.
    #[error("failed to spawn harness: {0}")]
    Spawn(#[source] std::io::Error),
    /// No session with that id.
    #[error("session {0} not found")]
    SessionNotFound(SessionId),
    /// The session has already ended.
    #[error("session {0} is not running")]
    SessionNotLive(SessionId),
    /// Writing to the harness's stdin failed.
    #[error("failed to write to session stdin: {0}")]
    Stdin(#[source] std::io::Error),
    /// The node's data directory could not be prepared.
    #[error("failed to prepare data dir {0}: {1}")]
    DataDir(std::path::PathBuf, #[source] std::io::Error),
    /// The listen socket could not be bound.
    #[error("failed to bind {0}: {1}")]
    Bind(SocketAddr, #[source] std::io::Error),
    /// The HTTP server stopped with an error.
    #[error("server error: {0}")]
    Serve(#[source] std::io::Error),
}

impl NodeError {
    const fn status(&self) -> StatusCode {
        match self {
            Self::HarnessNotInstalled(_) | Self::SessionNotFound(_) => StatusCode::NOT_FOUND,
            Self::SessionNotLive(_) => StatusCode::CONFLICT,
            Self::Spawn(_) | Self::Stdin(_) => StatusCode::BAD_GATEWAY,
            Self::DataDir(..) | Self::Bind(..) | Self::Serve(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for NodeError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}
