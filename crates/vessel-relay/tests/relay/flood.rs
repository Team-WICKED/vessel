//! The security review's round 7: connections that hold a relay's places
//! while doing nothing. Requests that never finish their headers, many
//! connections from one address, a stop with them still open; registering
//! places and a node's client places filled by strangers. Several client
//! addresses are stood for by `X-Real-IP` from a trusted loopback proxy,
//! as behind a reverse proxy (`Relay::with_config`), so the tests run on
//! every platform's loopback.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;
use vessel_proto::{NodeToRelay, RegistrationRefusal, RelayClose, RelayToNode};
use vessel_relay::{Limits, LinkState};
use vessel_secure::{NodeKeypair, answer_proof, routing_id};

use crate::support::{
    EchoNode, RawNode, RawSocket, Relay, channel, close_code, connect_url, raw, raw_from,
    receive_json, send_json, token, within,
};

/// The header the proxy-style relays here read the client's address from.
const HEADER: &str = "x-real-ip";

/// Long enough that a connection the relay should end has been ended:
/// every deadline under test is a few hundred milliseconds.
const ENDED: Duration = Duration::from_secs(5);

/// Default limits, except that one address may connect as often as these
/// tests do: each limit is tested alone.
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.connect_burst = 10_000;
    limits
}

fn node_url(relay: &Relay) -> String {
    format!("{}{}", relay.url, vessel_relay::NODE_PATH)
}

/// A connection that sends half a request's headers and then nothing.
async fn half_request(address: SocketAddr) -> TcpStream {
    let mut stream = within(TcpStream::connect(address)).await.unwrap();
    within(stream.write_all(b"GET /v1/node HTTP/1.1\r\nHost: relay\r\n"))
        .await
        .unwrap();
    stream
}

/// What the relay said before it ended `stream`, if it ended it within
/// `wait` (a reset counts as ended, with nothing said).
async fn ended(stream: &mut TcpStream, wait: Duration) -> Option<String> {
    let mut said = Vec::new();
    let read = timeout(wait, stream.read_to_end(&mut said)).await.ok()?;
    if read.is_err() {
        said.clear();
    }
    Some(String::from_utf8_lossy(&said).into_owned())
}

/// The relay's last answer to a registration by hand of `keypair` over
/// `control` (`registered` or `rejected`).
async fn register_over(control: &mut RawSocket, keypair: &NodeKeypair) -> RelayToNode {
    let key = *keypair.public();
    send_json(
        control,
        &NodeToRelay::Register {
            routing_id: routing_id(&key),
            key,
            token: Some(token()),
            ticket: None,
        },
    )
    .await;
    let message = match receive_json(control).await {
        RelayToNode::Prove { message } => message,
        other @ (RelayToNode::Registered
        | RelayToNode::Rejected { .. }
        | RelayToNode::Incoming { .. }) => return other,
    };
    let message = answer_proof(keypair, &message).unwrap();
    send_json(control, &NodeToRelay::Proof { message }).await;
    receive_json(control).await
}

#[tokio::test]
async fn a_request_that_never_finishes_its_headers_is_closed() {
    let mut limits = limits();
    limits.header_read_timeout = Duration::from_millis(300);
    let relay = Relay::with_limits(limits).await;
    let mut held = Vec::new();
    for _ in 0..20 {
        held.push(half_request(relay.address).await);
    }
    for stream in &mut held {
        assert!(ended(stream, ENDED).await.is_some(), "still open");
    }
}

#[tokio::test]
async fn connections_from_one_address_are_capped_from_accept() {
    let mut limits = limits();
    limits.max_connections_per_address = 4;
    let relay = Relay::with_limits(limits).await;
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(half_request(relay.address).await);
    }
    // The fifth is answered and closed at once, before any HTTP.
    let mut fifth = within(TcpStream::connect(relay.address)).await.unwrap();
    let said = ended(&mut fifth, ENDED)
        .await
        .expect("the fifth stayed open");
    assert!(said.starts_with("HTTP/1.1 503"), "{said:?}");
    // One ends; its place is free.
    drop(held.pop());
    let health = within(async {
        loop {
            let mut stream = TcpStream::connect(relay.address).await.unwrap();
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let said = ended(&mut stream, ENDED).await.unwrap_or_default();
            if said.starts_with("HTTP/1.1 200") {
                return said;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(health.ends_with("ok"), "{health:?}");
}

#[tokio::test]
async fn the_relays_connections_are_capped_from_accept() {
    let mut limits = limits();
    limits.max_connections = 3;
    let relay = Relay::with_limits(limits).await;
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(half_request(relay.address).await);
    }
    let mut fourth = within(TcpStream::connect(relay.address)).await.unwrap();
    let said = ended(&mut fourth, ENDED)
        .await
        .expect("the fourth stayed open");
    assert!(said.starts_with("HTTP/1.1 503"), "{said:?}");
}

#[tokio::test]
async fn a_stopping_relay_drops_what_is_still_open_after_its_grace() {
    let mut limits = limits();
    limits.header_read_timeout = Duration::from_secs(600);
    limits.shutdown_grace = Duration::from_millis(300);
    let relay = Relay::with_limits(limits).await;
    let mut held = Vec::new();
    for _ in 0..10 {
        held.push(half_request(relay.address).await);
    }
    let keypair = NodeKeypair::generate().unwrap();
    let _registered = RawNode::register(&relay.url, &keypair).await;
    assert!(
        timeout(ENDED, relay.stop()).await.is_ok(),
        "the relay did not stop"
    );
    for stream in &mut held {
        assert!(
            ended(stream, ENDED).await.is_some(),
            "still open after the stop"
        );
    }
}

#[tokio::test]
async fn a_registering_connection_has_seconds_for_its_first_message() {
    let mut limits = limits();
    limits.first_message_timeout = Duration::from_millis(300);
    limits.registration_timeout = Duration::from_secs(600);
    let relay = Relay::with_limits(limits).await;
    let mut silent = raw(&node_url(&relay)).await;
    assert!(
        timeout(ENDED, close_code(&mut silent)).await.is_ok(),
        "a silent registration was kept past its first-message deadline"
    );
}

#[tokio::test]
async fn registering_places_are_counted_per_address() {
    let mut limits = limits();
    limits.max_registering_per_address = 2;
    limits.first_message_timeout = Duration::from_secs(600);
    limits.registration_timeout = Duration::from_secs(600);
    let relay = Relay::with_config(limits, Some(HEADER)).await;
    let url = node_url(&relay);
    let _one = raw_from(&url, HEADER, "192.0.2.1").await;
    let _two = raw_from(&url, HEADER, "192.0.2.1").await;
    let mut three = raw_from(&url, HEADER, "192.0.2.1").await;
    assert_eq!(close_code(&mut three).await, Some(RelayClose::Full.code()));
    // Another address still registers.
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
}

#[tokio::test]
async fn a_node_registered_before_gets_back_in_while_strangers_hold_the_rest() {
    let mut limits = limits();
    limits.max_registering = 3;
    limits.reserved_registering = 1;
    limits.first_message_timeout = Duration::from_secs(600);
    limits.registration_timeout = Duration::from_secs(600);
    let relay = Relay::with_config(limits, Some(HEADER)).await;
    let url = node_url(&relay);
    // A node registers, then leaves: the relay knows its key.
    let known = NodeKeypair::generate().unwrap();
    drop(RawNode::register(&relay.url, &known).await);
    // Strangers take every place but the one kept for known nodes.
    let _first = raw_from(&url, HEADER, "192.0.2.1").await;
    let _second = raw_from(&url, HEADER, "192.0.2.2").await;
    // A node the relay never saw is refused once it names its key...
    let mut stranger = raw_from(&url, HEADER, "192.0.2.3").await;
    let unknown = NodeKeypair::generate().unwrap();
    assert_eq!(
        register_over(&mut stranger, &unknown).await,
        RelayToNode::Rejected {
            reason: RegistrationRefusal::Full
        }
    );
    // ...and the known one gets back in.
    let mut returning = raw_from(&url, HEADER, "192.0.2.4").await;
    assert_eq!(
        register_over(&mut returning, &known).await,
        RelayToNode::Registered
    );
}

#[tokio::test]
async fn client_places_are_counted_per_address() {
    let mut limits = limits();
    limits.max_clients_per_address = 2;
    limits.first_message_timeout = Duration::from_secs(600);
    let relay = Relay::with_config(limits, Some(HEADER)).await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let url = connect_url(&relay.url, &node.routing_id());
    let _one = raw_from(&url, HEADER, "192.0.2.1").await;
    let _two = raw_from(&url, HEADER, "192.0.2.1").await;
    let mut three = raw_from(&url, HEADER, "192.0.2.1").await;
    assert_eq!(close_code(&mut three).await, Some(RelayClose::Full.code()));
    // A client from another address reaches the node.
    let mut channel = channel(&relay.url, &node).await;
    within(futures_util::SinkExt::send(
        &mut channel,
        "hello".to_owned(),
    ))
    .await
    .unwrap();
    let echoed = within(futures_util::StreamExt::next(&mut channel)).await;
    assert_eq!(echoed.unwrap().unwrap(), "hello");
}

#[tokio::test]
async fn a_spliced_client_has_seconds_for_its_first_record() {
    let mut limits = limits();
    limits.first_message_timeout = Duration::from_millis(300);
    let relay = Relay::with_limits(limits).await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let mut client = raw(&connect_url(&relay.url, &routing_id(keypair.public()))).await;
    let conn = node.incoming().await;
    let _data = node.accept(conn).await;
    assert!(
        timeout(ENDED, close_code(&mut client)).await.is_ok(),
        "a client that sent nothing was kept past its first-record deadline"
    );
}
