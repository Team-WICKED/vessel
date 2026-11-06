//! The protocol's `Handler`: routes each call to the module that owns it
//! and maps its typed error to the wire, once. No logic of its own. Every
//! transport serves it behind the scope check (`guard::Guarded`).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::task::TaskTracker;
use vessel_harness::Lookup;
use vessel_proto::{
    Ack, BlobChunk, CloseTransfer, CreateSession, Creator, DeleteSecret, DeviceInfo, ErrorCode,
    EventPage, ExportMemory, ExportSession, FleetMember, Generation, GetManifest, GetNode,
    GetSession, Harness, ImportMemory, ImportSession, Invitation, InviteDevice, KillSession,
    ListDevices, ListHarnesses, ListMembers, ListProfiles, ListSecrets, ListSessions, Manifest,
    MemoryImported, MoveProfile, NodeInfo, ProfileInfo, ReadBlob, ReadEvents, ReadTransfer,
    ReloadProfiles, ReportApplied, RevokeDevice, SecretName, SendInput, SessionId, SessionImported,
    SessionInfo, SessionOrigin, SessionSpec, SetSecret, StopNode, Subscribe, Subscribed,
    TransferManifest, Unsubscribe, WatchFleet, WireError,
};
use vessel_rpc::{Outbound, Peer};

use crate::agents::{Agents, MAX_DEPTH};
use crate::devices::Devices;
use crate::error::ServiceError;
use crate::fleet::{Home, Links};
use crate::lifecycle::Lifecycle;
use crate::principal::Principal;
use crate::profiles::launch::{Launcher, agent_session};
use crate::profiles::{NodeProfiles, moving};
use crate::sessions::Sessions;
use crate::subscribe;
use crate::transfers::export::Exporter;
use crate::transfers::import::{Importer, Local, Pull, Remote};
use crate::transfers::{Holder, Transfers};
use crate::waits::{ConnectionWaits, WaitSlot, Waiter, Waits};

/// The node's protocol implementation.
#[derive(Debug)]
pub(crate) struct NodeService {
    pub(crate) lifecycle: Lifecycle,
    lookup: Lookup,
    sessions: Arc<Sessions>,
    devices: Arc<Devices>,
    /// Subscription forwarders, so shutdown can let them queue their last
    /// frames.
    forwarders: TaskTracker,
    /// `profiles.toml` (ADR 0011).
    profiles: Arc<NodeProfiles>,
    /// What `session.create` adds to the spec.
    launcher: Launcher,
    /// Sessions' agents (ADR 0013): credentials, grants, limits.
    pub(crate) agents: Arc<Agents>,
    /// The fleet's home side (ADR 0012).
    fleet: Arc<Home>,
    /// The links to other nodes of the fleet (protocol 6, `envelope`).
    pub(crate) links: Arc<Links>,
    /// Open transfers (protocol 6).
    transfers: Arc<Transfers>,
    /// Waiting reads by origin (`waits`).
    waits: Arc<Waits>,
}

/// The parts a [`NodeService`] serves.
pub(crate) struct Parts {
    pub(crate) lifecycle: Lifecycle,
    pub(crate) lookup: Lookup,
    pub(crate) sessions: Arc<Sessions>,
    pub(crate) devices: Arc<Devices>,
    pub(crate) forwarders: TaskTracker,
    pub(crate) profiles: Arc<NodeProfiles>,
    pub(crate) launcher: Launcher,
    pub(crate) agents: Arc<Agents>,
    pub(crate) fleet: Arc<Home>,
    pub(crate) links: Arc<Links>,
    pub(crate) transfers: Arc<Transfers>,
}

impl NodeService {
    pub(crate) fn new(parts: Parts) -> Self {
        let Parts {
            lifecycle,
            lookup,
            sessions,
            devices,
            forwarders,
            profiles,
            launcher,
            agents,
            fleet,
            links,
            transfers,
        } = parts;
        Self {
            lifecycle,
            lookup,
            sessions,
            devices,
            forwarders,
            profiles,
            launcher,
            agents,
            fleet,
            links,
            transfers,
            waits: Arc::default(),
        }
    }

    /// A place for the wait of a read `waiter` makes on `connection`, if it
    /// waits and both shares have room (`waits`); otherwise the wait is
    /// dropped and the read answers at once.
    pub(crate) fn wait_slot(
        &self,
        connection: &Connection,
        waiter: Waiter,
        wait: &mut Option<u32>,
    ) -> Option<WaitSlot> {
        let _seconds = wait.as_ref()?;
        let slot = self.waits.take(&connection.waiting, waiter);
        if slot.is_none() {
            *wait = None;
            tracing::debug!(
                ?waiter,
                "a waiting read answers at once: its connection's or its origin's share of waits \
                 is taken"
            );
        }
        slot
    }
}

/// Per-connection state: who is calling, and where this connection's
/// subscriptions go.
#[derive(Debug)]
pub(crate) struct Connection {
    outbound: Outbound,
    principal: Principal,
    /// Its waiting reads, against its share (`waits`).
    waiting: ConnectionWaits,
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
            waiting: ConnectionWaits::default(),
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

    /// This connection, calling as `principal`: a call delivered from
    /// another node runs as its delegated principal (protocol 6).
    pub(crate) fn acting_as(&self, principal: Principal) -> Self {
        Self {
            outbound: self.outbound.clone(),
            principal,
            waiting: Arc::clone(&self.waiting),
        }
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

    /// `session.create`: the spec's profile resolved, the caller recorded
    /// as its creator, and the session's agent given its credential within
    /// the caller's grant and the agent limits (ADRs 0011, 0013).
    pub(crate) async fn create_session(
        &self,
        connection: &Connection,
        request: CreateSession,
    ) -> Result<SessionInfo, WireError> {
        self.create(connection, request.spec, None).await
    }

    /// A session of `spec` for `connection`'s caller: `session.create`'s
    /// path, also an import's (`origin` says where it came from).
    async fn create(
        &self,
        connection: &Connection,
        spec: SessionSpec,
        origin: Option<SessionOrigin>,
    ) -> Result<SessionInfo, WireError> {
        if self.lifecycle.is_draining() {
            return Err(wire(&ServiceError::Draining));
        }
        let mut options = self
            .launcher
            .options(&self.profiles, &spec, &connection.principal)
            .map_err(|error| wire(&error))?;
        // An import says where it came from; otherwise the launcher's
        // (a call delivered from another node records `remote`).
        if origin.is_some() {
            options.origin = origin;
        }
        let id = SessionId::random();
        let secret = self
            .agents
            .reserve(id, &connection.principal, self.launcher.has_tools())
            .map_err(|error| wire(&error))?;
        options.id = Some(id);
        if let Some(secret) = &secret
            && let Err(error) = self
                .launcher
                .attach_tools(&mut options.mcp_servers, id, secret)
        {
            self.agents.forget(id);
            return Err(wire(&error));
        }
        let created = self.sessions.create(spec, options).await;
        if created.is_err() {
            self.agents.forget(id);
        }
        created.map_err(|error| wire(&error))
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
        connection: &Connection,
        mut request: ReadEvents,
    ) -> Result<EventPage, WireError> {
        let waiter = Waiter::of(connection.principal(), self.links.node());
        let _slot = self.wait_slot(connection, waiter, &mut request.wait);
        let read = match request.wait {
            Some(seconds) => {
                let wait = Duration::from_secs(seconds.min(ReadEvents::MAX_WAIT).into());
                self.sessions
                    .read_waiting(request.session, request.from, request.limit, wait)
                    .await
            }
            None => {
                self.sessions
                    .read(request.session, request.from, request.limit)
                    .await
            }
        };
        read.map_err(|error| wire(&error))
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

/// Profiles (ADR 0011, Track H).
impl NodeService {
    /// `profile.list`: names, labels, harnesses and variable names; never a
    /// value.
    #[expect(
        clippy::unused_async,
        reason = "every call has the same async shape, so the guard's rows are uniform"
    )]
    pub(crate) async fn list_profiles(
        &self,
        _: &Connection,
        request: ListProfiles,
    ) -> Result<Vec<ProfileInfo>, WireError> {
        Ok(self.profiles.list(request.harness))
    }

    /// `profile.reload`: read `profiles.toml` again; a file that cannot be
    /// used leaves the profiles in effect and says where it is wrong.
    pub(crate) async fn reload_profiles(
        &self,
        _: &Connection,
        _: ReloadProfiles,
    ) -> Result<Vec<ProfileInfo>, WireError> {
        let profiles = Arc::clone(&self.profiles);
        tokio::task::spawn_blocking(move || profiles.reload())
            .await
            .map_err(|error| wire(&ServiceError::Task(error)))?
            .map_err(|error| wire(&ServiceError::Profiles(error)))
    }

    /// `session.move_profile` (H2): a live structured session resumes its
    /// conversation under another profile (`profiles::moving`, then the
    /// session runtime).
    pub(crate) async fn move_profile(
        &self,
        _: &Connection,
        request: MoveProfile,
    ) -> Result<SessionInfo, WireError> {
        let id = request.session;
        let profiles = &self.profiles;
        let launcher = &self.launcher;
        let lookup = &self.lookup;
        let agents = &self.agents;
        let moved = self
            .sessions
            .move_profile(id, async |info: &SessionInfo| {
                let mut plan = moving::plan(profiles, launcher, lookup, info, &request).await?;
                // The restarted agent's `vessel mcp` gets a credential of
                // its own (`agents`, Decision Log).
                if let Some(secret) = agents.begin_move(id)? {
                    launcher.attach_tools(&mut plan.mcp_servers, id, &secret)?;
                }
                Ok(plan)
            })
            .await;
        self.agents.end_move(id, moved.is_ok());
        moved.map_err(|error| wire(&error))
    }

    /// For an agent, an `operate` call on `session` needs the session to
    /// have been started by the agent's session or a session it started,
    /// and so on (ADR 0013); anyone else passes. The agent may be another
    /// node's whose call was delivered here (protocol 6): the chain is
    /// followed through this node's sessions until it reaches the agent's
    /// `(node, session)`.
    pub(crate) async fn check_owner(
        &self,
        principal: &Principal,
        session: SessionId,
    ) -> Result<(), WireError> {
        let Some(agent) = principal.agent() else {
            return Ok(());
        };
        let here = self.lifecycle.info().id;
        let mut current = session;
        // A chain longer than the depth limit cannot lead to the agent.
        for _step in 0..=MAX_DEPTH {
            let info = self
                .sessions
                .get(current)
                .await
                .map_err(|error| wire(&error))?;
            let Creator::Agent { node, session } = info.created_by else {
                break;
            };
            if (node, session) == agent {
                return Ok(());
            }
            // Only this node's sessions can be followed further.
            match agent_session(&info.created_by, here) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Err(wire(&ServiceError::NotOwned(session)))
    }
}

/// The fleet (ADR 0012, protocol 5), served by the home's side of
/// `fleet`; a node that is not a home answers `unavailable`.
impl NodeService {
    #[expect(
        clippy::unused_async,
        reason = "every call has the same async shape, so the guard's rows are uniform"
    )]
    pub(crate) async fn get_manifest(
        &self,
        _: &Connection,
        _: GetManifest,
    ) -> Result<Manifest, WireError> {
        self.fleet.manifest()
    }

    pub(crate) async fn read_blob(
        &self,
        _: &Connection,
        request: ReadBlob,
    ) -> Result<BlobChunk, WireError> {
        self.fleet.blob(request.hash, request.offset).await
    }

    pub(crate) async fn watch_fleet(
        &self,
        _: &Connection,
        request: WatchFleet,
    ) -> Result<Generation, WireError> {
        self.fleet.watch(request.after).await
    }

    /// `fleet.report`: only a paired device (a member) reports; its device
    /// id is whose report it is.
    pub(crate) async fn report_applied(
        &self,
        connection: &Connection,
        request: ReportApplied,
    ) -> Result<Ack, WireError> {
        let Principal::Device { id, .. } = connection.principal() else {
            return Err(WireError::new(
                ErrorCode::InvalidRequest,
                "only a member node reports to its fleet's home".to_owned(),
            ));
        };
        self.fleet.report(*id, request).await.map(|()| Ack)
    }

    pub(crate) async fn list_members(
        &self,
        _: &Connection,
        _: ListMembers,
    ) -> Result<Vec<FleetMember>, WireError> {
        let devices = self.devices.list().await.map_err(|error| wire(&error))?;
        Ok(self.fleet.members(devices).await)
    }
}

/// This node's secrets (protocol 6, F3): the store `vessel fleet secret`
/// uses (`fleet::secrets`), on the wire. All admin. A value only ever goes
/// in; there is no call that reads one back.
impl NodeService {
    pub(crate) async fn set_secret(
        &self,
        _: &Connection,
        request: SetSecret,
    ) -> Result<Ack, WireError> {
        let files = self.fleet.files().clone();
        tokio::task::spawn_blocking(move || {
            crate::fleet::set_secret(&files, &request.name, request.value.expose())
        })
        .await
        .map_err(|error| wire(&ServiceError::Task(error)))?
        .map(|()| Ack)
        .map_err(|error| secrets_wire(&error))
    }

    pub(crate) async fn list_secrets(
        &self,
        _: &Connection,
        _: ListSecrets,
    ) -> Result<Vec<SecretName>, WireError> {
        let files = self.fleet.files().clone();
        tokio::task::spawn_blocking(move || crate::fleet::secret_names(&files))
            .await
            .map_err(|error| wire(&ServiceError::Task(error)))?
            .map_err(|error| secrets_wire(&error))
    }

    pub(crate) async fn delete_secret(
        &self,
        _: &Connection,
        request: DeleteSecret,
    ) -> Result<Ack, WireError> {
        let files = self.fleet.files().clone();
        let name = request.name.clone();
        let deleted =
            tokio::task::spawn_blocking(move || crate::fleet::delete_secret(&files, &name))
                .await
                .map_err(|error| wire(&ServiceError::Task(error)))?
                .map_err(|error| secrets_wire(&error))?;
        if deleted {
            Ok(Ack)
        } else {
            Err(WireError::new(
                ErrorCode::NotFound,
                format!("secret {} is not set on this node", request.name),
            ))
        }
    }
}

/// A `secrets.toml` problem on the wire; the message names no value.
fn secrets_wire(error: &crate::fleet::SecretsError) -> WireError {
    let code = match error {
        crate::fleet::SecretsError::File(_) => ErrorCode::Internal,
        crate::fleet::SecretsError::Parse { .. } | crate::fleet::SecretsError::Entry { .. } => {
            ErrorCode::Conflict
        }
    };
    WireError::new(code, error.to_string())
}

/// Transfers (protocol 6, F4): `transfers` exports and serves them, and
/// pulls imports from a source: this node, or another fleet node through
/// the envelope (`pass_on`).
impl NodeService {
    fn exporter(&self) -> Exporter<'_> {
        Exporter {
            sessions: &self.sessions,
            profiles: &self.profiles,
            transfers: &self.transfers,
        }
    }

    fn importer(&self) -> Importer<'_> {
        Importer {
            profiles: &self.profiles,
            transfers: &self.transfers,
            node: self.lifecycle.info().id,
        }
    }

    /// Where an import from `node` pulls, for `connection`'s caller: this
    /// node (an agent only copies a session it started, as for
    /// `session.export`), or another fleet node through the envelope, where
    /// the source applies its own ownership rules to the caller's origin.
    async fn source<'a>(
        &'a self,
        connection: &'a Connection,
        node: vessel_proto::NodeId,
        session: Option<SessionId>,
    ) -> Result<Pull<'a>, WireError> {
        if node != self.lifecycle.info().id {
            return Ok(Pull::Remote(Remote {
                service: self,
                principal: connection.principal(),
                node,
            }));
        }
        if let Some(session) = session {
            self.check_owner(connection.principal(), session).await?;
        }
        Ok(Pull::Local(Local {
            exporter: self.exporter(),
            holder: Holder::of(connection.principal()),
        }))
    }

    /// `session.export` (ownership checked by the guard).
    pub(crate) async fn export_session(
        &self,
        connection: &Connection,
        request: ExportSession,
    ) -> Result<TransferManifest, WireError> {
        self.exporter()
            .session(Holder::of(connection.principal()), request)
            .await
            .map_err(|error| wire(&error))
    }

    /// `memory.export`.
    pub(crate) async fn export_memory(
        &self,
        connection: &Connection,
        request: ExportMemory,
    ) -> Result<TransferManifest, WireError> {
        self.exporter()
            .memory(Holder::of(connection.principal()), request)
            .await
            .map_err(|error| wire(&error))
    }

    /// `transfer.read`: only for whoever opened it.
    pub(crate) async fn read_transfer(
        &self,
        connection: &Connection,
        request: ReadTransfer,
    ) -> Result<BlobChunk, WireError> {
        self.transfers
            .read(Holder::of(connection.principal()), request)
            .await
    }

    /// `transfer.close`: only for whoever opened it.
    pub(crate) async fn close_transfer(
        &self,
        connection: &Connection,
        request: CloseTransfer,
    ) -> Result<Ack, WireError> {
        self.transfers
            .close(Holder::of(connection.principal()), request.transfer)
            .await
            .map(|()| Ack)
            .map_err(|error| crate::transfers::wire(&error))
    }

    /// `session.import`: pulled and placed, then resumed if asked, as a
    /// session that records where it came from.
    pub(crate) async fn import_session(
        &self,
        connection: &Connection,
        request: ImportSession,
    ) -> Result<SessionImported, WireError> {
        if request.start && self.lifecycle.is_draining() {
            return Err(wire(&ServiceError::Draining));
        }
        let source = self
            .source(connection, request.from.node, Some(request.from.session))
            .await?;
        let placed = self.importer().session(&source, &request).await?;
        let mut imported = placed.imported;
        if request.start {
            let info = self
                .create(connection, placed.spec, Some(placed.origin))
                .await?;
            imported.session = Some(Box::new(info));
        }
        Ok(imported)
    }

    /// `memory.import`.
    pub(crate) async fn import_memory(
        &self,
        connection: &Connection,
        request: ImportMemory,
    ) -> Result<MemoryImported, WireError> {
        let source = self.source(connection, request.from.node, None).await?;
        self.importer().memory(&source, &request).await
    }
}
