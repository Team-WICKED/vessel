//! Errors of this crate. Call failures are `vessel_rpc::CallError`,
//! re-exported as [`CallError`](crate::CallError).

use std::time::Duration;

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
    /// `wss://` is not supported in milestone 1: no TLS stack is built
    /// (ADR 0005, transport security). Remote nodes arrive with the SSH and
    /// relay transports, which choose TLS with an explicit crypto provider.
    #[error(
        "TLS (wss://) connections are not supported yet; reach remote nodes through an SSH tunnel"
    )]
    TlsUnsupported,
    /// The node did not accept the connection in time.
    #[error("{url} did not answer in time")]
    Timeout {
        /// Where.
        url: NodeUrl,
    },
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

impl ConnectError {
    /// Whether trying again later may succeed: the node is down or the
    /// network failed, as opposed to a refusal or a mismatch that the next
    /// attempt meets again (a wrong token, no common protocol).
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Websocket { .. }
            | Self::Timeout { .. }
            | Self::Handshake(
                vessel_rpc::ConnectError::Transport(_)
                | vessel_rpc::ConnectError::Timeout
                | vessel_rpc::ConnectError::Closed,
            ) => true,
            Self::TlsUnsupported
            | Self::Unauthorized
            | Self::InvalidToken
            | Self::Handshake(
                vessel_rpc::ConnectError::Rejected(_)
                | vessel_rpc::ConnectError::Violation(_)
                | vessel_rpc::ConnectError::Decode(_)
                | vessel_rpc::ConnectError::Encode(_),
            ) => false,
        }
    }
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
    /// The connection was lost (or a new one could not be opened yet); a
    /// [`ResumingStream`](crate::ResumingStream) dials again in `retry_in`.
    /// Not fatal. `attempt` counts from 1 for each loss.
    #[error("{}", reconnecting(*attempt, *retry_in, reason))]
    Reconnecting {
        /// Which attempt comes next, from 1.
        attempt: u32,
        /// How long until it.
        retry_in: Duration,
        /// Why the connection is down: the loss itself, or why the previous
        /// attempt failed.
        reason: String,
    },
    /// A [`ResumingStream`](crate::ResumingStream) is connected again and
    /// continues at `at`, its cursor; nothing before it is repeated. Not
    /// fatal.
    #[error("reconnected; resuming at record {at}")]
    Resumed {
        /// The next record the stream yields.
        at: EventSeq,
    },
    /// Connecting again failed in a way that waiting does not fix (the node
    /// refused the token, its identity changed, no common protocol). Fatal.
    #[error("could not reconnect: {reason}")]
    Unreachable {
        /// Why, with its causes.
        reason: String,
    },
}

/// The text of [`StreamError::Reconnecting`]: the first attempt says what
/// happened; later ones say what went wrong and when the next try is.
fn reconnecting(attempt: u32, retry_in: Duration, reason: &str) -> String {
    if attempt <= 1 {
        "connection lost; reconnecting\u{2026}".to_owned()
    } else {
        format!(
            "still reconnecting (attempt {attempt} in {:.1}s): {reason}",
            retry_in.as_secs_f64()
        )
    }
}

impl StreamError {
    /// Whether the stream ends after this error.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        match self {
            Self::Missing { .. } | Self::Reconnecting { .. } | Self::Resumed { .. } => false,
            Self::Call(_) | Self::Unsubscribed | Self::Unreachable { .. } => true,
        }
    }
}
