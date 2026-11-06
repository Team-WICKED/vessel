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
//! - Keepalive ([`Keepalive`], one set of numbers): one end pings every
//!   10 s and both ends fail a connection after 30 s of silence
//!   (`TimedOut`), pings and pongs included, so a half-open connection (a
//!   laptop that slept, a NAT mapping that expired) cannot hang a reader.
//!   Through a relay, the relay pings both sides; on a direct route the
//!   node pings its `/secure` connections ([`AxumCarrier`] with a
//!   keepalive). Either way the client's carrier has the same silence
//!   limit, so a client treats a direct route and a relayed one alike, and
//!   the reconnecting stream takes over when it fires.
//! - Pings from inside a carrier: the channel owns the carrier, and the RPC
//!   layer splits it into a reading and a writing task, so a ping is sent
//!   from whichever side is polled: the reading side when nothing is being
//!   written (a quiet connection, the case that needs pings), the writing
//!   side before its next record otherwise. The reading side never touches
//!   the socket's write half while a write is in flight: the websocket
//!   keeps one waker per direction, and taking it would leave the writing
//!   task asleep.
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

use futures_util::Sink;
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

/// How a connection is kept alive: one end pings every `ping`, and either
/// end gives up after `silence` without a message. The same numbers for a
/// relay's connections, a node's `/secure` and every client carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keepalive {
    /// How often the pinging end pings.
    pub ping: Duration,
    /// Silence after which either end drops the connection: three pings
    /// missed.
    pub silence: Duration,
}

impl Keepalive {
    /// Ping every 10 s, give up after 30 s of silence.
    pub const DEFAULT: Self = Self {
        ping: Duration::from_secs(10),
        silence: Duration::from_secs(30),
    };
}

impl Default for Keepalive {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// When this end owes its peer a ping (a node's `/secure` connections,
/// which nobody else pings).
struct Pinger {
    every: Duration,
    next: Pin<Box<Sleep>>,
    /// A ping is owed and not yet handed to the socket.
    owed: bool,
    /// A ping was handed to the socket and not yet flushed.
    unflushed: bool,
}

impl Pinger {
    fn new(every: Option<Duration>) -> Option<Self> {
        every.map(|every| Self {
            every,
            next: Box::pin(tokio::time::sleep(every)),
            owed: false,
            unflushed: false,
        })
    }

    /// Whether a ping is owed now; otherwise the next one wakes `cx`.
    fn poll_owed(&mut self, cx: &mut Context<'_>) -> bool {
        if !self.owed && self.next.as_mut().poll(cx).is_ready() {
            self.owed = true;
            let now = Instant::now();
            self.next
                .as_mut()
                .reset(now.checked_add(self.every).unwrap_or(now));
            // Registers `cx` for the one after.
            let _pending = self.next.as_mut().poll(cx);
        }
        self.owed
    }
}

/// Hand an owed ping to `socket`, from the reading side: only when no
/// write is in flight (`writing`), and without waiting; a flush that cannot
/// finish now is finished on a later call or by the next write.
fn poll_ping<S, M>(
    pinger: &mut Option<Pinger>,
    mut socket: Pin<&mut S>,
    ping: impl FnOnce() -> M,
    writing: bool,
    cx: &mut Context<'_>,
) -> Result<(), S::Error>
where
    S: Sink<M>,
{
    let Some(pinger) = pinger else {
        return Ok(());
    };
    if writing {
        return Ok(());
    }
    if pinger.unflushed {
        match socket.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => pinger.unflushed = false,
            Poll::Ready(Err(error)) => return Err(error),
            Poll::Pending => return Ok(()),
        }
    }
    if !pinger.poll_owed(cx) {
        return Ok(());
    }
    match socket.as_mut().poll_ready(cx) {
        Poll::Ready(Ok(())) => {}
        Poll::Ready(Err(error)) => return Err(error),
        Poll::Pending => return Ok(()),
    }
    socket.as_mut().start_send(ping())?;
    pinger.owed = false;
    match socket.poll_flush(cx) {
        Poll::Ready(Ok(())) => {}
        Poll::Ready(Err(error)) => return Err(error),
        Poll::Pending => pinger.unflushed = true,
    }
    Ok(())
}

/// Hand an owed ping to `socket` from the writing side, before its next
/// record; `socket` is ready (its `poll_ready` returned `Ready(Ok)`).
fn ping_before_write<S, M>(
    pinger: &mut Option<Pinger>,
    socket: Pin<&mut S>,
    ping: impl FnOnce() -> M,
) -> Result<bool, S::Error>
where
    S: Sink<M>,
{
    match pinger {
        Some(pinger) if pinger.owed => {
            socket.start_send(ping())?;
            pinger.owed = false;
            pinger.unflushed = true;
            Ok(true)
        }
        Some(_) | None => Ok(false),
    }
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
