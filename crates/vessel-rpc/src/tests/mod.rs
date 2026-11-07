//! Client and server against each other over the in-memory transport.
//!
//! The fake handler answers every method with data derived from the request
//! and reports each call it saw, so a test checks both directions of every
//! method. Waiting is always on an event (`Notify`, a channel, a reply),
//! never on the clock.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::DateTime;
use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    Ack, Call, ClientFrame, ClientInfo, CreateSession, DeviceInfo, ErrorCode, EventPage,
    EventRecord, EventSeq, GetNode, GetSession, Handler, Harness, HarnessKind, Invitation,
    InviteDevice, KillSession, ListDevices, ListHarnesses, ListSessions, NodeId, NodeInfo,
    NodeState, Outcome, ProtocolRange, ProtocolVersion, ReadEvents, Request, RequestId,
    RevokeDevice, SendInput, ServerFrame, SessionEvent, SessionId, SessionInfo, SessionInput,
    SessionMode, SessionSpec, SessionStatus, StopNode, StopWhen, StructuredSupport, Subscribe,
    Subscribed, SubscriptionEnd, TerminalBytes, TerminalInput, TerminalSize, Unsubscribe,
    WireError,
};
use vessel_proto::{
    BlobChunk, FleetMember, Generation, GetManifest, ListMembers, ListProfiles, Manifest,
    MoveProfile, ProfileInfo, ReadBlob, ReloadProfiles, ReportApplied, WatchFleet,
};

use crate::memory::{self, Disconnected, MemoryTransport};
use crate::{
    CallError, ClientConfig, ConnectError, Connection, Outbound, ServeError, Server, ServerConfig,
    SubscriptionItem,
};

/// Records each subscription streams before ending.
const RECORDS: u64 = 5;

fn session(n: u128) -> SessionId {
    format!("00000000-0000-4000-8000-{n:012}").parse().unwrap()
}

/// A session whose subscription never ends on its own.
fn endless() -> SessionId {
    session(99)
}

/// A session whose forwarder sends two records and then drops its sink
/// without ending the subscription.
fn abandoned() -> SessionId {
    session(98)
}

fn node() -> NodeInfo {
    NodeInfo {
        id: NodeId::random(),
        name: "test-node".to_owned(),
        hostname: "test-host".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        version: "0.0.0".to_owned(),
        protocol: ProtocolRange::CURRENT,
        started_at: DateTime::UNIX_EPOCH,
        state: NodeState::Serving,
        public_key: None,
    }
}

fn spec() -> SessionSpec {
    SessionSpec::new(
        HarnessKind::Codex,
        SessionMode::Terminal {
            args: vec!["--help".to_owned()],
            size: TerminalSize::default(),
        },
    )
    .with_cwd(PathBuf::from("/work"))
}

fn info(id: SessionId, spec: SessionSpec) -> SessionInfo {
    let mut info = SessionInfo::new(id, spec, DateTime::UNIX_EPOCH);
    info.status = SessionStatus::Running;
    info.next_seq = EventSeq::new(RECORDS);
    info
}

/// A session whose log holds a protocol 5 event (`ProfileChanged`), for
/// checking what older connections receive instead.
fn moved() -> SessionId {
    session(97)
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

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "vessel-rpc-tests".to_owned(),
        version: "0".to_owned(),
    }
}

/// Signals from inside the handler.
struct Probe {
    /// A blocking call started.
    started: Notify,
    /// A blocking call's future was dropped (aborted).
    aborted: Notify,
    /// An endless subscription saw its sink cancelled.
    unsubscribed: Notify,
    /// One per finite subscription that queued all its frames.
    forwarded: mpsc::Sender<()>,
}

struct Fake {
    probe: Arc<Probe>,
    seen: mpsc::Sender<Call>,
    /// The server's shutdown, which `node.stop{now}` cancels as a node does.
    shutdown: CancellationToken,
}

struct Context {
    outbound: Outbound,
}

impl Fake {
    fn saw(&self, call: Call) {
        self.seen.try_send(call).unwrap();
    }
}

/// Notifies `aborted` when dropped, to observe that a call's task was
/// aborted.
struct AbortSignal(Arc<Probe>);

impl Drop for AbortSignal {
    fn drop(&mut self) {
        self.0.aborted.notify_one();
    }
}

/// Methods this fake does not serve: each records the call and refuses it.
macro_rules! unserved {
    ($( $handler:ident : $request:ty => $response:ty; )*) => {
        $(
            async fn $handler(
                &self,
                _context: &Context,
                request: $request,
            ) -> Result<$response, WireError> {
                self.unsupported(request)
            }
        )*
    };
}

impl Handler for Fake {
    type Context = Context;

    async fn node_info(&self, _: &Context, request: GetNode) -> Result<NodeInfo, WireError> {
        self.saw(request.into_call());
        Ok(node())
    }

    async fn list_harnesses(
        &self,
        _: &Context,
        request: ListHarnesses,
    ) -> Result<Vec<Harness>, WireError> {
        self.saw(request.into_call());
        Ok(vec![Harness {
            kind: HarnessKind::Gemini,
            path: PathBuf::from("/bin/gemini"),
            version: Some("1.0".to_owned()),
            structured: StructuredSupport::Unavailable {
                reason: "adapter missing".to_owned(),
            },
        }])
    }

    async fn list_sessions(
        &self,
        _: &Context,
        request: ListSessions,
    ) -> Result<Vec<SessionInfo>, WireError> {
        self.saw(request.into_call());
        Ok(vec![info(session(1), spec())])
    }

    async fn get_session(
        &self,
        _: &Context,
        request: GetSession,
    ) -> Result<SessionInfo, WireError> {
        self.saw(request.into_call());
        if request.session == session(1) {
            Ok(info(session(1), spec()))
        } else {
            Err(WireError::new(ErrorCode::NotFound, "no such session"))
        }
    }

    async fn create_session(
        &self,
        _: &Context,
        request: CreateSession,
    ) -> Result<SessionInfo, WireError> {
        self.saw(request.clone().into_call());
        Ok(info(session(2), request.spec))
    }

    async fn send_input(&self, _: &Context, request: SendInput) -> Result<Ack, WireError> {
        self.saw(request.into_call());
        Ok(Ack)
    }

    /// Never answers; used to test cancellation and disconnects.
    async fn kill_session(&self, _: &Context, request: KillSession) -> Result<Ack, WireError> {
        self.saw(request.into_call());
        let _signal = AbortSignal(Arc::clone(&self.probe));
        self.probe.started.notify_one();
        std::future::pending().await
    }

    async fn read_events(&self, _: &Context, request: ReadEvents) -> Result<EventPage, WireError> {
        let session = request.session;
        self.saw(request.into_call());
        let next = request.from.next().unwrap();
        let mut record = record(request.from.get());
        if session == moved() {
            record.event = SessionEvent::ProfileChanged {
                from: None,
                to: Some("alt".parse().unwrap()),
            };
        }
        Ok(EventPage {
            records: vec![record],
            next,
            at_end: next.get() >= u64::from(request.limit),
        })
    }

    async fn subscribe(
        &self,
        context: &Context,
        request: Subscribe,
    ) -> Result<Subscribed, WireError> {
        self.saw(request.into_call());
        let mut sink = context
            .outbound
            .open_subscription(request.from)
            .await
            .map_err(crate::OutboundError::to_wire)?;
        let subscription = sink.id();
        let probe = Arc::clone(&self.probe);
        // The forwarder starts before this call returns, so without the
        // announcement gate its events would race the reply.
        drop(tokio::spawn(async move {
            if request.session == endless() {
                sink.cancelled().await;
                probe.unsubscribed.notify_one();
                return;
            }
            if request.session == abandoned() {
                for seq in request.from.get()..request.from.get().saturating_add(2) {
                    if sink.send(Arc::new(record(seq))).await.is_err() {
                        return;
                    }
                }
                drop(sink);
                return;
            }
            for seq in request.from.get()..RECORDS {
                if sink.send(Arc::new(record(seq))).await.is_err() {
                    return;
                }
            }
            sink.end(SubscriptionEnd::SessionEnded).await;
            probe.forwarded.try_send(()).unwrap();
        }));
        // Let the forwarder run first, so it tries to queue its records
        // before this reply is queued: the race the gate exists for.
        tokio::task::yield_now().await;
        Ok(Subscribed {
            subscription,
            info: info(request.session, spec()),
        })
    }

    async fn unsubscribe(&self, context: &Context, request: Unsubscribe) -> Result<Ack, WireError> {
        self.saw(request.into_call());
        context
            .outbound
            .close_subscription(request.subscription)
            .await
            .map_err(crate::OutboundError::to_wire)?;
        Ok(Ack)
    }

    async fn stop_node(&self, _context: &Context, request: StopNode) -> Result<Ack, WireError> {
        let when = request.when;
        self.saw(request.into_call());
        if when == StopWhen::Now {
            // As the node does, the shutdown starts before this call
            // returns. Yielding lets the connection notice it first: the
            // race the answer has to survive.
            self.shutdown.cancel();
            tokio::task::yield_now().await;
        }
        Ok(Ack)
    }

    async fn invite_device(
        &self,
        _context: &Context,
        request: InviteDevice,
    ) -> Result<Invitation, WireError> {
        self.saw(request.into_call());
        Err(WireError::new(
            ErrorCode::Unsupported,
            "no devices in this fake",
        ))
    }

    async fn list_devices(
        &self,
        _context: &Context,
        request: ListDevices,
    ) -> Result<Vec<DeviceInfo>, WireError> {
        self.saw(request.into_call());
        Ok(Vec::new())
    }

    async fn revoke_device(
        &self,
        _context: &Context,
        request: RevokeDevice,
    ) -> Result<Ack, WireError> {
        self.saw(request.into_call());
        Ok(Ack)
    }

    async fn list_profiles(
        &self,
        _context: &Context,
        request: ListProfiles,
    ) -> Result<Vec<ProfileInfo>, WireError> {
        self.unsupported(request)
    }

    async fn reload_profiles(
        &self,
        _context: &Context,
        request: ReloadProfiles,
    ) -> Result<Vec<ProfileInfo>, WireError> {
        self.unsupported(request)
    }

    async fn move_profile(
        &self,
        _context: &Context,
        request: MoveProfile,
    ) -> Result<SessionInfo, WireError> {
        self.unsupported(request)
    }

    async fn get_manifest(
        &self,
        _context: &Context,
        request: GetManifest,
    ) -> Result<Manifest, WireError> {
        self.unsupported(request)
    }

    async fn read_blob(
        &self,
        _context: &Context,
        request: ReadBlob,
    ) -> Result<BlobChunk, WireError> {
        self.unsupported(request)
    }

    async fn watch_fleet(
        &self,
        _context: &Context,
        request: WatchFleet,
    ) -> Result<Generation, WireError> {
        self.unsupported(request)
    }

    async fn report_applied(
        &self,
        _context: &Context,
        request: ReportApplied,
    ) -> Result<Ack, WireError> {
        self.unsupported(request)
    }

    async fn list_members(
        &self,
        _context: &Context,
        request: ListMembers,
    ) -> Result<Vec<FleetMember>, WireError> {
        self.unsupported(request)
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

impl Fake {
    /// Record a call this fake has no data for and refuse it.
    fn unsupported<R: Request, T>(&self, request: R) -> Result<T, WireError> {
        self.saw(request.into_call());
        Err(WireError::new(ErrorCode::Unsupported, "not in this fake"))
    }
}

struct Served {
    probe: Arc<Probe>,
    forwarded: mpsc::Receiver<()>,
    seen: mpsc::Receiver<Call>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), ServeError<Disconnected>>>,
}

struct Running {
    client: Connection,
    probe: Arc<Probe>,
    forwarded: mpsc::Receiver<()>,
    seen: mpsc::Receiver<Call>,
    shutdown: CancellationToken,
    served: JoinHandle<Result<(), ServeError<Disconnected>>>,
}

fn pair() -> (MemoryTransport, MemoryTransport) {
    memory::pair(NonZeroUsize::new(16).unwrap())
}

fn serve(transport: MemoryTransport) -> Served {
    serve_with(transport, ServerConfig::default())
}

fn serve_with(transport: MemoryTransport, config: ServerConfig) -> Served {
    let (forwarded, forwarded_rx) = mpsc::channel(64);
    let probe = Arc::new(Probe {
        started: Notify::new(),
        aborted: Notify::new(),
        unsubscribed: Notify::new(),
        forwarded,
    });
    let (seen, seen_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let fake = Fake {
        probe: Arc::clone(&probe),
        seen,
        shutdown: shutdown.clone(),
    };
    let node = tokio::sync::watch::channel(node()).1;
    let server = Server::new(Arc::new(fake), node, config, shutdown.clone());
    let task = tokio::spawn(async move {
        server
            .serve(transport, |peer| Context {
                outbound: peer.outbound,
            })
            .await
    });
    Served {
        probe,
        forwarded: forwarded_rx,
        seen: seen_rx,
        shutdown,
        task,
    }
}

async fn start(config: ClientConfig) -> Running {
    let (client_end, server_end) = pair();
    let served = serve(server_end);
    let client = Connection::connect(client_end, client_info(), &config)
        .await
        .unwrap();
    Running {
        client,
        probe: served.probe,
        forwarded: served.forwarded,
        seen: served.seen,
        shutdown: served.shutdown,
        served: served.task,
    }
}

mod calls;
mod lifecycle;
mod protocol;
mod subscriptions;
