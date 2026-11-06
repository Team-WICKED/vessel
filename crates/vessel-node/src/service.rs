//! The protocol's `Handler`: routes each call to the module that owns it
//! and maps its typed error to the wire, once. No logic of its own. Every
//! transport serves it behind the scope check (`guard::Guarded`).

use std::sync::Arc;

use tokio_util::task::TaskTracker;
use vessel_harness::Lookup;
use vessel_proto::{
    Ack, CreateSession, DeviceInfo, EventPage, GetNode, GetSession, Harness, Invitation,
    InviteDevice, KillSession, ListDevices, ListHarnesses, ListSessions, NodeInfo, ReadEvents,
    RevokeDevice, SendInput, SessionInfo, StopNode, Subscribe, Subscribed, Unsubscribe, WireError,
};
use vessel_rpc::{Outbound, Peer};

use crate::devices::Devices;
use crate::error::ServiceError;
use crate::lifecycle::Lifecycle;
use crate::principal::Principal;
use crate::sessions::Sessions;
use crate::subscribe;

/// The node's protocol implementation.
#[derive(Debug)]
pub(crate) struct NodeService {
    lifecycle: Lifecycle,
    lookup: Lookup,
    sessions: Arc<Sessions>,
    devices: Arc<Devices>,
    /// Subscription forwarders, so shutdown can let them queue their last
    /// frames.
    forwarders: TaskTracker,
}

impl NodeService {
    pub(crate) const fn new(
        lifecycle: Lifecycle,
        lookup: Lookup,
        sessions: Arc<Sessions>,
        devices: Arc<Devices>,
        forwarders: TaskTracker,
    ) -> Self {
        Self {
            lifecycle,
            lookup,
            sessions,
            devices,
            forwarders,
        }
    }
}

/// Per-connection state: who is calling, and where this connection's
/// subscriptions go.
#[derive(Debug)]
pub(crate) struct Connection {
    outbound: Outbound,
    principal: Principal,
}

impl Connection {
    pub(crate) fn new(peer: Peer, principal: Principal) -> Self {
        tracing::info!(
            client = %peer.client.name,
            version = %peer.client.version,
            ?principal,
            "client connected"
        );
        Self {
            outbound: peer.outbound,
            principal,
        }
    }

    /// A connection of the machine's own clients (token or local socket).
    pub(crate) fn owner(peer: Peer) -> Self {
        Self::new(peer, Principal::Owner)
    }

    /// Who is calling.
    pub(crate) const fn principal(&self) -> &Principal {
        &self.principal
    }
}

fn wire(error: &ServiceError) -> WireError {
    error.to_wire()
}

/// The calls, one per protocol method. Inherent, not a `Handler`: only
/// [`Guarded`](crate::guard::Guarded) implements `Handler`, so the service
/// cannot be served without the scope check.
impl NodeService {
    #[expect(
        clippy::unused_async,
        reason = "every call has the same async shape, so the guard's rows are uniform"
    )]
    pub(crate) async fn node_info(
        &self,
        _: &Connection,
        _: GetNode,
    ) -> Result<NodeInfo, WireError> {
        Ok(self.lifecycle.info())
    }

    pub(crate) async fn list_harnesses(
        &self,
        _: &Connection,
        _: ListHarnesses,
    ) -> Result<Vec<Harness>, WireError> {
        Ok(vessel_harness::discover(&self.lookup).await)
    }

    pub(crate) async fn list_sessions(
        &self,
        _: &Connection,
        _: ListSessions,
    ) -> Result<Vec<SessionInfo>, WireError> {
        self.sessions.list().await.map_err(|error| wire(&error))
    }

    pub(crate) async fn get_session(
        &self,
        _: &Connection,
        request: GetSession,
    ) -> Result<SessionInfo, WireError> {
        self.sessions
            .get(request.session)
            .await
            .map_err(|error| wire(&error))
    }

    pub(crate) async fn create_session(
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

    pub(crate) async fn send_input(
        &self,
        _: &Connection,
        request: SendInput,
    ) -> Result<Ack, WireError> {
        self.sessions
            .send(request.session, request.input)
            .await
            .map(|()| Ack)
            .map_err(|error| wire(&error))
    }

    pub(crate) async fn kill_session(
        &self,
        _: &Connection,
        request: KillSession,
    ) -> Result<Ack, WireError> {
        self.sessions
            .kill(request.session)
            .await
            .map(|()| Ack)
            .map_err(|error| wire(&error))
    }

    pub(crate) async fn read_events(
        &self,
        _: &Connection,
        request: ReadEvents,
    ) -> Result<EventPage, WireError> {
        self.sessions
            .read(request.session, request.from, request.limit)
            .await
            .map_err(|error| wire(&error))
    }

    pub(crate) async fn subscribe(
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

    pub(crate) async fn unsubscribe(
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
    #[expect(
        clippy::unused_async,
        reason = "every call has the same async shape, so the guard's rows are uniform"
    )]
    pub(crate) async fn stop_node(
        &self,
        _: &Connection,
        request: StopNode,
    ) -> Result<Ack, WireError> {
        self.lifecycle.stop(request.when);
        Ok(Ack)
    }

    /// `device.invite` (ADR 0010): a single-use pairing link.
    #[expect(
        clippy::unused_async,
        reason = "every call has the same async shape, so the guard's rows are uniform"
    )]
    pub(crate) async fn invite_device(
        &self,
        _: &Connection,
        request: InviteDevice,
    ) -> Result<Invitation, WireError> {
        self.devices.invite(&request).map_err(|error| wire(&error))
    }

    pub(crate) async fn list_devices(
        &self,
        _: &Connection,
        _: ListDevices,
    ) -> Result<Vec<DeviceInfo>, WireError> {
        self.devices.list().await.map_err(|error| wire(&error))
    }

    /// `device.revoke`: also closes the device's open connections, this
    /// one included if a device revokes itself (after its `ack`).
    pub(crate) async fn revoke_device(
        &self,
        _: &Connection,
        request: RevokeDevice,
    ) -> Result<Ack, WireError> {
        self.devices
            .revoke(request.device)
            .await
            .map(|()| Ack)
            .map_err(|error| wire(&error))
    }
}
