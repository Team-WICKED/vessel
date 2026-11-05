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
    Ack, Call, ClientFrame, ClientInfo, CreateSession, ErrorCode, EventPage, EventRecord, EventSeq,
    GetNode, GetSession, Handler, Harness, HarnessKind, KillSession, ListHarnesses, ListSessions,
    NodeId, NodeInfo, Outcome, ProtocolRange, ProtocolVersion, ReadEvents, Request, RequestId,
    SendInput, ServerFrame, SessionEvent, SessionId, SessionInfo, SessionInput, SessionMode,
    SessionSpec, SessionStatus, StructuredSupport, Subscribe, Subscribed, SubscriptionEnd,
    TerminalBytes, TerminalInput, TerminalSize, Unsubscribe, WireError,
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
    }
}

fn spec() -> SessionSpec {
    SessionSpec {
        harness: HarnessKind::Codex,
        cwd: Some(PathBuf::from("/work")),
        mode: SessionMode::Terminal {
            args: vec!["--help".to_owned()],
            size: TerminalSize::default(),
        },
    }
}

fn info(id: SessionId, spec: SessionSpec) -> SessionInfo {
    SessionInfo {
        id,
        spec,
        status: SessionStatus::Running,
        created_at: DateTime::UNIX_EPOCH,
        ended_at: None,
        next_seq: EventSeq::new(RECORDS),
        harness_session: None,
    }
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
        self.saw(request.into_call());
        let next = request.from.next().unwrap();
        Ok(EventPage {
            records: vec![record(request.from.get())],
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
            .open_subscription()
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
    };
    let server = Server::new(
        Arc::new(fake),
        node(),
        ServerConfig::default(),
        shutdown.clone(),
    );
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
mod subscriptions;
