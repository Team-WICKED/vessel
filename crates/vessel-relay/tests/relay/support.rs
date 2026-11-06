//! A relay on a loopback port, nodes that echo, raw websocket peers that
//! follow (or break) the protocol by hand, and a TCP forwarder that records
//! what passes and can be pointed at another relay.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use vessel_proto::{ConnId, NodeToRelay, RelayToNode, RelayToken, RouteUrl, RoutingId};
use vessel_relay::{Limits, LinkState, NodeLink, RelayConfig, TlsConfig, WsCarrier};
use vessel_secure::{Config, NodeKeypair, SecureChannel, answer_proof, routing_id};

pub(crate) const TOKEN: &str = "relay-test-token";

/// Every test waits on events; this only stops a broken one.
pub(crate) const SAFETY: Duration = Duration::from_secs(30);

pub(crate) async fn within<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(SAFETY, work)
        .await
        .expect("the test hung")
}

pub(crate) fn tls() -> TlsConfig {
    TlsConfig::with_roots(rustls::RootCertStore::empty()).unwrap()
}

pub(crate) fn token() -> RelayToken {
    RelayToken::new(TOKEN).unwrap()
}

/// A relay serving on a loopback port until dropped.
pub(crate) struct Relay {
    pub(crate) url: RouteUrl,
    pub(crate) address: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Relay {
    pub(crate) async fn start() -> Self {
        Self::with_limits(Limits::default()).await
    }

    pub(crate) async fn with_limits(limits: Limits) -> Self {
        let mut config = RelayConfig::new(token());
        config.limits = limits;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                vessel_relay::serve(listener, config, shutdown)
                    .await
                    .unwrap();
            }
        });
        Self {
            url: format!("ws://{address}").parse().unwrap(),
            address,
            shutdown,
            task: Some(task),
        }
    }

    /// Stop serving and wait for the server to end.
    pub(crate) async fn stop(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            within(task).await.unwrap();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// A node linked to a relay whose clients get their frames echoed back.
pub(crate) struct EchoNode {
    pub(crate) keypair: Arc<NodeKeypair>,
    pub(crate) state: watch::Receiver<LinkState>,
    shutdown: CancellationToken,
}

impl EchoNode {
    pub(crate) fn start(relay: &RouteUrl) -> Self {
        Self::start_with(relay, Arc::new(NodeKeypair::generate().unwrap()), token())
    }

    pub(crate) fn start_with(
        relay: &RouteUrl,
        keypair: Arc<NodeKeypair>,
        token: RelayToken,
    ) -> Self {
        let link = NodeLink::new(relay.clone(), Arc::clone(&keypair), token, tls());
        let state = link.state();
        let shutdown = CancellationToken::new();
        let accept = {
            let keypair = Arc::clone(&keypair);
            move |carrier: WsCarrier| {
                let keypair = Arc::clone(&keypair);
                drop(tokio::spawn(async move { echo(carrier, &keypair).await }));
            }
        };
        drop(tokio::spawn(link.run(accept, shutdown.clone())));
        Self {
            keypair,
            state,
            shutdown,
        }
    }

    pub(crate) fn routing_id(&self) -> RoutingId {
        routing_id(self.keypair.public())
    }

    pub(crate) async fn wait_for(&mut self, wanted: LinkState) {
        let reached = within(self.state.wait_for(|state| *state == wanted))
            .await
            .unwrap();
        drop(reached);
    }
}

impl Drop for EchoNode {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Answer the handshake (closing with `4401` if it fails), then echo.
pub(crate) async fn echo(carrier: impl vessel_relay::NodeCarrier, keypair: &NodeKeypair) {
    let Ok(mut channel) = vessel_relay::respond(carrier, keypair, &Config::default()).await else {
        return;
    };
    while let Some(Ok(frame)) = channel.next().await {
        if channel.send(frame).await.is_err() {
            return;
        }
    }
}

pub(crate) type RawSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A websocket to `url` with the library's default (large) limits.
pub(crate) async fn raw(url: &str) -> RawSocket {
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

pub(crate) async fn send_json<T: serde::Serialize>(socket: &mut RawSocket, message: &T) {
    let text = serde_json::to_string(message).unwrap();
    socket.send(Message::Text(text.into())).await.unwrap();
}

pub(crate) async fn receive_json(socket: &mut RawSocket) -> RelayToNode {
    loop {
        match within(socket.next()).await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a relay message, got {other:?}"),
        }
    }
}

/// The close code the peer ends with (`None` for a close without one).
pub(crate) async fn close_code(socket: &mut RawSocket) -> Option<u16> {
    loop {
        match within(socket.next()).await {
            Some(Ok(Message::Close(frame))) => {
                return frame.map(|frame: CloseFrame| frame.code.into());
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            None | Some(Err(_)) => return None,
            Some(Ok(other @ (Message::Text(_) | Message::Binary(_) | Message::Frame(_)))) => {
                panic!("expected a close, got {other:?}")
            }
        }
    }
}

/// A node speaking the control protocol by hand: it registers properly,
/// then does only what the test says.
pub(crate) struct RawNode {
    pub(crate) relay: RouteUrl,
    pub(crate) control: RawSocket,
}

impl RawNode {
    pub(crate) async fn register(relay: &RouteUrl, keypair: &NodeKeypair) -> Self {
        let mut control = raw(&format!("{relay}{}", vessel_relay::NODE_PATH)).await;
        let key = *keypair.public();
        send_json(
            &mut control,
            &NodeToRelay::Register {
                routing_id: routing_id(&key),
                key,
                token: token(),
            },
        )
        .await;
        let RelayToNode::Prove { message } = receive_json(&mut control).await else {
            panic!("expected a challenge");
        };
        let message = answer_proof(keypair, &message).unwrap();
        send_json(&mut control, &NodeToRelay::Proof { message }).await;
        assert_eq!(receive_json(&mut control).await, RelayToNode::Registered);
        Self {
            relay: relay.clone(),
            control,
        }
    }

    pub(crate) async fn incoming(&mut self) -> ConnId {
        match receive_json(&mut self.control).await {
            RelayToNode::Incoming { conn } => conn,
            other @ (RelayToNode::Prove { .. }
            | RelayToNode::Registered
            | RelayToNode::Rejected { .. }) => {
                panic!("expected an incoming client, got {other:?}")
            }
        }
    }

    pub(crate) async fn accept(&self, conn: ConnId) -> RawSocket {
        raw(&format!(
            "{}{}?conn={conn}",
            self.relay,
            vessel_relay::ACCEPT_PATH
        ))
        .await
    }
}

pub(crate) fn connect_url(relay: &RouteUrl, node: &RoutingId) -> String {
    format!("{relay}{}?node={node}", vessel_relay::CONNECT_PATH)
}

/// A TCP forwarder in front of a relay: it records every byte, and can be
/// pointed at another relay (a relay "restart" at the same address).
pub(crate) struct Forwarder {
    pub(crate) url: RouteUrl,
    upstream: Arc<Mutex<SocketAddr>>,
    captured: Arc<Mutex<Vec<u8>>>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
    task: JoinHandle<()>,
}

impl Forwarder {
    pub(crate) async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = Arc::new(Mutex::new(upstream));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let (upstream, captured, connections) = (
                Arc::clone(&upstream),
                Arc::clone(&captured),
                Arc::clone(&connections),
            );
            async move {
                loop {
                    let (inbound, _peer) = listener.accept().await.unwrap();
                    let target = *upstream.lock().unwrap();
                    let captured = Arc::clone(&captured);
                    let connection = tokio::spawn(async move {
                        let Ok(outbound) = TcpStream::connect(target).await else {
                            return;
                        };
                        let (inbound_read, inbound_write) = inbound.into_split();
                        let (outbound_read, outbound_write) = outbound.into_split();
                        tokio::join!(
                            copy(inbound_read, outbound_write, Arc::clone(&captured)),
                            copy(outbound_read, inbound_write, captured),
                        );
                    });
                    connections.lock().unwrap().push(connection);
                }
            }
        });
        Self {
            url: format!("ws://{address}").parse().unwrap(),
            upstream,
            captured,
            connections,
            task,
        }
    }

    /// Send new connections to `upstream`, and cut the current ones.
    pub(crate) fn redirect(&self, upstream: SocketAddr) {
        *self.upstream.lock().unwrap() = upstream;
        for connection in self.connections.lock().unwrap().drain(..) {
            connection.abort();
        }
    }

    pub(crate) fn captured(&self) -> Vec<u8> {
        self.captured.lock().unwrap().clone()
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.task.abort();
        for connection in self.connections.lock().unwrap().drain(..) {
            connection.abort();
        }
    }
}

async fn copy(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    captured: Arc<Mutex<Vec<u8>>>,
) {
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        captured.lock().unwrap().extend_from_slice(&buffer[..read]);
        if to.write_all(&buffer[..read]).await.is_err() {
            break;
        }
    }
    let _shutdown = to.shutdown().await;
}

/// Reach `node` through `relay` and run the handshake to its key.
pub(crate) async fn channel(relay: &RouteUrl, node: &EchoNode) -> SecureChannel<WsCarrier> {
    let carrier = within(vessel_relay::dial(relay, &node.routing_id(), &tls()))
        .await
        .unwrap();
    within(vessel_secure::initiate(carrier, node.keypair.public()))
        .await
        .unwrap()
}
