//! The client against a fake node: over the in-memory transport for stream
//! logic, and over a loopback websocket for the connection and the token.

mod stream;
mod websocket;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::DateTime;
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    Ack, ClientInfo, CreateSession, ErrorCode, EventPage, EventRecord, EventSeq, GetNode,
    GetSession, Handler, Harness, HarnessKind, KillSession, ListHarnesses, ListSessions, NodeId,
    NodeInfo, ProtocolRange, ReadEvents, SendInput, SessionEvent, SessionId, SessionInfo,
    SessionMode, SessionSpec, SessionStatus, Subscribe, Subscribed, SubscriptionEnd, TerminalSize,
    Unsubscribe, WireError,
};
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
    }
}

fn info() -> SessionInfo {
    SessionInfo {
        id: session_id(),
        spec: SessionSpec {
            harness: HarnessKind::OpenCode,
            cwd: Some(PathBuf::from("/work")),
            mode: SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        },
        status: SessionStatus::Running,
        created_at: DateTime::UNIX_EPOCH,
        ended_at: None,
        next_seq: EventSeq::new(LOG_LEN),
        harness_session: None,
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
    /// Records below this were pruned.
    oldest: u64,
    subscribes: AtomicUsize,
    reads: AtomicUsize,
}

impl FakeNode {
    fn new(oldest: u64) -> Self {
        Self {
            oldest,
            subscribes: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        }
    }
}

struct Context {
    outbound: Outbound,
}

fn unsupported() -> WireError {
    WireError::new(ErrorCode::Unsupported, "not in the fake")
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
        Err(unsupported())
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
        if request.session != session_id() {
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
}

fn server(fake: Arc<FakeNode>) -> Arc<Server<FakeNode>> {
    Arc::new(Server::new(
        fake,
        node(),
        ServerConfig::default(),
        CancellationToken::new(),
    ))
}

/// A client connected to `node` over the in-memory transport.
async fn connect(node: Arc<FakeNode>) -> NodeClient {
    let (client_end, server_end) = vessel_rpc::memory::pair(NonZeroUsize::new(16).unwrap());
    let server = server(node);
    drop(tokio::spawn(async move {
        server
            .serve(server_end, |peer| Context {
                outbound: peer.outbound,
            })
            .await
    }));
    NodeClient::over(client_end, client_info(), &ClientConfig::default())
        .await
        .unwrap()
}
