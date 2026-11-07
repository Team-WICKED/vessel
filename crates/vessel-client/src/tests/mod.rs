//! The client against a fake node: over the in-memory transport for stream
//! logic, and over a loopback websocket for the connection and the token.

mod calls;
mod memory;
mod pinned;
mod resume;
mod secure;
mod stream;
mod websocket;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::DateTime;
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    Ack, ClientInfo, CreateSession, DeviceInfo, ErrorCode, EventPage, EventRecord, EventSeq,
    GetNode, GetSession, Handler, Harness, HarnessKind, Invitation, InviteDevice, KillSession,
    ListDevices, ListHarnesses, ListSessions, NodeId, NodeInfo, NodeState, ProtocolRange,
    ReadEvents, RevokeDevice, SendInput, SessionEvent, SessionId, SessionInfo, SessionMode,
    SessionSpec, SessionStatus, StopNode, StopWhen, Subscribe, Subscribed, SubscriptionEnd,
    TerminalSize, Unsubscribe, WireError,
};
use vessel_proto::{
    BlobChunk, FleetMember, Generation, GetManifest, ListMembers, ListProfiles, Manifest,
    MoveProfile, ProfileInfo, ReadBlob, ReloadProfiles, ReportApplied, WatchFleet,
};
use vessel_rpc::memory::Disconnected;
use vessel_rpc::{ClientConfig, Outbound, OutboundError, Server, ServerConfig};

use crate::NodeClient;

/// Records in the fake session's log.
const LOG_LEN: u64 = 20;
/// The first subscription delivers this many records, then lags.
const LAG_AFTER: u64 = 5;
/// Records `session.read` can see; the rest are "not persisted yet", so
/// catching up must switch back to a subscription to get them.
const STORED: u64 = 12;
/// `session.read` pages are at most this long.
const PAGE: u64 = 4;

fn session_id() -> SessionId {
    "00000000-0000-4000-8000-000000000001".parse().unwrap()
}

fn record(seq: u64) -> EventRecord {
    EventRecord {
        seq: EventSeq::new(seq),
        at: DateTime::UNIX_EPOCH,
        event: SessionEvent::Status {
            status: SessionStatus::Running,
        },
    }
}

fn node() -> NodeInfo {
    NodeInfo {
        id: NodeId::random(),
        name: "fake".to_owned(),
        hostname: "fake".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        version: "0".to_owned(),
        protocol: ProtocolRange::CURRENT,
        started_at: DateTime::UNIX_EPOCH,
        state: NodeState::Serving,
        public_key: None,
    }
}

fn info() -> SessionInfo {
    {
        let mut info = SessionInfo::new(
            session_id(),
            SessionSpec::new(
                HarnessKind::OpenCode,
                SessionMode::Terminal {
                    args: Vec::new(),
                    size: TerminalSize::default(),
                },
            )
            .with_cwd(PathBuf::from("/work")),
            DateTime::UNIX_EPOCH,
        );
        info.status = SessionStatus::Running;
        info.next_seq = EventSeq::new(LOG_LEN);
        info
    }
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "vessel-client-tests".to_owned(),
        version: "0".to_owned(),
    }
}

/// A node with one session whose log is `oldest..LOG_LEN`.
struct FakeNode {
    /// The id new connections are welcomed with (change it to play a
    /// reinstalled machine).
    id: Mutex<NodeId>,
    /// Records below this were pruned.
    oldest: u64,
    subscribes: AtomicUsize,
    reads: AtomicUsize,
    /// `session.kill` calls received; each one hangs (never answers).
    kills: AtomicUsize,
    /// Set: the session is gone (`NotFound`), as after a wiped data dir.
    forgotten: AtomicBool,
    /// The last `node.stop`.
    stopped: Mutex<Option<StopWhen>>,
}

impl FakeNode {
    fn new(oldest: u64) -> Self {
        Self {
            id: Mutex::new(NodeId::random()),
            oldest,
            subscribes: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            kills: AtomicUsize::new(0),
            forgotten: AtomicBool::new(false),
            stopped: Mutex::new(None),
        }
    }
}

struct Context {
    outbound: Outbound,
}

fn unsupported() -> WireError {
    WireError::new(ErrorCode::Unsupported, "not in the fake")
}

/// Methods the fake does not serve.
macro_rules! unserved {
    ($( $handler:ident : $request:ty => $response:ty; )*) => {
        $(
            async fn $handler(
                &self,
                _context: &Context,
                _request: $request,
            ) -> Result<$response, WireError> {
                Err(unsupported())
            }
        )*
    };
}

impl Handler for FakeNode {
    type Context = Context;

    async fn node_info(&self, _: &Context, _: GetNode) -> Result<NodeInfo, WireError> {
        Ok(node())
    }

    async fn list_harnesses(
        &self,
        _: &Context,
        _: ListHarnesses,
    ) -> Result<Vec<Harness>, WireError> {
        Ok(Vec::new())
    }

    async fn list_sessions(
        &self,
        _: &Context,
        _: ListSessions,
    ) -> Result<Vec<SessionInfo>, WireError> {
        Ok(vec![info()])
    }

    async fn get_session(
        &self,
        _: &Context,
        request: GetSession,
    ) -> Result<SessionInfo, WireError> {
        if request.session == session_id() {
            Ok(info())
        } else {
            Err(WireError::new(ErrorCode::NotFound, "no such session"))
        }
    }

    async fn create_session(
        &self,
        _: &Context,
        _: CreateSession,
    ) -> Result<SessionInfo, WireError> {
        Err(unsupported())
    }

    async fn send_input(&self, _: &Context, _: SendInput) -> Result<Ack, WireError> {
        Err(unsupported())
    }

    async fn kill_session(&self, _: &Context, _: KillSession) -> Result<Ack, WireError> {
        let _kills = self.kills.fetch_add(1, Ordering::Relaxed);
        std::future::pending().await
    }

    async fn read_events(&self, _: &Context, request: ReadEvents) -> Result<EventPage, WireError> {
        let _reads = self.reads.fetch_add(1, Ordering::Relaxed);
        let start = request.from.get().max(self.oldest).min(STORED);
        let end = start.saturating_add(PAGE).min(STORED);
        Ok(EventPage {
            records: (start..end).map(record).collect(),
            next: EventSeq::new(end),
            at_end: end == STORED,
        })
    }

    async fn subscribe(
        &self,
        context: &Context,
        request: Subscribe,
    ) -> Result<Subscribed, WireError> {
        if request.session != session_id() || self.forgotten.load(Ordering::Relaxed) {
            return Err(WireError::new(ErrorCode::NotFound, "no such session"));
        }
        let first = self.subscribes.fetch_add(1, Ordering::Relaxed) == 0;
        let mut sink = context
            .outbound
            .open_subscription(request.from)
            .await
            .map_err(OutboundError::to_wire)?;
        let subscription = sink.id();
        let start = request.from.get().max(self.oldest);
        drop(tokio::spawn(async move {
            let (end, reason) = if first {
                let lag_at = start.saturating_add(LAG_AFTER);
                let resume_from = EventSeq::new(lag_at);
                (lag_at, SubscriptionEnd::Lagged { resume_from })
            } else {
                (LOG_LEN, SubscriptionEnd::SessionEnded)
            };
            for seq in start..end {
                if sink.send(Arc::new(record(seq))).await.is_err() {
                    return;
                }
            }
            sink.end(reason).await;
        }));
        Ok(Subscribed {
            subscription,
            info: info(),
        })
    }

    async fn unsubscribe(&self, context: &Context, request: Unsubscribe) -> Result<Ack, WireError> {
        context
            .outbound
            .close_subscription(request.subscription)
            .await
            .map_err(OutboundError::to_wire)?;
        Ok(Ack)
    }

    async fn stop_node(&self, _context: &Context, request: StopNode) -> Result<Ack, WireError> {
        *self.stopped.lock().unwrap() = Some(request.when);
        Ok(Ack)
    }

    async fn invite_device(
        &self,
        _context: &Context,
        _request: InviteDevice,
    ) -> Result<Invitation, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "no devices in this fake",
        ))
    }

    async fn list_devices(
        &self,
        _context: &Context,
        _request: ListDevices,
    ) -> Result<Vec<DeviceInfo>, WireError> {
        Ok(Vec::new())
    }

    async fn revoke_device(
        &self,
        _context: &Context,
        _request: RevokeDevice,
    ) -> Result<Ack, WireError> {
        Ok(Ack)
    }

    async fn list_profiles(
        &self,
        _context: &Context,
        _request: ListProfiles,
    ) -> Result<Vec<ProfileInfo>, WireError> {
        Err(unsupported())
    }

    async fn reload_profiles(
        &self,
        _context: &Context,
        _request: ReloadProfiles,
    ) -> Result<Vec<ProfileInfo>, WireError> {
        Err(unsupported())
    }

    async fn move_profile(
        &self,
        _context: &Context,
        _request: MoveProfile,
    ) -> Result<SessionInfo, WireError> {
        Err(unsupported())
    }

    async fn get_manifest(
        &self,
        _context: &Context,
        _request: GetManifest,
    ) -> Result<Manifest, WireError> {
        Err(unsupported())
    }

    async fn read_blob(
        &self,
        _context: &Context,
        _request: ReadBlob,
    ) -> Result<BlobChunk, WireError> {
        Err(unsupported())
    }

    async fn watch_fleet(
        &self,
        _context: &Context,
        _request: WatchFleet,
    ) -> Result<Generation, WireError> {
        Err(unsupported())
    }

    async fn report_applied(
        &self,
        _context: &Context,
        _request: ReportApplied,
    ) -> Result<Ack, WireError> {
        Err(unsupported())
    }

    async fn list_members(
        &self,
        _context: &Context,
        _request: ListMembers,
    ) -> Result<Vec<FleetMember>, WireError> {
        Err(unsupported())
    }

    unserved! {
        forward: vessel_proto::Forward => vessel_proto::Forwarded;
        relay: vessel_proto::RelayCall => vessel_proto::Forwarded;
        deliver: vessel_proto::Deliver => vessel_proto::Forwarded;
        grant_home: vessel_proto::GrantHome => Ack;
        get_callback: vessel_proto::GetCallback => vessel_proto::Callback;
        forget_node: vessel_proto::ForgetNode => vessel_proto::NodeForgotten;
        list_peers: vessel_proto::ListPeers => Vec<vessel_proto::FleetNode>;
        list_nodes: vessel_proto::ListNodes => Vec<vessel_proto::FleetNode>;
        export_session: vessel_proto::ExportSession => vessel_proto::TransferManifest;
        import_session: vessel_proto::ImportSession => vessel_proto::SessionImported;
        export_memory: vessel_proto::ExportMemory => vessel_proto::TransferManifest;
        import_memory: vessel_proto::ImportMemory => vessel_proto::MemoryImported;
        read_transfer: vessel_proto::ReadTransfer => BlobChunk;
        close_transfer: vessel_proto::CloseTransfer => Ack;
        set_secret: vessel_proto::SetSecret => Ack;
        list_secrets: vessel_proto::ListSecrets => Vec<vessel_proto::SecretName>;
        delete_secret: vessel_proto::DeleteSecret => Ack;
        copy_secret: vessel_proto::CopySecret => Ack;
        lsp_workspace_symbols: vessel_proto::lsp::WorkspaceSymbols => vessel_proto::lsp::Page<vessel_proto::lsp::Symbol>;
        lsp_document_symbols: vessel_proto::lsp::DocumentSymbols => vessel_proto::lsp::Page<vessel_proto::lsp::Symbol>;
        lsp_definition: vessel_proto::lsp::Definition => vessel_proto::lsp::Page<vessel_proto::lsp::Location>;
        lsp_references: vessel_proto::lsp::References => vessel_proto::lsp::Page<vessel_proto::lsp::Location>;
        lsp_hover: vessel_proto::lsp::GetHover => vessel_proto::lsp::Hover;
        lsp_implementations: vessel_proto::lsp::Implementations => vessel_proto::lsp::Page<vessel_proto::lsp::Location>;
        lsp_type_definition: vessel_proto::lsp::TypeDefinition => vessel_proto::lsp::Page<vessel_proto::lsp::Location>;
        lsp_incoming_calls: vessel_proto::lsp::IncomingCalls => vessel_proto::lsp::Page<vessel_proto::lsp::Call>;
        lsp_outgoing_calls: vessel_proto::lsp::OutgoingCalls => vessel_proto::lsp::Page<vessel_proto::lsp::Call>;
        lsp_diagnostics: vessel_proto::lsp::Diagnostics => vessel_proto::lsp::Page<vessel_proto::lsp::Diagnostic>;
        list_models: vessel_proto::ListModels => Vec<vessel_proto::ModelRoute>;
    }
}

fn server(fake: Arc<FakeNode>) -> Arc<Server<FakeNode>> {
    let mut info = node();
    info.id = *fake.id.lock().unwrap();
    Arc::new(Server::new(
        fake,
        tokio::sync::watch::channel(info).1,
        ServerConfig::default(),
        CancellationToken::new(),
    ))
}

/// A client connected to `node` over the in-memory transport.
async fn connect(node: Arc<FakeNode>) -> NodeClient {
    let (client, _serving) = open(node).await.unwrap();
    client
}

/// A connection to `node` over the in-memory transport, and the task
/// serving it: aborting the task drops the node's end, as a lost network
/// would.
async fn open(
    node: Arc<FakeNode>,
) -> Result<(NodeClient, tokio::task::JoinHandle<()>), vessel_rpc::ConnectError<Disconnected>> {
    let (client_end, server_end) = vessel_rpc::memory::pair(NonZeroUsize::new(16).unwrap());
    let server = server(node);
    let serving = tokio::spawn(async move {
        let _served = server
            .serve(server_end, |peer| Context {
                outbound: peer.outbound,
            })
            .await;
    });
    let client = NodeClient::over(client_end, client_info(), &ClientConfig::default()).await?;
    Ok((client, serving))
}
