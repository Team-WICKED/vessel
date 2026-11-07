//! Errors of this crate. Transport errors stay generic (`E`) so the caller
//! keeps the concrete error of whatever transport it used.

use vessel_proto::{
    ErrorCode, Method, ProtocolVersion, RequestId, SubscriptionId, UnexpectedReply, WireError,
};

/// A peer broke the protocol's flow rules. The connection is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolViolation {
    /// The client's first frame was not `Hello`.
    #[error("the first frame was not a hello")]
    ExpectedHello,
    /// The client sent `Hello` again after the handshake.
    #[error("a second hello arrived after the handshake")]
    RepeatedHello,
    /// The client reused the id of a call that is still in flight, so its
    /// responses could no longer be told apart.
    #[error("request id {0} is already in flight")]
    DuplicateRequestId(RequestId),
    /// The node's first frame was neither `Welcome` nor `Rejected`.
    #[error("the node did not answer the hello with welcome or rejected")]
    ExpectedWelcome,
    /// The node sent a handshake frame in the middle of a connection.
    #[error("a handshake frame arrived after the handshake")]
    UnexpectedHandshake,
}

/// Why a server connection ended abnormally.
#[derive(Debug, thiserror::Error)]
pub enum ServeError<E> {
    /// Reading or writing the transport failed.
    #[error("transport failed")]
    Transport(#[source] E),
    /// The client did not send `Hello` in time.
    #[error("the client did not complete the handshake in time")]
    HandshakeTimeout,
    /// The client speaks no protocol version this node supports; it was told
    /// so with a `Rejected` frame.
    #[error("rejected the client: {0}")]
    Rejected(WireError),
    /// The client broke the protocol.
    #[error("protocol violation")]
    Violation(#[source] ProtocolViolation),
    /// A frame was not valid JSON for any client frame, and did not even
    /// carry a request id to answer.
    #[error("undecodable frame")]
    Decode(#[source] serde_json::Error),
    /// A frame could not be serialised. This is a bug; the connection is
    /// closed so the client sees a disconnect instead of a call that never
    /// answers.
    #[error("could not encode a frame")]
    Encode(#[source] serde_json::Error),
}

/// Why a client could not establish a connection.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError<E> {
    /// Reading or writing the transport failed.
    #[error("transport failed")]
    Transport(#[source] E),
    /// The node did not answer the hello in time.
    #[error("the node did not answer the handshake in time")]
    Timeout,
    /// The node closed the connection during the handshake.
    #[error("the node closed the connection during the handshake")]
    Closed,
    /// The node refused the connection (for example no common protocol).
    #[error("the node rejected the connection: {0}")]
    Rejected(WireError),
    /// The node broke the protocol.
    #[error("protocol violation")]
    Violation(#[source] ProtocolViolation),
    /// The node's answer was not a valid server frame.
    #[error("undecodable frame from the node")]
    Decode(#[source] serde_json::Error),
    /// The hello could not be serialised (a bug).
    #[error("could not encode the hello")]
    Encode(#[source] serde_json::Error),
}

/// Why a call did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The connection closed before the response arrived. The call may or
    /// may not have taken effect on the node.
    #[error("the connection to the node closed")]
    Disconnected,
    /// The node answered with an error.
    #[error("the node returned an error: {0}")]
    Remote(WireError),
    /// The node answered with a reply for a different method (a broken peer).
    #[error(transparent)]
    UnexpectedReply(UnexpectedReply),
    /// This connection has used every request id (2^64 calls).
    #[error("request ids exhausted on this connection")]
    RequestIdsExhausted,
    /// The call needs a newer protocol than this connection negotiated, so
    /// it was not sent (the node would refuse it, or ignore a field it does
    /// not know).
    #[error("`{}` needs protocol {needs}; the node speaks {negotiated}", method.as_str())]
    NotInProtocol {
        /// The method.
        method: Method,
        /// The version the call needs.
        needs: ProtocolVersion,
        /// The connection's version.
        negotiated: ProtocolVersion,
    },
}

impl CallError {
    /// The node's error code, if the node answered with an error.
    #[must_use]
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote(error) => Some(error.code),
            Self::NotInProtocol { .. } => Some(ErrorCode::Unsupported),
            Self::Disconnected | Self::UnexpectedReply(_) | Self::RequestIdsExhausted => None,
        }
    }
}

/// Why the server could not open or close a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OutboundError {
    /// The connection is closing or closed.
    #[error("the connection is closed")]
    ConnectionClosed,
    /// No open subscription has this id on this connection.
    #[error("no subscription {0} on this connection")]
    UnknownSubscription(SubscriptionId),
    /// This connection has used every subscription id (2^64).
    #[error("subscription ids exhausted on this connection")]
    SubscriptionIdsExhausted,
}

impl OutboundError {
    /// The wire error code a handler should answer with.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::ConnectionClosed | Self::SubscriptionIdsExhausted => ErrorCode::Unavailable,
            Self::UnknownSubscription(_) => ErrorCode::NotFound,
        }
    }

    /// The wire error a handler should answer with.
    #[must_use]
    pub fn to_wire(self) -> WireError {
        WireError::new(self.code(), self.to_string())
    }
}
