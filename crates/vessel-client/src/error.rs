//! Errors of this crate. Call failures are `vessel_rpc::CallError`,
//! re-exported as [`CallError`](crate::CallError).

use tokio_tungstenite::tungstenite;
use vessel_proto::EventSeq;
use vessel_rpc::CallError;

use crate::url::NodeUrl;
use crate::ws::WsTransportError;

/// Why [`NodeClient::connect`](crate::NodeClient::connect) failed.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The websocket could not be opened (node down, wrong address, TLS).
    #[error("could not open a websocket to {url}")]
    Websocket {
        /// Where.
        url: NodeUrl,
        /// Why.
        #[source]
        source: tungstenite::Error,
    },
    /// `wss://` is not supported yet. The workspace builds `rustls` without
    /// a crypto provider, and `tokio-tungstenite` would panic on the first
    /// TLS handshake; remote nodes arrive with the relay and SSH transports.
    #[error(
        "TLS (wss://) connections are not supported yet; reach remote nodes through an SSH tunnel"
    )]
    TlsUnsupported,
    /// The node refused the token.
    #[error("the node refused the token")]
    Unauthorized,
    /// The token cannot be put in a request header.
    #[error("the token cannot be sent in a request header")]
    InvalidToken,
    /// The websocket opened but the protocol handshake failed (for example
    /// no common protocol version).
    #[error("the protocol handshake with the node failed")]
    Handshake(#[source] vessel_rpc::ConnectError<WsTransportError>),
}

/// Why a [`SessionStream`](crate::SessionStream) yielded an error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// Records `expected..resumed_at` are no longer on the node (retention
    /// pruned them). Not fatal: the stream continues at `resumed_at`.
    #[error("records {expected}..{resumed_at} were pruned by the node")]
    Missing {
        /// The first record that was wanted.
        expected: EventSeq,
        /// Where the stream continues.
        resumed_at: EventSeq,
    },
    /// A call (subscribe or read) failed, or the connection closed. Fatal.
    #[error(transparent)]
    Call(#[from] CallError),
    /// The node ended the subscription without the session ending (someone
    /// unsubscribed it). Fatal.
    #[error("the node closed the subscription")]
    Unsubscribed,
}

impl StreamError {
    /// Whether the stream ends after this error.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        match self {
            Self::Missing { .. } => false,
            Self::Call(_) | Self::Unsubscribed => true,
        }
    }
}
