//! The fixtures: a running UI, a browser tab on it, real nodes in this
//! process with the fake harnesses, and a loopback proxy in front of a
//! node that a test can cut.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_util::sync::{CancellationToken, DropGuard};
use vessel_client::{BearerToken, ConnectError, Connector, NodeClient, NodeUrl, WsConnector};
use vessel_harness::Lookup;
use vessel_harness::fakes::{Role, install};
use vessel_node::{Config, DataDir, Node};
use vessel_proto::{Call, ClientInfo, HarnessKind, NodeId, RawJson};
use vessel_testkit::Fake;
use vessel_web::wire::{
    BookNode, CallId, ClientFrame, NodeKey, Op, PresentedToken, ServerFrame, Transport,
};
use vessel_web::{LaunchToken, NodeSource, UiConfig};

/// Only turns a hang into a failure (the fakes are built before the
/// tests run, as in CI).
pub(crate) const SAFETY: Duration = Duration::from_secs(60);

pub(crate) async fn within<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(SAFETY, Box::pin(work)).await.unwrap()
}

/// A running UI and its token.
pub(crate) struct Ui {
    pub(crate) address: SocketAddr,
    pub(crate) token: LaunchToken,
    _stop: DropGuard,
}

impl Ui {
    /// A UI offering `source`'s nodes, with a short hello deadline and fast
    /// redials.
    pub(crate) async fn start(source: Source) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = LaunchToken::mint().unwrap();
        let mut config = UiConfig::new(token.clone());
        config.hello_deadline = Duration::from_millis(300);
        config.backoff = vessel_client::Backoff {
            first: Duration::from_millis(20),
            max: Duration::from_millis(100),
        };
        let stop = CancellationToken::new();
        drop(tokio::spawn(vessel_web::serve(
            listener,
            config,
            source,
            stop.clone(),
        )));
        Self {
            address,
            token,
            _stop: stop.drop_guard(),
        }
    }

    pub(crate) fn host(&self) -> String {
        self.address.to_string()
    }

    pub(crate) fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The token as the page has it (from the URL's fragment).
    pub(crate) fn secret(&self) -> String {
        let url = self.token.url(self.address);
        url.split_once("#token=").unwrap().1.to_owned()
    }

    /// A tab: a websocket with our origin, welcomed.
    pub(crate) async fn tab(&self) -> Tab {
        let mut request = format!("ws://{}/api/ws", self.address)
            .into_client_request()
            .unwrap();
        let _none = request
            .headers_mut()
            .insert("origin", self.origin().parse().unwrap());
        let (socket, _response) = within(tokio_tungstenite::connect_async(request))
            .await
            .unwrap();
        let mut tab = Tab { socket, next_id: 0 };
        tab.send(&ClientFrame::Hello {
            token: PresentedToken::new(self.secret()),
        })
        .await;
        tab.until(|frame| matches!(frame, ServerFrame::Welcome { .. }).then_some(()))
            .await;
        tab
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// A browser tab, as the page's bridge client would drive it.
pub(crate) struct Tab {
    socket: Socket,
    next_id: u64,
}

impl Tab {
    pub(crate) async fn send(&mut self, frame: &ClientFrame) {
        let text = serde_json::to_string(frame).unwrap();
        within(self.socket.send(Message::Text(text.into())))
            .await
            .unwrap();
    }

    /// Send text as a frame, whatever it holds.
    pub(crate) async fn send_text(&mut self, text: &str) {
        within(self.socket.send(Message::Text(text.into())))
            .await
            .unwrap();
    }

    /// The next frame from the bridge.
    pub(crate) async fn next(&mut self) -> ServerFrame {
        loop {
            match within(self.socket.next()).await {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("the bridge closed: {other:?}"),
            }
        }
    }

    /// Read frames until `pick` takes one.
    pub(crate) async fn until<T>(&mut self, mut pick: impl FnMut(ServerFrame) -> Option<T>) -> T {
        loop {
            if let Some(picked) = pick(self.next().await) {
                return picked;
            }
        }
    }

    /// Send `call` to `node`; its id.
    pub(crate) async fn call(&mut self, node: &str, call: &Call) -> CallId {
        let id = self.id();
        let call = RawJson::from_serialize(call).unwrap();
        self.send(&ClientFrame::Call {
            id,
            node: NodeKey::new(node),
            call,
        })
        .await;
        id
    }

    /// Send a call as raw text (what a broken page might send); its id.
    pub(crate) async fn call_text(&mut self, node: &str, call: &str) -> CallId {
        let id = self.id();
        self.send(&ClientFrame::Call {
            id,
            node: NodeKey::new(node),
            call: RawJson::from_text(call).unwrap(),
        })
        .await;
        id
    }

    /// Ask the bridge; the op's id.
    pub(crate) async fn op(&mut self, op: Op) -> CallId {
        let id = self.id();
        self.send(&ClientFrame::Op { id, op }).await;
        id
    }

    fn id(&mut self) -> CallId {
        self.next_id = self.next_id.checked_add(1).unwrap();
        CallId::new(self.next_id)
    }
}

/// A node in this process with `fake-tui` as `claude` and `fake-acp` as
/// its ACP adapter, under a temporary home.
pub(crate) struct TestNode {
    data: TempDir,
    _bin: TempDir,
    node: Node,
}

impl TestNode {
    pub(crate) async fn start() -> Self {
        let data = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        let acp = Fake::Acp.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
        let user_home = data.path().join("user-home");
        std::fs::create_dir_all(&user_home).unwrap();
        let mut config = Config::new(
            DataDir::new(data.path()),
            vessel_harness::HarnessEnv::new(user_home),
        );
        config.bind = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        config.harness_lookup = Lookup::new([bin.path().to_path_buf()]);
        let node = Node::start(config).await.unwrap();
        Self {
            data,
            _bin: bin,
            node,
        }
    }

    pub(crate) fn address(&self) -> SocketAddr {
        self.node.address().unwrap()
    }

    pub(crate) fn id(&self) -> NodeId {
        self.node.info().id
    }

    pub(crate) fn dir(&self) -> &std::path::Path {
        self.data.path()
    }

    pub(crate) fn token(&self) -> BearerToken {
        let token = DataDir::new(self.data.path()).read_token().unwrap();
        token.expose().parse().unwrap()
    }
}

/// A loopback proxy in front of `upstream` whose connections can be cut
/// at once (a laptop's network dropping), while new ones still pass.
pub(crate) struct Proxy {
    pub(crate) address: SocketAddr,
    current: Arc<Mutex<CancellationToken>>,
    _stop: DropGuard,
}

impl Proxy {
    pub(crate) async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let current = Arc::new(Mutex::new(CancellationToken::new()));
        let stop = CancellationToken::new();
        let accepting = Arc::clone(&current);
        let stopped = stop.clone();
        drop(tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    () = stopped.cancelled() => return,
                };
                let Ok((inbound, _from)) = accepted else {
                    return;
                };
                let cut = accepting.lock().unwrap().clone();
                drop(tokio::spawn(pipe(inbound, upstream, cut)));
            }
        }));
        Self {
            address,
            current,
            _stop: stop.drop_guard(),
        }
    }

    /// Close every connection through the proxy now.
    pub(crate) fn cut(&self) {
        let mut current = self.current.lock().unwrap();
        current.cancel();
        *current = CancellationToken::new();
    }
}

async fn pipe(mut inbound: TcpStream, upstream: SocketAddr, cut: CancellationToken) {
    let Ok(mut outbound) = TcpStream::connect(upstream).await else {
        return;
    };
    tokio::select! {
        _copied = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
        () = cut.cancelled() => {}
    }
}

/// The nodes the tests' UI offers: names to websocket addresses and
/// tokens, with every dial counted.
#[derive(Default)]
pub(crate) struct Source {
    nodes: Vec<(String, NodeUrl, BearerToken)>,
    dials: Arc<AtomicUsize>,
}

impl Source {
    /// Offer the node at `address` as `name`.
    pub(crate) fn with(mut self, name: &str, address: SocketAddr, token: BearerToken) -> Self {
        let url = address.to_string().parse().unwrap();
        self.nodes.push((name.to_owned(), url, token));
        self
    }

    /// Counts the dials made through this source.
    pub(crate) fn dials(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.dials)
    }
}

/// There is no such node in the tests' source.
#[derive(Debug, thiserror::Error)]
#[error("no node named {0}")]
pub(crate) struct NoSuchNode(String);

impl NodeSource for Source {
    type Connector = Counted;
    type Error = NoSuchNode;

    async fn nodes(&self) -> Result<Vec<BookNode>, NoSuchNode> {
        Ok(self
            .nodes
            .iter()
            .map(|(name, url, _token)| BookNode {
                key: NodeKey::new(name.clone()),
                label: name.clone(),
                address: url.to_string(),
                transport: Transport::Ws,
                default: false,
                id: None,
            })
            .collect())
    }

    async fn connector(&self, node: &NodeKey) -> Result<Counted, NoSuchNode> {
        let (_name, url, token) = self
            .nodes
            .iter()
            .find(|(name, _url, _token)| name == node.as_str())
            .ok_or_else(|| NoSuchNode(node.to_string()))?;
        Ok(Counted {
            inner: WsConnector::new(url.clone(), token.clone()),
            dials: Arc::clone(&self.dials),
        })
    }

    async fn seen(&self, _node: &NodeKey, _id: NodeId) {}

    async fn pair(&self, name: &str, _link: &str) -> Result<BookNode, NoSuchNode> {
        Err(NoSuchNode(name.to_owned()))
    }
}

/// A websocket connector that counts its dials.
pub(crate) struct Counted {
    inner: WsConnector,
    dials: Arc<AtomicUsize>,
}

impl Connector for Counted {
    type Error = ConnectError;

    async fn connect(&self, client: ClientInfo) -> Result<NodeClient, ConnectError> {
        let _before = self.dials.fetch_add(1, Ordering::SeqCst);
        self.inner.connect(client).await
    }

    fn is_transient(&self, error: &ConnectError) -> bool {
        self.inner.is_transient(error)
    }
}
