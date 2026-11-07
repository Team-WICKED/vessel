//! Keepalive on direct routes: a node pings its `/secure` connections, and
//! both ends drop a connection silent past `Keepalive::DEFAULT.silence`, as
//! relayed ones are. On paused time: the 10 s pings and the 30 s limit pass
//! in an instant, with real loopback sockets underneath.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures_util::StreamExt as _;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use vessel_relay::{AxumCarrier, Keepalive, connect};

use crate::support::tls;

/// Generous on paused time: only a hang reaches it.
const HANG: Duration = Duration::from_secs(600);

/// How the node's side of one connection ended, and after how long.
type Ended = (Option<io::ErrorKind>, Duration);

/// A node whose `/secure` reads its carrier (with the default keepalive)
/// until it ends, and reports how; its URL.
async fn pinging_node() -> (String, mpsc::Receiver<Ended>) {
    async fn secure(
        State(ended): State<mpsc::Sender<Ended>>,
        upgrade: WebSocketUpgrade,
    ) -> Response {
        upgrade.on_upgrade(move |socket| async move {
            let started = Instant::now();
            let mut carrier = AxumCarrier::new(socket, Some(Keepalive::DEFAULT));
            let end = loop {
                match carrier.next().await {
                    Some(Ok(_record)) => {}
                    Some(Err(error)) => break Some(error.kind()),
                    None => break None,
                }
            };
            let _sent = ended.send((end, started.elapsed())).await;
        })
    }
    let (ended, reports) = mpsc::channel(4);
    let url = serve(
        Router::new()
            .route("/secure", get(secure))
            .with_state(ended),
    )
    .await;
    (url, reports)
}

/// A node whose `/secure` upgrades, then never reads, writes or pings.
async fn silent_node() -> String {
    type Held = Arc<Mutex<Vec<WebSocket>>>;
    async fn secure(State(held): State<Held>, upgrade: WebSocketUpgrade) -> Response {
        upgrade.on_upgrade(move |socket| async move {
            held.lock().unwrap().push(socket);
        })
    }
    let held: Held = Arc::default();
    serve(Router::new().route("/secure", get(secure)).with_state(held)).await
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    format!("ws://{address}/secure")
}

fn about(elapsed: Duration, expected: Duration) -> bool {
    elapsed >= expected && Some(elapsed) < expected.checked_add(Duration::from_secs(2))
}

/// A client that stops reading and answering (a laptop that slept): the
/// node keeps pinging, hears no pong, and drops the connection after the
/// silence limit.
#[tokio::test(start_paused = true)]
async fn a_node_drops_a_direct_connection_whose_client_went_silent() {
    let (url, mut ended) = pinging_node().await;
    let (mut client, _response) = tokio_tungstenite::connect_async(&url).await.unwrap();

    let (end, after) = tokio::time::timeout(HANG, ended.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end, Some(io::ErrorKind::TimedOut));
    assert!(about(after, Keepalive::DEFAULT.silence), "{after:?}");

    // What the client never read: the node's pings, every 10 s until then.
    let mut pings = 0;
    while let Some(Ok(message)) = client.next().await {
        if matches!(message, Message::Ping(_)) {
            pings += 1;
        }
    }
    assert!(pings >= 2, "{pings} pings");
}

/// A client that reads answers the pings (its websocket pongs by itself),
/// so a quiet connection stays up on both ends for as long as it lasts.
#[tokio::test(start_paused = true)]
async fn a_quiet_direct_connection_stays_up_while_the_client_answers() {
    let (url, mut ended) = pinging_node().await;
    let mut carrier = connect(&url, &tls()).await.unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let reading = tokio::spawn(async move {
        tokio::select! {
            next = carrier.next() => Some(next.map(|item| item.map(|_record| ()).map_err(|error| error.kind()))),
            _stop = stopped => None,
        }
    });

    tokio::time::sleep(Keepalive::DEFAULT.silence.saturating_mul(3)).await;
    assert!(ended.try_recv().is_err(), "the node dropped a live client");
    assert!(!reading.is_finished(), "the client dropped a live node");

    drop(stop);
    assert_eq!(reading.await.unwrap(), None);
    let (end, _after) = tokio::time::timeout(HANG, ended.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        end,
        Some(io::ErrorKind::TimedOut),
        "it ended because the client left"
    );
}

/// A node that goes silent (it stopped, its machine slept, a NAT dropped
/// the mapping): the client's direct carrier fails after the silence limit,
/// as one through a relay does, and the reconnecting stream takes over.
#[tokio::test(start_paused = true)]
async fn a_client_drops_a_direct_connection_whose_node_went_silent() {
    let url = silent_node().await;
    let mut carrier = connect(&url, &tls()).await.unwrap();
    let started = Instant::now();
    let next = tokio::time::timeout(HANG, carrier.next()).await.unwrap();
    let error = next.unwrap().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(
        about(started.elapsed(), Keepalive::DEFAULT.silence),
        "{:?}",
        started.elapsed()
    );
}
