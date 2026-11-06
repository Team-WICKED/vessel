//! The protocol's `Handler`: routes each call to the module that owns it
//! and maps its typed error to the wire, once. No logic of its own.

use std::sync::Arc;

use tokio_util::task::TaskTracker;
use vessel_harness::Lookup;
use vessel_proto::{
    Ack, CreateSession, EventPage, GetNode, GetSession, Handler, Harness, KillSession,
    ListHarnesses, ListSessions, NodeInfo, ReadEvents, SendInput, SessionInfo, StopNode, Subscribe,
    Subscribed, Unsubscribe, WireError,
};
use vessel_rpc::{Outbound, Peer};

use crate::error::ServiceError;
use crate::lifecycle::Lifecycle;
use crate::sessions::Sessions;
use crate::subscribe;

/// The node's protocol implementation.
#[derive(Debug)]
pub(crate) struct NodeService {
    lifecycle: Lifecycle,
    lookup: Lookup,
    sessions: Arc<Sessions>,
    /// Subscription forwarders, so shutdown can let them queue their last
    /// frames.
    forwarders: TaskTracker,
}

impl NodeService {
    pub(crate) const fn new(
        lifecycle: Lifecycle,
        lookup: Lookup,
        sessions: Arc<Sessions>,
        forwarders: TaskTracker,
    ) -> Self {
        Self {
            lifecycle,
            lookup,
            sessions,
            forwarders,
        }
    }
}

/// Per-connection state: where this connection's subscriptions go.
#[derive(Debug)]
pub(crate) struct Connection {
    outbound: Outbound,
}

impl Connection {
    pub(crate) fn new(peer: Peer) -> Self {
        tracing::info!(client = %peer.client.name, version = %peer.client.version, "client connected");
        Self {
            outbound: peer.outbound,
        }
    }
}

fn wire(error: &ServiceError) -> WireError {
    error.to_wire()
}

impl Handler for NodeService {
    type Context = Connection;

    async fn node_info(&self, _: &Connection, _: GetNode) -> Result<NodeInfo, WireError> {
        Ok(self.lifecycle.info())
    }

    async fn list_harnesses(
        &self,
        _: &Connection,
        _: ListHarnesses,
    ) -> Result<Vec<Harness>, WireError> {
        Ok(vessel_harness::discover(&self.lookup).await)
    }

    async fn list_sessions(
        &self,
        _: &Connection,
        _: ListSessions,
    ) -> Result<Vec<SessionInfo>, WireError> {
        self.sessions.list().await.map_err(|error| wire(&error))
    }

    async fn get_session(
        &self,
        _: &Connection,
        request: GetSession,
    ) -> Result<SessionInfo, WireError> {
        self.sessions
            .get(request.session)
            .await
            .map_err(|error| wire(&error))
    }

    async fn create_session(
        &self,
        _: &Connection,
        request: CreateSession,
    ) -> Result<SessionInfo, WireError> {
        if self.lifecycle.is_draining() {
            return Err(wire(&ServiceError::Draining));
        }
        self.sessions
            .create(request.spec)
            .await
            .map_err(|error| wire(&error))
    }

    async fn send_input(&self, _: &Connection, request: SendInput) -> Result<Ack, WireError> {
        self.sessions
            .send(request.session, request.input)
            .await
            .map(|()| Ack)
            .map_err(|error| wire(&error))
    }

    async fn kill_session(&self, _: &Connection, request: KillSession) -> Result<Ack, WireError> {
        self.sessions
            .kill(request.session)
            .await
            .map(|()| Ack)
            .map_err(|error| wire(&error))
    }

    async fn read_events(
        &self,
        _: &Connection,
        request: ReadEvents,
    ) -> Result<EventPage, WireError> {
        self.sessions
            .read(request.session, request.from, request.limit)
            .await
            .map_err(|error| wire(&error))
    }

    async fn subscribe(
        &self,
        connection: &Connection,
        request: Subscribe,
    ) -> Result<Subscribed, WireError> {
        subscribe::open(
            &self.sessions,
            &connection.outbound,
            &self.forwarders,
            request,
        )
        .await
        .map_err(|error| wire(&error))
    }

    async fn unsubscribe(
        &self,
        connection: &Connection,
        request: Unsubscribe,
    ) -> Result<Ack, WireError> {
        connection
            .outbound
            .close_subscription(request.subscription)
            .await
            .map(|()| Ack)
            .map_err(|error| wire(&error.into()))
    }

    /// `node.stop` (protocol 3, ADR 0008): stop now, or drain and stop once
    /// idle. The `Ack` reaches the client even though `now` starts the
    /// shutdown inside this call: a closing connection still answers the
    /// calls already running (`vessel-rpc`, `server::calls`).
    async fn stop_node(&self, _: &Connection, request: StopNode) -> Result<Ack, WireError> {
        self.lifecycle.stop(request.when);
        Ok(Ack)
    }
}
