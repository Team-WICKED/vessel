//! [`NodeClient`]: one typed method per protocol call.

use vessel_proto::{
    ClientInfo, CreateSession, EventPage, EventSeq, GetNode, GetSession, Harness, KillSession,
    ListHarnesses, ListSessions, NodeInfo, ProtocolVersion, ReadEvents, SendInput, SessionId,
    SessionInfo, SessionInput, SessionSpec, Subscribe,
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
    /// Connect to the node at `url` with its token.
    ///
    /// # Errors
    /// The node is unreachable, refused the token, or shares no protocol
    /// version with this client.
    pub async fn connect(
        url: &NodeUrl,
        token: &BearerToken,
        client: ClientInfo,
    ) -> Result<Self, ConnectError> {
        let transport = ws::connect(url, token).await?;
        let connection = Connection::connect(transport, client, &ClientConfig::default())
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
