//! Websockets as [`Carrier`]s for the secure channel: [`WsCarrier`] for
//! the websockets nodes and clients open (to a relay, or to a node's
//! `/secure`), [`AxumCarrier`] for the ones a node accepts on `/secure`.
//!
//! [Decision Log]
//! - Purpose: the channel needs whole binary messages; nodes and clients
//!   also need to know why a connection was closed: a relay's code (`4404`
//!   means the node may be restarting, `4429` to slow down) or a node's
//!   (`4401`, the handshake failed: pair again).
//! - Choice: a close frame with one of those codes is an error carrying
//!   [`RelayClosed`] or [`NodeClosed`], found again through
//!   `SecureError::Carrier` with [`relay_close`] and [`node_close`]. Any
//!   other close ends the stream. A text message is an error: these
//!   connections carry records only.
//! - Idle: a relay pings every 10 s, so a carrier through one fails after
//!   30 s of silence (`TimedOut`) and a half-open connection cannot hang a
//!   reader. Direct connections have no pinger, so no idle limit unless
//!   their owner sets one.
//! - The node's handshake: with NK only the node notices a client that
//!   pinned the wrong key, so [`respond`] closes with `4401` when the
//!   handshake fails (`vessel_proto::NodeClose`), and relays forward it.
//! - One module for both websocket libraries: the node's `/secure` (axum)
//!   and its relayed connections (tungstenite) take the same path into
//!   [`respond`].

mod accepted;
mod opened;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::{Instant, Sleep};
use vessel_proto::{NodeClose, RelayClose};
use vessel_secure::{
    Carrier, Config, NodeKeypair, RespondError, SecureChannel, SecureError, try_respond,
};

pub use self::accepted::AxumCarrier;
pub use self::opened::WsCarrier;

/// A relay closed the connection with one of its close codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the relay closed the connection ({})", describe_relay(*.0))]
pub struct RelayClosed(pub RelayClose);

/// A node closed the connection with one of its close codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the node closed the connection ({})", describe_node(*.0))]
pub struct NodeClosed(pub NodeClose);

fn describe_relay(close: RelayClose) -> &'static str {
    match close {
        RelayClose::NoSuchNode => "the node is not connected to it",
        RelayClose::NotAccepted => "the node did not accept in time",
        RelayClose::TooLarge => "a message was too large",
        RelayClose::RateLimited => "rate limited",
    }
}

fn describe_node(close: NodeClose) -> &'static str {
    match close {
        NodeClose::HandshakeFailed => {
            "the secure handshake failed: the node's key is not the one pinned; pair it again"
        }
    }
}

/// The relay close code inside a carrier error, if that is why it failed.
#[must_use]
pub fn relay_close(error: &io::Error) -> Option<RelayClose> {
    error
        .get_ref()?
        .downcast_ref::<RelayClosed>()
        .map(|closed| closed.0)
}

/// The node close code inside a carrier error, if that is why it failed.
/// `HandshakeFailed` is final: the client pinned another key.
#[must_use]
pub fn node_close(error: &io::Error) -> Option<NodeClose> {
    error
        .get_ref()?
        .downcast_ref::<NodeClosed>()
        .map(|closed| closed.0)
}

/// What a close frame with `code` means to a reader: an error for the
/// relay's and the node's codes, a plain end otherwise.
fn closed(code: Option<u16>) -> Option<io::Error> {
    let code = code?;
    if let Some(close) = RelayClose::from_code(code) {
        return Some(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            RelayClosed(close),
        ));
    }
    NodeClose::from_code(code)
        .map(|close| io::Error::new(io::ErrorKind::ConnectionRefused, NodeClosed(close)))
}

fn text_message() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "a text message on a secure-channel connection",
    )
}

/// An optional limit on silence: reset by every message, fires only while
/// the socket has nothing to read.
struct Idle {
    after: Duration,
    deadline: Pin<Box<Sleep>>,
}

impl Idle {
    fn new(after: Option<Duration>) -> Option<Self> {
        after.map(|after| Self {
            after,
            deadline: Box::pin(tokio::time::sleep(after)),
        })
    }

    fn heard(this: &mut Option<Self>) {
        if let Some(idle) = this {
            let now = Instant::now();
            idle.deadline
                .as_mut()
                .reset(now.checked_add(idle.after).unwrap_or(now));
        }
    }

    /// Called when the socket is pending: an error once the silence is too
    /// long, `Pending` otherwise.
    fn poll_expired<T>(
        this: &mut Option<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<T>>> {
        let Some(idle) = this else {
            return Poll::Pending;
        };
        if idle.deadline.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        Poll::Ready(Some(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the connection went silent",
        ))))
    }
}

/// A carrier the node can close with a reason the client reads.
pub trait NodeCarrier: Carrier {
    /// Close with `close`'s code (best effort, bounded in time).
    fn close_with(self, close: NodeClose) -> impl Future<Output = ()> + Send;
}

/// How long closing with a code may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// The node's side of a secure channel on a websocket:
/// [`vessel_secure::respond_with`], closing the websocket with
/// `NodeClose::HandshakeFailed` (`4401`) when the handshake itself fails,
/// so a client that pinned another key stops retrying.
///
/// # Errors
/// As [`vessel_secure::respond`].
pub async fn respond<C: NodeCarrier>(
    carrier: C,
    keypair: &NodeKeypair,
    config: &Config,
) -> Result<SecureChannel<C>, SecureError> {
    match try_respond(carrier, keypair, config).await {
        Ok(channel) => Ok(channel),
        Err(failed) => {
            let RespondError { error, carrier, .. } = failed;
            if matches!(
                error,
                SecureError::Handshake(_) | SecureError::HandshakeMessage { .. }
            ) {
                carrier.close_with(NodeClose::HandshakeFailed).await;
            }
            Err(error)
        }
    }
}
