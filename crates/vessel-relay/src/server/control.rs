//! A node's control connection: registration (token or ticket, then
//! possession proof), then `Incoming` for every waiting client, pings both
//! ways. A registration made with a ticket ends when the ticket expires;
//! the node registers again with a newer one (`crate::ticket`).
//!
//! A node is registered only once it proved its key, and only then does it
//! replace an earlier registration for the same routing id: a stranger
//! cannot black-hole a node's clients, and a node whose old connection went
//! half-open takes its slot back at once.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use subtle::ConstantTimeEq as _;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    ConnId, HandshakeMessage, NodePublicKey, NodeToRelay, RegistrationRefusal, RelayClose,
    RelayTicket, RelayToNode, RelayToken, RoutingId,
};
use vessel_secure::proof_challenge;

use crate::ticket::{check_ticket, unix_now};

use super::{Registration, Relay, close_with, too_large};

pub(super) async fn upgrade(
    State(relay): State<Arc<Relay>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let admitted = relay.admit(peer, &headers);
    relay
        .limit(upgrade)
        .on_upgrade(move |mut socket| async move {
            if !admitted {
                close_with(&mut socket, RelayClose::RateLimited).await;
                return;
            }
            // A connection that has not registered yet holds a place among
            // those registering (`Limits::max_registering`) until it has.
            let Some(registering) = relay.registering() else {
                close_with(&mut socket, RelayClose::Full).await;
                return;
            };
            let registered = tokio::select! {
                registered = register(&relay, &mut socket) => registered,
                () = relay.shutdown.cancelled() => None,
            };
            drop(registering);
            if let Some((node, until)) = registered {
                serve(&relay, socket, node, until).await;
            }
        })
}

/// What reading one control message gave.
enum Read {
    Message(NodeToRelay),
    /// Not a control message: the wrong type, or not JSON.
    Malformed,
    /// Over the message limit.
    TooLarge,
    /// Closed, failed, or silent past the registration timeout.
    Gone,
}

async fn read(relay: &Relay, socket: &mut WebSocket) -> Read {
    let deadline = Instant::now()
        .checked_add(relay.limits.registration_timeout)
        .unwrap_or_else(Instant::now);
    loop {
        let message = match tokio::time::timeout_at(deadline, socket.recv()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) if too_large(&error) => return Read::TooLarge,
            Ok(Some(Err(_)) | None) | Err(_) => return Read::Gone,
        };
        match message {
            Message::Text(text) => {
                return serde_json::from_str(&text).map_or(Read::Malformed, Read::Message);
            }
            Message::Binary(_) => return Read::Malformed,
            Message::Close(_) => return Read::Gone,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

async fn send(socket: &mut WebSocket, message: &RelayToNode) -> bool {
    match serde_json::to_string(message) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(error) => {
            tracing::warn!(%error, "could not encode a relay message");
            false
        }
    }
}

/// Answer a failed read: a refusal the node can read, or the size close.
async fn refuse(socket: &mut WebSocket, read: Read, reason: RegistrationRefusal) {
    match read {
        Read::TooLarge => close_with(socket, RelayClose::TooLarge).await,
        Read::Gone => {}
        Read::Message(_) | Read::Malformed => {
            let _sent = send(socket, &RelayToNode::Rejected { reason }).await;
            let _closed = socket.send(Message::Close(None)).await;
        }
    }
}

fn token_matches(expected: &RelayToken, given: &RelayToken) -> bool {
    bool::from(
        expected
            .expose()
            .as_bytes()
            .ct_eq(given.expose().as_bytes()),
    )
}

/// Whether a registration may go on to the proof: with the relay's token
/// (no end), or a ticket for `key` (until it expires, in seconds since the
/// Unix epoch); one of the two, never both.
fn admitted(
    relay: &Relay,
    key: &NodePublicKey,
    token: Option<&RelayToken>,
    ticket: Option<&RelayTicket>,
) -> Result<Option<u64>, RegistrationRefusal> {
    match (token, ticket) {
        (Some(token), None) if token_matches(&relay.token, token) => Ok(None),
        (None, Some(ticket)) => check_ticket(&relay.token, ticket, key, unix_now())
            .map(|()| Some(ticket.expires_at))
            .map_err(|error| {
                tracing::info!(%error, "registration with a ticket refused");
                RegistrationRefusal::Ticket
            }),
        (Some(_), None | Some(_)) | (None, None) => Err(RegistrationRefusal::Token),
    }
}

/// Run the registration; the node's routing id once it proved its key,
/// and when a ticket it registered with expires.
async fn register(relay: &Relay, socket: &mut WebSocket) -> Option<(RoutingId, Option<u64>)> {
    let (routing_id, key, until) = match read(relay, socket).await {
        Read::Message(NodeToRelay::Register {
            routing_id,
            key,
            token,
            ticket,
        }) => match admitted(relay, &key, token.as_ref(), ticket.as_ref()) {
            Ok(until) => (routing_id, key, until),
            Err(reason) => {
                tracing::info!(%routing_id, ?reason, "registration refused");
                refuse(socket, Read::Malformed, reason).await;
                return None;
            }
        },
        other @ (Read::Message(NodeToRelay::Proof { .. })
        | Read::Malformed
        | Read::TooLarge
        | Read::Gone) => {
            refuse(socket, other, RegistrationRefusal::Protocol).await;
            return None;
        }
    };
    if !proves(relay, socket, routing_id, &key).await? {
        tracing::info!(%routing_id, "registration without proof of the key refused");
        refuse(socket, Read::Malformed, RegistrationRefusal::Proof).await;
        return None;
    }
    Some((routing_id, until))
}

/// Whether the node holds `key` (and `routing_id` is the key's). `None`
/// when the connection ended or broke the protocol (already answered).
async fn proves(
    relay: &Relay,
    socket: &mut WebSocket,
    routing_id: RoutingId,
    key: &NodePublicKey,
) -> Option<bool> {
    if vessel_secure::routing_id(key) != routing_id {
        return Some(false);
    }
    let (challenge, message) = match proof_challenge(key) {
        Ok(challenge) => challenge,
        Err(error) => {
            tracing::warn!(%error, "could not make a possession challenge");
            return None;
        }
    };
    if !send(socket, &RelayToNode::Prove { message }).await {
        return None;
    }
    let answer: HandshakeMessage = match read(relay, socket).await {
        Read::Message(NodeToRelay::Proof { message }) => message,
        other @ (Read::Message(NodeToRelay::Register { .. })
        | Read::Malformed
        | Read::TooLarge
        | Read::Gone) => {
            refuse(socket, other, RegistrationRefusal::Protocol).await;
            return None;
        }
    };
    Some(challenge.verify(&answer))
}

/// Serve a registered node until it goes, is replaced, its ticket expires
/// (`until`, seconds since the Unix epoch), or the relay stops.
async fn serve(relay: &Relay, mut socket: WebSocket, node: RoutingId, until: Option<u64>) {
    let generation = relay.next_generation();
    // As many announcements as clients may wait; at least one.
    let (incoming, mut announce) = mpsc::channel::<ConnId>(relay.limits.waiting_per_node.max(1));
    let replaced = CancellationToken::new();
    let registration = Registration {
        generation,
        incoming,
        waiting: Arc::new(AtomicUsize::new(0)),
        replaced: replaced.clone(),
    };
    if relay.register(node, registration).is_err() {
        // The relay holds its most nodes (`Limits::max_nodes`); logged there.
        let refusal = RelayToNode::Rejected {
            reason: RegistrationRefusal::Full,
        };
        let _sent = send(&mut socket, &refusal).await;
        let _closed = socket.send(Message::Close(None)).await;
        return;
    }
    tracing::info!(%node, "node registered");
    if send(&mut socket, &RelayToNode::Registered).await {
        tokio::select! {
            () = announce_until_gone(relay, &mut socket, &mut announce, &replaced) => {}
            () = expiry(until) => tracing::info!(%node, "the node's ticket expired"),
        }
    }
    relay.unregister(&node, generation);
    let _closed = socket.send(Message::Close(None)).await;
    tracing::info!(%node, "node gone");
}

async fn announce_until_gone(
    relay: &Relay,
    socket: &mut WebSocket,
    announce: &mut mpsc::Receiver<ConnId>,
    replaced: &CancellationToken,
) {
    let limits = &relay.limits;
    let first_ping = Instant::now()
        .checked_add(limits.ping_interval)
        .unwrap_or_else(Instant::now);
    let mut ping = tokio::time::interval_at(first_ping, limits.ping_interval);
    let idle = sleep(limits.idle_timeout);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            conn = announce.recv() => {
                let Some(conn) = conn else { return };
                if !send(socket, &RelayToNode::Incoming { conn }).await {
                    return;
                }
            }
            message = socket.recv() => {
                idle.as_mut().reset(after(limits.idle_timeout));
                match message {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                        tracing::debug!("unexpected message on a control connection");
                        return;
                    }
                    Some(Err(error)) => {
                        if too_large(&error) {
                            close_with(socket, RelayClose::TooLarge).await;
                        }
                        return;
                    }
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
            }
            () = &mut idle => {
                tracing::debug!("a node's control connection went silent");
                return;
            }
            () = replaced.cancelled() => {
                tracing::info!("a node registered again; dropping its old connection");
                return;
            }
            () = relay.shutdown.cancelled() => return,
        }
    }
}

/// Resolves when a ticket that expires at `until` (seconds since the Unix
/// epoch) has; never for a registration made with the token.
async fn expiry(until: Option<u64>) {
    match until {
        Some(until) => {
            let left = until.saturating_sub(unix_now());
            sleep(std::time::Duration::from_secs(left)).await;
        }
        None => std::future::pending().await,
    }
}

/// `duration` from now.
pub(super) fn after(duration: std::time::Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration).unwrap_or(now)
}
