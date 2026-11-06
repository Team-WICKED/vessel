//! `SecureConnector` over in-memory carriers against the fake node behind a
//! real Noise channel: route order and dial timeouts, which failures are
//! final, and resuming over the next route after one drops (P5).

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use tokio::task::JoinHandle;
use vessel_proto::{
    DeviceId, DeviceSecret, NodeClose, Preamble, PreambleReply, RefusalReason, RelayClose,
    RouteUrl, RoutingId,
};
use vessel_rpc::CallError;
use vessel_secure::{MemoryCarrier, NodeKeypair, memory_pair};

use super::memory::{FAST, drain, notices, records};
use super::*;
use crate::address::SecureTarget;
use crate::secure::{Dialer, PeerClose, ReachError, RouteError, SecureConnector};
use crate::{Connector, Pinned, StreamError, follow_resuming, pairing};

/// What a route does when dialled.
#[derive(Debug, Clone, Copy)]
enum Behaviour {
    /// Reaches the node.
    Up,
    /// The dial fails at once.
    Down,
    /// The dial never finishes.
    Hang,
    /// Someone else answers: the handshake fails and they close with 4401.
    Impostor,
    /// The far end closes with this code after the first message (a relay
    /// that has no such node, a rate limit).
    Closes(u16),
}

/// What the node says to a device's credential.
#[derive(Debug, Clone, Copy)]
enum Answer {
    Welcome,
    Refuse(RefusalReason),
}

/// Dials in-memory carriers to a fake node behind its key, route by route
/// as told, and can cut every connection it made.
struct MemoryDialer {
    node: Arc<FakeNode>,
    keypair: Arc<NodeKeypair>,
    routes: Mutex<HashMap<String, Behaviour>>,
    answer: Mutex<Answer>,
    dialled: Mutex<Vec<String>>,
    serving: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("the fake network is down")]
struct Unreachable;

/// A close code inside a carrier error, as a websocket carrier reports it.
#[derive(Debug, thiserror::Error)]
#[error("closed with {0}")]
struct CloseCode(u16);

impl MemoryDialer {
    fn new(node: Arc<FakeNode>) -> Arc<Self> {
        Arc::new(Self {
            node,
            keypair: Arc::new(NodeKeypair::generate().unwrap()),
            routes: Mutex::new(HashMap::new()),
            answer: Mutex::new(Answer::Welcome),
            dialled: Mutex::new(Vec::new()),
            serving: Mutex::new(Vec::new()),
        })
    }

    fn set(&self, route: &str, behaviour: Behaviour) {
        let _previous = self
            .routes
            .lock()
            .unwrap()
            .insert(route.to_owned(), behaviour);
    }

    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    fn dialled(&self) -> Vec<String> {
        self.dialled.lock().unwrap().clone()
    }

    /// Cut every open connection, and wait until the node's ends are gone.
    async fn drop_all(&self) {
        let serving = std::mem::take(&mut *self.serving.lock().unwrap());
        for task in serving {
            task.abort();
            let _cancelled = task.await;
        }
    }

    async fn open(&self, route: &RouteUrl) -> Result<Coded, Unreachable> {
        self.dialled.lock().unwrap().push(route.to_string());
        let behaviour = self
            .routes
            .lock()
            .unwrap()
            .get(route.as_str())
            .copied()
            .unwrap_or(Behaviour::Up);
        let (client_end, node_end) = memory_pair(NonZeroUsize::new(16).unwrap());
        let close = match behaviour {
            Behaviour::Down => return Err(Unreachable),
            Behaviour::Hang => std::future::pending().await,
            Behaviour::Up => {
                let task = tokio::spawn(serve(
                    Arc::clone(&self.node),
                    Arc::clone(&self.keypair),
                    *self.answer.lock().unwrap(),
                    node_end,
                ));
                self.serving.lock().unwrap().push(task);
                None
            }
            Behaviour::Impostor => {
                let impostor = NodeKeypair::generate().unwrap();
                drop(tokio::spawn(async move {
                    let _failed = vessel_secure::respond(node_end, &impostor).await;
                }));
                Some(NodeClose::HandshakeFailed.code())
            }
            Behaviour::Closes(code) => {
                drop(tokio::spawn(async move {
                    let mut node_end = node_end;
                    let _first = node_end.next().await;
                }));
                Some(code)
            }
        };
        Ok(Coded {
            carrier: client_end,
            close,
        })
    }
}

impl Dialer for MemoryDialer {
    type Carrier = Coded;
    type Error = Unreachable;

    async fn direct(&self, route: &RouteUrl) -> Result<Coded, Unreachable> {
        self.open(route).await
    }

    async fn relay(&self, route: &RouteUrl, node: &RoutingId) -> Result<Coded, Unreachable> {
        assert_eq!(*node, vessel_secure::routing_id(self.keypair.public()));
        self.open(route).await
    }

    fn closed_with(&self, error: &io::Error) -> Option<PeerClose> {
        let code = error.get_ref()?.downcast_ref::<CloseCode>()?.0;
        NodeClose::from_code(code)
            .map(PeerClose::Node)
            .or_else(|| RelayClose::from_code(code).map(PeerClose::Relay))
    }
}

/// An in-memory carrier whose end reports a close code, as a websocket
/// closed with one does.
struct Coded {
    carrier: MemoryCarrier,
    close: Option<u16>,
}

impl Coded {
    fn closed(&self) -> Option<io::Error> {
        self.close
            .map(|code| io::Error::new(io::ErrorKind::ConnectionAborted, CloseCode(code)))
    }
}

impl Stream for Coded {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.carrier).poll_next(cx) {
            Poll::Ready(None) => Poll::Ready(this.closed().map(Err)),
            other @ (Poll::Ready(Some(_)) | Poll::Pending) => other,
        }
    }
}

impl Sink<Bytes> for Coded {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.carrier)
            .poll_ready(cx)
            .map_err(|error| this.closed().unwrap_or(error))
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> io::Result<()> {
        let this = self.get_mut();
        Pin::new(&mut this.carrier)
            .start_send(item)
            .map_err(|error| this.closed().unwrap_or(error))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.carrier)
            .poll_flush(cx)
            .map_err(|error| this.closed().unwrap_or(error))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().carrier).poll_close(cx)
    }
}

/// The fake node behind its key: the handshake, the device preamble
/// (answered as told, without checking the credential), then the
/// protocol.
async fn serve(
    node: Arc<FakeNode>,
    keypair: Arc<NodeKeypair>,
    answer: Answer,
    carrier: MemoryCarrier,
) {
    let Ok(mut channel) = vessel_secure::respond(carrier, &keypair).await else {
        return;
    };
    let Some(Ok(frame)) = channel.next().await else {
        return;
    };
    let Ok(Preamble::DeviceAuth { device, .. }) = serde_json::from_str(&frame) else {
        return;
    };
    let reply = match answer {
        Answer::Welcome => PreambleReply::Authenticated { device },
        Answer::Refuse(reason) => PreambleReply::Refused { reason },
    };
    let reply = serde_json::to_string(&reply).unwrap();
    if channel.send(reply).await.is_err() || matches!(answer, Answer::Refuse(_)) {
        return;
    }
    let _served = server(node)
        .serve(channel, |peer| Context {
            outbound: peer.outbound,
        })
        .await;
}

fn url(text: &str) -> RouteUrl {
    text.parse().unwrap()
}

fn connector(
    dialer: &Arc<MemoryDialer>,
    direct: &[&str],
    relays: &[&str],
) -> SecureConnector<Arc<MemoryDialer>> {
    let target = SecureTarget {
        key: *dialer.keypair.public(),
        direct: direct.iter().map(|route| url(route)).collect(),
        relays: relays.iter().map(|route| url(route)).collect(),
    };
    let credential = pairing::credential(DeviceId::random(), DeviceSecret::from_bytes([7; 32]));
    SecureConnector::new(Arc::clone(dialer), target, credential)
}

const D1: &str = "ws://192.0.2.1:7077";
const D2: &str = "ws://192.0.2.2:7077";
const R1: &str = "wss://relay-one.example";
const R2: &str = "wss://relay-two.example";

#[tokio::test(start_paused = true)]
async fn direct_addresses_come_first_each_bounded_then_relays_in_order() {
    let dialer = MemoryDialer::new(Arc::new(FakeNode::new(0)));
    dialer.set(D1, Behaviour::Down);
    dialer.set(D2, Behaviour::Hang);
    dialer.set(R1, Behaviour::Closes(RelayClose::NoSuchNode.code()));
    let connector = connector(&dialer, &[D1, D2], &[R1, R2]);

    let started = tokio::time::Instant::now();
    let connection = connector.connect(client_info()).await.unwrap();
    let waited = started.elapsed();

    assert_eq!(dialer.dialled(), [D1, D2, R1, R2]);
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(4),
        "only the hanging direct address costs time, 3 s: {waited:?}"
    );
    assert_eq!(connection.node().id, *dialer.node.id.lock().unwrap());
}

fn failures(error: &ReachError<Unreachable>) -> Vec<String> {
    error
        .failures()
        .iter()
        .map(|failure| match &failure.error {
            RouteError::Dial { .. } => "dial".to_owned(),
            RouteError::DialTimeout(_) => "timeout".to_owned(),
            RouteError::WrongKey => "wrong key".to_owned(),
            RouteError::Relay(close) => format!("relay {}", close.code()),
            RouteError::Open(error) => format!("open: {error}"),
        })
        .collect()
}

#[tokio::test]
async fn a_wrong_key_is_final_and_a_relay_without_the_node_is_not() {
    let dialer = MemoryDialer::new(Arc::new(FakeNode::new(0)));
    dialer.set(D1, Behaviour::Impostor);
    dialer.set(R1, Behaviour::Closes(RelayClose::NoSuchNode.code()));
    dialer.set(R2, Behaviour::Closes(RelayClose::RateLimited.code()));

    let away = connector(&dialer, &[], &[R1, R2]);
    let error = away.connect(client_info()).await.unwrap_err();
    assert_eq!(failures(&error), ["relay 4404", "relay 4429"]);
    assert!(away.is_transient(&error), "the node may be restarting");
    let text = error.to_string();
    assert!(
        text.contains(&format!("relay {R1}: the node is not connected")),
        "{text}"
    );

    // Every route is tried; the wrong key outweighs the relay that was
    // only missing the node.
    let rekeyed = connector(&dialer, &[D1], &[R1]);
    let error = rekeyed.connect(client_info()).await.unwrap_err();
    assert_eq!(failures(&error), ["wrong key", "relay 4404"]);
    assert!(!rekeyed.is_transient(&error));
    assert!(error.to_string().contains("pair it again"), "{error}");

    // A message too large for the relay does not pass with time either.
    dialer.set(R2, Behaviour::Closes(RelayClose::TooLarge.code()));
    let error = away.connect(client_info()).await.unwrap_err();
    assert!(!away.is_transient(&error), "{error}");

    let nowhere = connector(&dialer, &[], &[]);
    let error = nowhere.connect(client_info()).await.unwrap_err();
    assert!(!nowhere.is_transient(&error));
    assert_eq!(error.to_string(), "the node has no route to try");
}

#[tokio::test]
async fn the_nodes_refusal_ends_the_search_on_the_first_route() {
    let dialer = MemoryDialer::new(Arc::new(FakeNode::new(0)));
    let connector = connector(&dialer, &[D1], &[R1]);
    for (reason, transient) in [
        (RefusalReason::Revoked, false),
        (RefusalReason::UnknownDevice, false),
        (RefusalReason::Unavailable, true),
    ] {
        dialer.dialled.lock().unwrap().clear();
        dialer.answer(Answer::Refuse(reason));
        let error = connector.connect(client_info()).await.unwrap_err();
        assert_eq!(dialer.dialled(), [D1], "the relay leads to the same node");
        assert_eq!(connector.is_transient(&error), transient, "{reason:?}");
    }
    dialer.answer(Answer::Welcome);
    assert!(connector.connect(client_info()).await.is_ok());
}

/// P5: the route drops mid-stream, the first route is gone when the stream
/// dials again, and it resumes over the next one at its cursor, with
/// nothing repeated or skipped. A call in flight fails as `Disconnected`
/// and is not retried.
#[tokio::test]
async fn a_dropped_route_resumes_over_the_next_one_at_the_cursor() {
    let node = Arc::new(FakeNode::new(0));
    let dialer = MemoryDialer::new(Arc::clone(&node));
    let connector = Arc::new(Pinned::new(connector(&dialer, &[], &[R1, R2]), None));
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);
    let clients = stream.client();

    let seen = drain(&mut stream, async |records| {
        if records == 3 {
            let client = clients.borrow().clone().unwrap();
            let kill = tokio::spawn(async move { client.kill(session_id()).await });
            while node.kills.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
            dialer.set(R1, Behaviour::Down);
            dialer.drop_all().await;
            assert!(matches!(kill.await.unwrap(), Err(CallError::Disconnected)));
        }
    })
    .await;

    assert_eq!(records(&seen), (0..LOG_LEN).collect::<Vec<_>>(), "{seen:?}");
    let notices = notices(&seen);
    assert!(
        matches!(
            notices.as_slice(),
            [
                StreamError::Reconnecting { attempt: 1, .. },
                StreamError::Resumed { .. }
            ]
        ),
        "{seen:?}"
    );
    assert_eq!(dialer.dialled(), [R1, R1, R2]);
    assert_eq!(node.kills.load(Ordering::Relaxed), 1, "never retried");
}

/// P5: a device revoked while attached loses its connection, and the
/// redial is refused with `Revoked`: the stream ends with that reason
/// instead of retrying.
#[tokio::test]
async fn a_revoked_device_ends_the_stream_with_the_reason() {
    let node = Arc::new(FakeNode::new(0));
    let dialer = MemoryDialer::new(Arc::clone(&node));
    let connector = Arc::new(Pinned::new(connector(&dialer, &[D1], &[R1]), None));
    let mut stream = follow_resuming(
        Arc::clone(&connector),
        client_info(),
        session_id(),
        EventSeq::ZERO,
    )
    .with_backoff(FAST);

    let seen = drain(&mut stream, async |records| {
        if records == 3 {
            dialer.answer(Answer::Refuse(RefusalReason::Revoked));
            dialer.drop_all().await;
        }
    })
    .await;

    // What was already on its way before the drop, then nothing more.
    let got = records(&seen);
    let expected: Vec<u64> = (0..u64::try_from(got.len()).unwrap()).collect();
    assert!(got.len() >= 3 && got == expected, "{seen:?}");
    let notices = notices(&seen);
    let [
        StreamError::Reconnecting { attempt: 1, .. },
        StreamError::Unreachable { reason },
    ] = notices.as_slice()
    else {
        panic!("{seen:?}");
    };
    assert!(reason.contains("revoked"), "{reason}");
    assert!(stream.next().await.is_none(), "the stream ended");
    assert_eq!(dialer.dialled(), [D1, D1], "the relay was not asked");
}
