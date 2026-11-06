//! The node's side: stay registered with one relay, and open a data
//! connection for every client the relay announces.
//!
//! [Decision Log]
//! - Purpose: a node behind NAT is reachable only while its control
//!   connection to the relay is up, so the link must come back on its own
//!   after a relay restart, a network change, or a half-open connection.
//! - Choice: one loop per relay: connect, register (token, then the
//!   possession proof), then serve `Incoming` until the connection ends;
//!   then wait and start over. The wait is 0.5 s doubling to 30 s with
//!   jitter, back to 0.5 s after a registration that worked, and 30 s
//!   after a refusal (a wrong token does not fix itself).
//! - A connection silent for 30 s (the relay pings every 10 s) counts as
//!   lost, so a half-open one is replaced; the relay replaces the old
//!   registration once the new one proves the key.
//! - Each client's data connection is opened in a task of its own and
//!   handed, as a carrier, to the node's `accept`: the node runs
//!   `vessel_secure::respond` on it, exactly as for its own `/secure`.
//! - The state is published on a watch ([`LinkState`]) for `vessel node
//!   status` and for tests to wait on, instead of sleeping.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use vessel_proto::{ConnId, NodeToRelay, RegistrationRefusal, RelayToNode, RelayToken, RouteUrl};
use vessel_secure::{NodeKeypair, answer_proof, routing_id};

use crate::backoff::Backoff;
use crate::carrier::WsCarrier;
use crate::dial::IDLE;
use crate::socket::{self, Socket};
use crate::tls::TlsConfig;
use crate::{ACCEPT_PATH, NODE_PATH, endpoint};

/// How long connecting, and each step of the registration, may take.
const STEP: Duration = Duration::from_secs(10);

/// Where a node's link to one relay stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkState {
    /// Connecting or registering (again).
    Connecting,
    /// Registered: clients can reach the node through this relay.
    Registered,
    /// The relay refused the registration; retried every 30 s.
    Refused(RegistrationRefusal),
    /// The link was shut down.
    Stopped,
}

/// A node's link to one relay.
pub struct NodeLink {
    relay: RouteUrl,
    keypair: Arc<NodeKeypair>,
    token: RelayToken,
    tls: TlsConfig,
    state: watch::Sender<LinkState>,
}

impl std::fmt::Debug for NodeLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeLink")
            .field("relay", &self.relay)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

/// How one connection to the relay ended.
enum Ended {
    /// It never got registered.
    Failed,
    /// It was registered, then lost.
    Lost,
    /// The relay refused the registration.
    Refused(RegistrationRefusal),
}

impl NodeLink {
    /// A link to `relay` for the node with `keypair`, registering with the
    /// relay's `token`.
    #[must_use]
    pub fn new(
        relay: RouteUrl,
        keypair: Arc<NodeKeypair>,
        token: RelayToken,
        tls: TlsConfig,
    ) -> Self {
        let (state, _receiver) = watch::channel(LinkState::Connecting);
        Self {
            relay,
            keypair,
            token,
            tls,
            state,
        }
    }

    /// Follow the link's state.
    #[must_use]
    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.state.subscribe()
    }

    /// Keep the node registered until `shutdown`, handing each client's
    /// data connection to `accept`.
    pub async fn run<F>(self, accept: F, shutdown: CancellationToken)
    where
        F: Fn(WsCarrier) + Send + Sync + 'static,
    {
        let accept = Arc::new(accept);
        let mut backoff = Backoff::new();
        loop {
            let _previous = self.state.send_replace(LinkState::Connecting);
            let ended = tokio::select! {
                ended = self.connection(&accept) => ended,
                () = shutdown.cancelled() => break,
            };
            match ended {
                Ended::Failed => {}
                Ended::Lost => backoff.reset(),
                Ended::Refused(reason) => {
                    tracing::warn!(relay = %self.relay, ?reason, "the relay refused the registration");
                    let _previous = self.state.send_replace(LinkState::Refused(reason));
                    backoff.exhaust();
                }
            }
            let delay = backoff.next_delay();
            tracing::debug!(relay = %self.relay, ?delay, "reconnecting to the relay");
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shutdown.cancelled() => break,
            }
        }
        let _previous = self.state.send_replace(LinkState::Stopped);
    }

    /// One control connection, from connecting until it ends.
    async fn connection<F>(&self, accept: &Arc<F>) -> Ended
    where
        F: Fn(WsCarrier) + Send + Sync + 'static,
    {
        let url = endpoint(&self.relay, NODE_PATH);
        let mut socket = match timeout(STEP, socket::open(&url, &self.tls)).await {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                tracing::debug!(relay = %self.relay, %error, "could not reach the relay");
                return Ended::Failed;
            }
            Err(_elapsed) => return Ended::Failed,
        };
        match self.register(&mut socket).await {
            Ok(()) => {}
            Err(Some(reason)) => return Ended::Refused(reason),
            Err(None) => return Ended::Failed,
        }
        tracing::info!(relay = %self.relay, "registered with the relay");
        let _previous = self.state.send_replace(LinkState::Registered);
        loop {
            let message = match timeout(IDLE, socket.next()).await {
                Ok(Some(Ok(message))) => message,
                Ok(Some(Err(error))) => {
                    tracing::debug!(relay = %self.relay, %error, "lost the relay");
                    return Ended::Lost;
                }
                Ok(None) => return Ended::Lost,
                Err(_elapsed) => {
                    tracing::debug!(relay = %self.relay, "the relay went silent");
                    return Ended::Lost;
                }
            };
            match message {
                Message::Text(text) => match serde_json::from_str::<RelayToNode>(&text) {
                    Ok(RelayToNode::Incoming { conn }) => self.open_data(conn, Arc::clone(accept)),
                    Ok(other) => {
                        tracing::debug!(?other, "unexpected relay message");
                        return Ended::Lost;
                    }
                    Err(error) => {
                        tracing::debug!(%error, "malformed relay message");
                        return Ended::Lost;
                    }
                },
                Message::Close(_) => return Ended::Lost,
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    /// Register and prove the key. `Err(Some)` is a refusal, `Err(None)` any
    /// other failure.
    async fn register(&self, socket: &mut Socket) -> Result<(), Option<RegistrationRefusal>> {
        let key = *self.keypair.public();
        send(
            socket,
            &NodeToRelay::Register {
                routing_id: routing_id(&key),
                key,
                token: self.token.clone(),
            },
        )
        .await?;
        let challenge = match receive::<RelayToNode>(socket).await? {
            RelayToNode::Prove { message } => message,
            RelayToNode::Rejected { reason } => return Err(Some(reason)),
            RelayToNode::Registered | RelayToNode::Incoming { .. } => return Err(None),
        };
        let message = answer_proof(&self.keypair, &challenge).map_err(|error| {
            tracing::debug!(%error, "could not answer the relay's challenge");
            None
        })?;
        send(socket, &NodeToRelay::Proof { message }).await?;
        match receive::<RelayToNode>(socket).await? {
            RelayToNode::Registered => Ok(()),
            RelayToNode::Rejected { reason } => Err(Some(reason)),
            RelayToNode::Prove { .. } | RelayToNode::Incoming { .. } => Err(None),
        }
    }

    /// Open the data connection for a waiting client and hand it over.
    fn open_data<F>(&self, conn: ConnId, accept: Arc<F>)
    where
        F: Fn(WsCarrier) + Send + Sync + 'static,
    {
        let url = endpoint(&self.relay, &format!("{ACCEPT_PATH}?conn={conn}"));
        let tls = self.tls.clone();
        drop(tokio::spawn(async move {
            match timeout(STEP, socket::open(&url, &tls)).await {
                Ok(Ok(socket)) => accept(WsCarrier::new(socket, Some(IDLE))),
                Ok(Err(error)) => tracing::debug!(%error, "could not accept a relayed client"),
                Err(_elapsed) => tracing::debug!("accepting a relayed client timed out"),
            }
        }));
    }
}

async fn send<T: Serialize>(
    socket: &mut Socket,
    message: &T,
) -> Result<(), Option<RegistrationRefusal>> {
    let text = serde_json::to_string(message).map_err(|error| {
        tracing::debug!(%error, "could not encode a relay message");
        None
    })?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|error| {
            tracing::debug!(%error, "could not write to the relay");
            None
        })?;
    Ok(())
}

async fn receive<T: DeserializeOwned>(
    socket: &mut Socket,
) -> Result<T, Option<RegistrationRefusal>> {
    loop {
        let message = match timeout(STEP, socket.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                tracing::debug!(%error, "lost the relay while registering");
                return Err(None);
            }
            Ok(None) | Err(_) => return Err(None),
        };
        match message {
            Message::Text(text) => {
                return serde_json::from_str(&text).map_err(|error| {
                    tracing::debug!(%error, "malformed relay message");
                    None
                });
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Binary(_) | Message::Close(_) => return Err(None),
        }
    }
}
