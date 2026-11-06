//! [`NodeClient`]: one typed method per protocol call.

use vessel_proto::{
    BlobChunk, BlobHash, BundlePath, ClientInfo, CreateSession, DeleteSecret, DeviceId, DeviceInfo,
    EventPage, EventSeq, FleetMember, Generation, GetManifest, GetNode, GetSession, Harness,
    HarnessKind, Invitation, InviteDevice, KillSession, ListDevices, ListHarnesses, ListMembers,
    ListProfiles, ListSecrets, ListSessions, Manifest, MoveProfile, NodeInfo, ProfileInfo,
    ProfileName, ProtocolVersion, ReadBlob, ReadEvents, ReloadProfiles, ReportApplied,
    RevokeDevice, Scope, SecretName, SecretValue, SendInput, SessionId, SessionInfo, SessionInput,
    SessionSpec, SetSecret, StopNode, StopWhen, Subscribe, WatchFleet,
};
use vessel_rpc::{CallError, ClientConfig, Connection, FrameTransport, Subscription};

use crate::error::ConnectError;
use crate::stream::SessionStream;
use crate::token::BearerToken;
use crate::url::NodeUrl;
use crate::ws;

/// A connection to one node. Cheap to clone; clones share the connection,
/// which closes when the last clone (and every stream) is dropped.
#[derive(Debug, Clone)]
pub struct NodeClient {
    connection: Connection,
}

impl NodeClient {
    /// Connect to the node at `url` with its token, with default limits.
    ///
    /// # Errors
    /// The node is unreachable, refused the token, did not answer in time,
    /// or shares no protocol version with this client.
    pub async fn connect(
        url: &NodeUrl,
        token: &BearerToken,
        client: ClientInfo,
    ) -> Result<Self, ConnectError> {
        Self::connect_with(url, token, client, &ClientConfig::default()).await
    }

    /// [`connect`](Self::connect) with explicit limits. Opening the TCP
    /// connection and the websocket upgrade together get
    /// `config.handshake_timeout` (the OS's own connect timeout is about two
    /// minutes), and the protocol handshake gets it again.
    ///
    /// # Errors
    /// As [`connect`](Self::connect).
    pub async fn connect_with(
        url: &NodeUrl,
        token: &BearerToken,
        client: ClientInfo,
        config: &ClientConfig,
    ) -> Result<Self, ConnectError> {
        let transport = tokio::time::timeout(config.handshake_timeout, ws::connect(url, token))
            .await
            .map_err(|_elapsed| ConnectError::Timeout { url: url.clone() })??;
        let connection = Connection::connect(transport, client, config)
            .await
            .map_err(ConnectError::Handshake)?;
        tracing::debug!(%url, node = %connection.welcome().node.name, "connected");
        Ok(Self { connection })
    }

    /// Talk to a node over any frame transport (an in-process pair, later
    /// SSH stdio), with explicit limits.
    ///
    /// # Errors
    /// The handshake failed.
    pub async fn over<T: FrameTransport>(
        transport: T,
        client: ClientInfo,
        config: &ClientConfig,
    ) -> Result<Self, vessel_rpc::ConnectError<T::TransportError>> {
        let connection = Connection::connect(transport, client, config).await?;
        Ok(Self { connection })
    }

    /// The node, as described when the connection opened.
    #[must_use]
    pub fn node(&self) -> &NodeInfo {
        &self.connection.welcome().node
    }

    /// The protocol version in use.
    #[must_use]
    pub fn protocol(&self) -> ProtocolVersion {
        self.connection.welcome().protocol
    }

    /// Resolves once the connection has closed.
    pub async fn closed(&self) {
        self.connection.closed().await;
    }

    /// `node.info`.
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn node_info(&self) -> Result<NodeInfo, CallError> {
        self.connection.call(GetNode).await
    }

    /// `harness.list`: harnesses installed on the node.
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn harnesses(&self) -> Result<Vec<Harness>, CallError> {
        self.connection.call(ListHarnesses).await
    }

    /// `session.list`: every session, live and ended, newest first.
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, CallError> {
        self.connection.call(ListSessions).await
    }

    /// `session.get`.
    ///
    /// # Errors
    /// `NotFound` if there is no such session; see [`CallError`].
    pub async fn session(&self, session: SessionId) -> Result<SessionInfo, CallError> {
        self.connection.call(GetSession { session }).await
    }

    /// `session.create`: start a harness.
    ///
    /// # Errors
    /// The harness is not installed or could not start; see [`CallError`].
    pub async fn create_session(&self, spec: SessionSpec) -> Result<SessionInfo, CallError> {
        self.connection.call(CreateSession { spec }).await
    }

    /// `session.send`: terminal input, a prompt, a cancel, or a permission
    /// answer.
    ///
    /// # Errors
    /// `Unsupported` if the input does not match the session's mode,
    /// `Conflict` if the session has ended; see [`CallError`].
    pub async fn send(&self, session: SessionId, input: SessionInput) -> Result<(), CallError> {
        self.connection
            .call(SendInput { session, input })
            .await
            .map(drop)
    }

    /// `session.kill`: stop the session's process group.
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn kill(&self, session: SessionId) -> Result<(), CallError> {
        self.connection
            .call(KillSession { session })
            .await
            .map(drop)
    }

    /// `session.read`: one page of a session's log.
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn read(
        &self,
        session: SessionId,
        from: EventSeq,
        limit: u32,
    ) -> Result<EventPage, CallError> {
        self.connection
            .call(ReadEvents {
                session,
                from,
                limit,
                wait: None,
            })
            .await
    }

    /// `session.subscribe`, raw: the subscription ends with `Lagged` if this
    /// side falls behind. Most callers want [`follow`](Self::follow).
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn subscribe(
        &self,
        session: SessionId,
        from: EventSeq,
    ) -> Result<Subscription, CallError> {
        self.connection.subscribe(Subscribe { session, from }).await
    }

    /// `node.stop` (protocol 3): stop the node now (live sessions end
    /// `Interrupted`) or once no session is live (it refuses new ones
    /// meanwhile and reports `NodeState::Draining`). The node closes every
    /// connection when it stops, this one included.
    ///
    /// # Errors
    /// `Unsupported` if the connection negotiated protocol 2, without
    /// asking: such a node does not know the method, and protocol 2 clients
    /// never send it. Otherwise see [`CallError`].
    pub async fn stop_node(&self, when: StopWhen) -> Result<(), CallError> {
        // A protocol 2 connection is refused by `vessel-rpc` before anything
        // is sent (`CallError::NotInProtocol`, code `Unsupported`), as every
        // method newer than the connection is (`Method::since`).
        self.connection.call(StopNode { when }).await.map(drop)
    }

    /// `device.invite`: a single-use pairing link valid for `ttl_secs` (the
    /// node caps it at a day). Needs the `admin` scope.
    ///
    /// # Errors
    /// `Unsupported` on a node before protocol 4; `Unavailable` from a node
    /// without a key yet; otherwise see [`CallError`].
    pub async fn invite_device(
        &self,
        label: Option<String>,
        ttl_secs: u32,
    ) -> Result<Invitation, CallError> {
        self.connection
            .call(InviteDevice {
                label,
                ttl_secs,
                scopes: None,
            })
            .await
    }

    /// `device.list`: every paired device, revoked ones included.
    ///
    /// # Errors
    /// `Unsupported` on a node before protocol 4; otherwise see
    /// [`CallError`].
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, CallError> {
        self.connection.call(ListDevices).await
    }

    /// `device.revoke`: revoke a device; its open connections close.
    ///
    /// # Errors
    /// `NotFound` for an unknown device; `Unsupported` on a node before
    /// protocol 4; otherwise see [`CallError`].
    pub async fn revoke_device(&self, device: DeviceId) -> Result<(), CallError> {
        self.connection
            .call(RevokeDevice { device })
            .await
            .map(drop)
    }

    /// `profile.list`: the node's harness profiles (ADR 0011), `harness`'s
    /// only if given. Names, labels and variable names; never a value.
    ///
    /// # Errors
    /// `Unsupported` on a node before protocol 5; otherwise see
    /// [`CallError`].
    pub async fn profiles(
        &self,
        harness: Option<HarnessKind>,
    ) -> Result<Vec<ProfileInfo>, CallError> {
        self.connection.call(ListProfiles { harness }).await
    }

    /// `profile.reload`: the node reads `profiles.toml` again and answers
    /// with the profiles now in effect (needs the `admin` scope).
    ///
    /// # Errors
    /// `Conflict` when the file does not parse (the node keeps the profiles
    /// it had; the message says where); `Unsupported` on a node before
    /// protocol 5; otherwise see [`CallError`].
    pub async fn reload_profiles(&self) -> Result<Vec<ProfileInfo>, CallError> {
        self.connection.call(ReloadProfiles).await
    }

    /// `fleet.manifest` (ADR 0012): the home's current bundle. Needs the
    /// `sync` scope.
    ///
    /// # Errors
    /// `Unavailable` from a node that is not a fleet home or has no bundle
    /// yet; `Unsupported` on a node before protocol 5; otherwise see
    /// [`CallError`].
    pub async fn fleet_manifest(&self) -> Result<Manifest, CallError> {
        self.connection.call(GetManifest).await
    }

    /// `fleet.blob`: up to 1 MiB of blob `hash` from `offset`, and the
    /// blob's size. Needs the `sync` scope.
    ///
    /// # Errors
    /// `NotFound` for a blob the current bundle does not have (it may have
    /// changed: fetch the manifest again); `InvalidRequest` for an offset
    /// past the end; otherwise see [`CallError`].
    pub async fn fleet_blob(&self, hash: BlobHash, offset: u64) -> Result<BlobChunk, CallError> {
        self.connection.call(ReadBlob { hash, offset }).await
    }

    /// `fleet.watch`: wait for a generation other than `after`; the home
    /// answers with its current one when its wait limit (about 50 s)
    /// passes. Needs the `sync` scope.
    ///
    /// # Errors
    /// As [`fleet_manifest`](Self::fleet_manifest).
    pub async fn fleet_watch(&self, after: Generation) -> Result<Generation, CallError> {
        self.connection.call(WatchFleet { after }).await
    }

    /// `fleet.report`: this member applied `generation`, leaving
    /// `conflicts` as they are. Needs the `sync` scope, as a paired device.
    ///
    /// # Errors
    /// `InvalidRequest` from anything but a paired device; otherwise see
    /// [`CallError`].
    pub async fn fleet_report(
        &self,
        generation: Generation,
        conflicts: Vec<BundlePath>,
    ) -> Result<(), CallError> {
        self.connection
            .call(ReportApplied {
                generation,
                conflicts,
            })
            .await
            .map(drop)
    }

    /// `fleet.members`: the home's members and what each last reported.
    /// Needs the `admin` scope.
    ///
    /// # Errors
    /// `Unsupported` on a node before protocol 5; otherwise see
    /// [`CallError`].
    pub async fn fleet_members(&self) -> Result<Vec<FleetMember>, CallError> {
        self.connection.call(ListMembers).await
    }

    /// `secret.set` (protocol 6): store `value` as secret `name` on the
    /// node. Needs `admin`.
    ///
    /// # Errors
    /// `Conflict` when the node's `secrets.toml` cannot be used; otherwise
    /// see [`CallError`].
    pub async fn secret_set(&self, name: SecretName, value: SecretValue) -> Result<(), CallError> {
        self.connection
            .call(SetSecret { name, value })
            .await
            .map(drop)
    }

    /// `secret.list` (protocol 6): the names of the node's secrets, never
    /// their values. Needs `admin`.
    ///
    /// # Errors
    /// See [`CallError`].
    pub async fn secret_list(&self) -> Result<Vec<SecretName>, CallError> {
        self.connection.call(ListSecrets).await
    }

    /// `secret.delete` (protocol 6). Needs `admin`.
    ///
    /// # Errors
    /// `NotFound` when it is not set; otherwise see [`CallError`].
    pub async fn secret_delete(&self, name: SecretName) -> Result<(), CallError> {
        self.connection.call(DeleteSecret { name }).await.map(drop)
    }

    /// `device.invite` with explicit `scopes` (protocol 5): `[sync]` makes
    /// a link for a node joining this fleet's home.
    ///
    /// # Errors
    /// `CallError::NotInProtocol` on a node before protocol 5; otherwise
    /// as [`invite_device`](Self::invite_device).
    pub async fn invite_with_scopes(
        &self,
        label: Option<String>,
        ttl_secs: u32,
        scopes: Vec<Scope>,
    ) -> Result<Invitation, CallError> {
        self.connection
            .call(InviteDevice {
                label,
                ttl_secs,
                scopes: Some(scopes),
            })
            .await
    }

    /// `session.move_profile` (ADR 0011): the structured session `session`
    /// resumes its conversation under `profile` (`None`: the harness's
    /// default profile); a running turn is cancelled only with `force`.
    /// Answers with the session as it is after the move.
    ///
    /// # Errors
    /// `Conflict` mid-turn without `force`, when the target profile is not
    /// logged in, or when the session's files cannot be moved;
    /// `InvalidRequest` for an unknown profile or another harness's;
    /// `Unsupported` for a terminal session, or on a node before protocol
    /// 5; `SpawnFailed` when the agent did not start under the new profile
    /// (the session goes on under the old one). Otherwise see
    /// [`CallError`].
    pub async fn move_profile(
        &self,
        session: SessionId,
        profile: Option<ProfileName>,
        force: bool,
    ) -> Result<SessionInfo, CallError> {
        self.connection
            .call(MoveProfile {
                session,
                profile,
                force,
            })
            .await
    }

    /// Every record of a session from `from` on, gap-free, ending when the
    /// session ends.
    ///
    /// # Errors
    /// The first subscription failed (for example `NotFound`).
    pub async fn follow(
        &self,
        session: SessionId,
        from: EventSeq,
    ) -> Result<SessionStream, CallError> {
        let subscription = self.subscribe(session, from).await?;
        Ok(SessionStream::new(
            self.connection.clone(),
            from,
            subscription,
        ))
    }
}
