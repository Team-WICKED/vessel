//! Errors: why a node could not start ([`NodeError`]), and why a call
//! failed (`ServiceError`, mapped to the wire in one place).

use std::net::SocketAddr;

use vessel_proto::{ErrorCode, EventSeq, SessionId, WireError};
use vessel_rpc::OutboundError;
use vessel_session::SessionError;
use vessel_store::StoreError;

use crate::persist::Unpersisted;

/// A node failed to start, or its server failed.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The data directory, node id or token could not be prepared.
    #[error("the data directory is unusable")]
    DataDir(#[source] StoreError),
    /// The event log could not be opened.
    #[error("the event log could not be opened")]
    Store(#[source] StoreError),
    /// The listen address could not be bound.
    #[error("cannot listen on {address}")]
    Bind {
        /// The address.
        address: SocketAddr,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP server failed.
    #[error("the server failed")]
    Serve(#[source] std::io::Error),
    /// A startup or shutdown task failed unexpectedly.
    #[error("a node task failed")]
    Task(#[source] tokio::task::JoinError),
}

/// Why a call failed inside the node. Mapped to the wire once, by
/// [`ServiceError::to_wire`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    /// No session with this id, live or stored.
    #[error("no session {0}")]
    NotFound(SessionId),
    /// The session ended in an earlier run of this node; it can be read but
    /// not driven.
    #[error("session {0} ended in an earlier run of this node")]
    EndedEarlier(SessionId),
    /// `from` is past the end of the session's log.
    #[error("session {session} has {next} records; cannot start at {from}")]
    Ahead {
        /// The session.
        session: SessionId,
        /// Requested start.
        from: EventSeq,
        /// The log's length.
        next: EventSeq,
    },
    /// The session runtime refused or failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The event log failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The session's records could not all be persisted.
    #[error(transparent)]
    Unpersisted(#[from] Unpersisted),
    /// The connection cannot take the subscription.
    #[error(transparent)]
    Outbound(#[from] OutboundError),
    /// A task the call depended on failed.
    #[error("a node task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl ServiceError {
    /// The error as the client sees it: a code to branch on and the full
    /// chain of causes as text (none of which contains secrets).
    pub(crate) fn to_wire(&self) -> WireError {
        let code = match self {
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::EndedEarlier(_) => ErrorCode::Conflict,
            Self::Ahead { .. } => ErrorCode::InvalidRequest,
            Self::Session(error) => error.code(),
            Self::Store(error) => error.code(),
            Self::Outbound(error) => error.code(),
            Self::Unpersisted(_) | Self::Task(_) => ErrorCode::Internal,
        };
        let mut message = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        WireError::new(code, message)
    }
}
