//! Clients and the node's data connections: announce, wait, splice.
//!
//! [Decision Log]
//! - Purpose: join one client's websocket to the node's data connection
//!   for it, and nothing else.
//! - Constraint: a relay must not buffer for anyone (ADR 0009): Paseo keeps
//!   a client's first 200 frames and drops the oldest.
//! - Choice: the client's socket is not read at all until the node's data
//!   connection is there. Its first messages wait in the kernel, with TCP
//!   backpressure, and nothing is held or dropped here. Once paired, each
//!   direction is forwarded one message at a time: a slow reader slows its
//!   writer, end to end.
//! - Every refusal is a close code (`RelayClose`); refusals by the node
//!   travel inside the channel.
//! - Resource caps (`Limits`): a client holds a place, to its node and in
//!   all, from the moment its node is found until the splice ends
//!   (`RelayClose::Full` past either), and each direction of a splice is
//!   held to `max_bytes_per_second` (`ByteRate`, waiting before a message
//!   is forwarded, so the sender is pushed back by TCP). A splice silent
//!   in one direction past the idle timeout ends (pongs count).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Extension, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio::time::sleep;
use vessel_proto::{ConnId, RelayClose, RoutingId};

use super::accept::Authenticated;
use super::control::after;
use super::throttle::ByteRate;
use super::{Client, Limits, Relay, close_with, lock, too_large};

#[derive(Debug, Deserialize)]
pub(super) struct ConnectQuery {
    node: RoutingId,
}

#[derive(Debug, Deserialize)]
pub(super) struct AcceptQuery {
    conn: ConnId,
}

pub(super) async fn connect(
    State(relay): State<Arc<Relay>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(socket_place): Extension<Authenticated>,
    headers: HeaderMap,
    Query(query): Query<ConnectQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let client = relay.client_address(peer, &headers);
    let admitted = relay.admit(client);
    relay
        .limit(upgrade)
        .on_upgrade(move |mut socket| async move {
            if !admitted {
                close_with(&mut socket, RelayClose::RateLimited).await;
                return;
            }
            pair(&relay, query.node, socket, client, &socket_place).await;
        })
}

pub(super) async fn accept(
    State(relay): State<Arc<Relay>>,
    Extension(socket_place): Extension<Authenticated>,
    Query(query): Query<AcceptQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    relay.limit(upgrade).on_upgrade(move |node| async move {
        let waiting = lock(&relay.pending).remove(&query.conn);
        let unclaimed = match waiting {
            // The connection id was announced to the registered node
            // only: naming it is the node's data connection
            // authenticating.
            Some(client) => {
                socket_place.promote();
                client.send(node).err()
            }
            None => Some(node),
        };
        // The client gave up (or never was): nothing to join.
        if let Some(mut node) = unclaimed {
            close_with(&mut node, RelayClose::NotAccepted).await;
        }
    })
}

/// A place among a node's waiting clients, given back on drop.
struct Waiting(Arc<AtomicUsize>);

impl Waiting {
    fn enter(count: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiting| {
                waiting.checked_add(1).filter(|&next| next <= max)
            })
            .ok()
            .map(|_previous| Self(Arc::clone(count)))
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let _previous = self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn new_conn() -> Option<ConnId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(ConnId::from_bytes(bytes))
}

/// Announce `client` to `node`, wait for the node's data connection, then
/// splice them. Paired, the client's socket has authenticated
/// (`socket_place`): the node took it.
async fn pair(
    relay: &Relay,
    node: RoutingId,
    mut client: WebSocket,
    from: Client,
    socket_place: &Authenticated,
) {
    let slot = lock(&relay.nodes).get(&node).map(|registration| {
        (
            registration.incoming.clone(),
            Arc::clone(&registration.waiting),
        )
    });
    let Some((incoming, waiting)) = slot else {
        close_with(&mut client, RelayClose::NoSuchNode).await;
        return;
    };
    // Held until this function returns: through the wait and the splice.
    let Some(_client) = relay.client(node, from.address) else {
        close_with(&mut client, RelayClose::Full).await;
        return;
    };
    let Some(place) = Waiting::enter(&waiting, relay.limits.waiting_per_node) else {
        close_with(&mut client, RelayClose::RateLimited).await;
        return;
    };
    let Some(conn) = new_conn() else {
        tracing::warn!("no randomness for a connection id");
        close_with(&mut client, RelayClose::NotAccepted).await;
        return;
    };
    let (deliver, delivered) = oneshot::channel();
    let _fresh = lock(&relay.pending).insert(conn, deliver);
    if incoming.try_send(conn).is_err() {
        // The node's control connection is gone (or behind by a full
        // queue, which the waiting cap rules out).
        let _gone = lock(&relay.pending).remove(&conn);
        close_with(&mut client, RelayClose::NoSuchNode).await;
        return;
    }
    let accepted = tokio::select! {
        accepted = tokio::time::timeout(relay.limits.accept_timeout, delivered) => {
            accepted.ok().and_then(Result::ok)
        }
        () = relay.shutdown.cancelled() => None,
    };
    let _late = lock(&relay.pending).remove(&conn);
    drop(place);
    match accepted {
        Some(node_socket) => {
            socket_place.promote();
            relay.refund(from);
            splice(relay, client, node_socket).await;
        }
        None => close_with(&mut client, RelayClose::NotAccepted).await,
    }
}

/// Why forwarding one direction stopped.
enum End {
    /// The sender closed, with this frame: it goes to the other side
    /// unchanged (a node's `4401` must reach the client).
    Closed(Option<CloseFrame>),
    /// The sender failed, or the receiver went away.
    Gone,
    /// The sender was silent past the idle timeout.
    Idle,
    /// The sender sent a message over the limit.
    TooLarge,
    /// The sender sent text: relays carry records only.
    Protocol,
}

async fn splice(relay: &Relay, client: WebSocket, node: WebSocket) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut node_tx, mut node_rx) = node.split();
    let limits = &relay.limits;
    let (end, client_sent) = tokio::select! {
        // The client speaks first (its handshake message): one that says
        // nothing gives its place back within `first_message_timeout`.
        end = forward(&mut client_rx, &mut node_tx, limits, Some(limits.first_message_timeout)) => (end, true),
        end = forward(&mut node_rx, &mut client_tx, limits, None) => (end, false),
        () = relay.shutdown.cancelled() => (End::Gone, true),
    };
    let (sender, receiver) = if client_sent {
        (&mut client_tx, &mut node_tx)
    } else {
        (&mut node_tx, &mut client_tx)
    };
    match end {
        End::Closed(frame) => {
            close_quietly(sender, None).await;
            close_quietly(receiver, frame).await;
        }
        End::TooLarge => {
            close_with(sender, RelayClose::TooLarge).await;
            close_quietly(receiver, None).await;
        }
        End::Gone | End::Idle | End::Protocol => {
            close_quietly(sender, None).await;
            close_quietly(receiver, None).await;
        }
    }
}

/// Forward `from`'s records to `to`, pinging `to` and watching `from`'s
/// silence, at most `max_bytes_per_second`, until something ends it.
async fn forward(
    from: &mut SplitStream<WebSocket>,
    to: &mut SplitSink<WebSocket, Message>,
    limits: &Limits,
    first_record: Option<std::time::Duration>,
) -> End {
    let largest = u64::try_from(limits.max_message).unwrap_or(u64::MAX);
    let mut rate = ByteRate::new(
        limits.max_bytes_per_second,
        largest,
        tokio::time::Instant::now(),
    );
    let mut ping = tokio::time::interval_at(after(limits.ping_interval), limits.ping_interval);
    let idle = sleep(limits.idle_timeout);
    tokio::pin!(idle);
    // Pings and pongs do not count here: only a record shows a client.
    let mut awaiting_first = first_record.is_some();
    let first = sleep(first_record.unwrap_or(limits.idle_timeout));
    tokio::pin!(first);
    loop {
        tokio::select! {
            message = from.next() => {
                let record = match message {
                    Some(Ok(Message::Binary(record))) => {
                        awaiting_first = false;
                        record
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        idle.as_mut().reset(after(limits.idle_timeout));
                        continue;
                    }
                    Some(Ok(Message::Text(_))) => return End::Protocol,
                    Some(Ok(Message::Close(frame))) => return End::Closed(frame),
                    None => return End::Closed(None),
                    Some(Err(error)) if too_large(&error) => return End::TooLarge,
                    Some(Err(_)) => return End::Gone,
                };
                if let Some(rate) = rate.as_mut() {
                    let length = u64::try_from(record.len()).unwrap_or(u64::MAX);
                    let wait = rate.take(length, tokio::time::Instant::now());
                    if !wait.is_zero() {
                        sleep(wait).await;
                    }
                }
                if to.send(Message::Binary(record)).await.is_err() {
                    return End::Gone;
                }
                // `from` was not silent while the receiver held us back.
                idle.as_mut().reset(after(limits.idle_timeout));
            }
            _ = ping.tick() => {
                if to.send(Message::Ping(Bytes::new())).await.is_err() {
                    return End::Gone;
                }
            }
            () = &mut idle => return End::Idle,
            () = &mut first, if awaiting_first => return End::Idle,
        }
    }
}

async fn close_quietly(socket: &mut SplitSink<WebSocket, Message>, frame: Option<CloseFrame>) {
    let closing = async {
        let _sent = socket.send(Message::Close(frame)).await;
        let _closed = socket.close().await;
    };
    let _timed_out = tokio::time::timeout(std::time::Duration::from_secs(1), closing).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_places_are_capped_and_given_back() {
        let count = Arc::new(AtomicUsize::new(0));
        let first = Waiting::enter(&count, 2).unwrap();
        let second = Waiting::enter(&count, 2).unwrap();
        assert!(Waiting::enter(&count, 2).is_none());
        drop(first);
        let third = Waiting::enter(&count, 2).unwrap();
        drop((second, third));
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
