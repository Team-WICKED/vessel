//! The security review's round 9: clients a relay splices to a node before
//! the node has admitted them. One IPv6 `/48` holds only a few of one
//! node's clients while they are being admitted; a client gives its rate
//! token back only once its node kept it past admission; and a trusted
//! proxy's address header is taken only in the one form documented, a
//! missing one counting as the proxy itself. Client addresses are stood
//! for by `X-Real-IP` from a trusted loopback proxy (`Relay::with_config`).

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use vessel_proto::{RelayClose, RoutingId};
use vessel_relay::Limits;
use vessel_secure::{NodeKeypair, routing_id};

use crate::support::{RawNode, RawSocket, Relay, close_code, connect_url, raw_from, within};

const HEADER: &str = "x-real-ip";

/// Long enough for anything the relay should do at once.
const SOON: Duration = Duration::from_secs(3);

/// Default limits, except that clients wait for their node as long as a
/// test needs and one address may connect as often as it likes.
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.connect_burst = 10_000;
    limits.accept_timeout = Duration::from_secs(600);
    limits.first_message_timeout = Duration::from_secs(600);
    limits
}

/// Whether the relay closed `client` as full within [`SOON`].
async fn refused_full(client: &mut RawSocket) -> bool {
    timeout(SOON, close_code(client)).await.ok().flatten() == Some(RelayClose::Full.code())
}

/// A client of `node` from `address`, waiting for the node (not refused).
async fn waiting(relay: &Relay, node: &RoutingId, address: &str) -> RawSocket {
    raw_from(&connect_url(&relay.url, node), HEADER, address).await
}

#[tokio::test]
async fn one_block_holds_a_few_of_a_nodes_clients_while_they_are_admitted() {
    let relay = Relay::with_config(limits(), Some(HEADER)).await;
    let keypair = NodeKeypair::generate().unwrap();
    let _node = RawNode::register(&relay.url, &keypair).await;
    let node = routing_id(keypair.public());
    let mut held = Vec::new();
    // Four from one /64 (the default `max_node_clients_per_address`)...
    for _ in 0..4 {
        held.push(waiting(&relay, &node, "2001:db8:5:100::1").await);
    }
    // ...and not a fifth, though the address may have 16 clients in all.
    let mut fifth = waiting(&relay, &node, "2001:db8:5:100::2").await;
    assert!(refused_full(&mut fifth).await, "a fifth from one /64");
    // A /48 holds four times that, sixteen, over its /64s and /56s.
    for from in [
        "2001:db8:5:101::1",
        "2001:db8:5:200::1",
        "2001:db8:5:201::1",
    ] {
        for _ in 0..4 {
            held.push(waiting(&relay, &node, from).await);
        }
    }
    let mut more = waiting(&relay, &node, "2001:db8:5:300::1").await;
    assert!(refused_full(&mut more).await, "a seventeenth from one /48");
    // Another node is another count; another /48 still reaches this one.
    let other = NodeKeypair::generate().unwrap();
    let _other = RawNode::register(&relay.url, &other).await;
    let mut elsewhere = waiting(&relay, &routing_id(other.public()), "2001:db8:5:300::1").await;
    assert!(!refused_full(&mut elsewhere).await);
    let mut stranger = waiting(&relay, &node, "2001:db8:6::1").await;
    assert!(!refused_full(&mut stranger).await);
}

/// A client of `node` from `address`, spliced with the node's data
/// connection once a record went through.
async fn spliced(
    relay: &Relay,
    node: &mut RawNode,
    routing: &RoutingId,
    address: &str,
) -> (RawSocket, RawSocket) {
    let mut client = waiting(relay, routing, address).await;
    let conn = node.incoming().await;
    let mut data = node.accept(conn).await;
    within(client.send(Message::Binary(Bytes::from_static(b"hello"))))
        .await
        .unwrap();
    match within(data.next()).await {
        Some(Ok(Message::Binary(record))) => assert_eq!(&record[..], b"hello"),
        other => panic!("expected the client's record, got {other:?}"),
    }
    (client, data)
}

/// Rate limits for these tests: two new connections an address, never
/// refilled, and clients settled after a short while.
fn rate_limits() -> Limits {
    let mut limits = limits();
    limits.connect_burst = 2;
    limits.connect_refill = Duration::from_secs(3600);
    limits.client_settle = Duration::from_millis(300);
    limits
}

#[tokio::test]
async fn a_client_closed_before_its_node_kept_it_spends_its_token() {
    let mut limits = rate_limits();
    // Far past anything this test waits for, however slow the machine.
    limits.client_settle = Duration::from_secs(600);
    let relay = Relay::with_config(limits, Some(HEADER)).await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let routing = routing_id(keypair.public());
    // Spliced, a record through, then gone before it settled: the node
    // never admitted it, as far as the relay can tell.
    for _ in 0..2 {
        drop(spliced(&relay, &mut node, &routing, "192.0.2.9").await);
    }
    let mut third = waiting(&relay, &routing, "192.0.2.9").await;
    assert_eq!(
        timeout(SOON, close_code(&mut third)).await.ok().flatten(),
        Some(RelayClose::RateLimited.code())
    );
}

#[tokio::test]
async fn a_client_its_node_kept_gives_its_token_back() {
    let relay = Relay::with_config(rate_limits(), Some(HEADER)).await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let routing = routing_id(keypair.public());
    let mut kept = Vec::new();
    for _ in 0..4 {
        kept.push(spliced(&relay, &mut node, &routing, "192.0.2.9").await);
        // Well past `client_settle`, still open: admitted.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The status line the relay answers a websocket upgrade to `path` with,
/// sending `headers` (name, value), each on a line of its own.
async fn status(address: SocketAddr, path: &str, headers: &[(&str, &str)]) -> String {
    let mut stream = within(TcpStream::connect(address)).await.unwrap();
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: relay\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
    );
    for (name, value) in headers {
        let _written = writeln!(request, "{name}: {value}\r");
    }
    request.push_str("\r\n");
    within(stream.write_all(request.as_bytes())).await.unwrap();
    let mut line = String::new();
    let _read = within(BufReader::new(stream).read_line(&mut line))
        .await
        .unwrap();
    line.trim_end().to_owned()
}

#[tokio::test]
async fn a_trusted_proxys_header_is_one_bare_address() {
    let relay = Relay::with_config(limits(), Some(HEADER)).await;
    let path = format!(
        "{}?node={}",
        vessel_relay::CONNECT_PATH,
        RoutingId::from_bytes([1; 16])
    );
    for good in [
        "203.0.113.1",
        "2001:db8::1",
        " 2001:db8::2 ",
        "::ffff:192.0.2.1",
    ] {
        let answer = status(relay.address, &path, &[(HEADER, good)]).await;
        assert!(answer.starts_with("HTTP/1.1 101"), "{good:?}: {answer}");
    }
    for bad in [
        "203.0.113.1, 10.0.0.1",
        "[2001:db8::1]:443",
        "203.0.113.1:443",
        "[2001:db8::1]",
        "unknown",
        "",
    ] {
        let answer = status(relay.address, &path, &[(HEADER, bad)]).await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{bad:?}: {answer}");
    }
    // Two of them: which one the proxy wrote is anyone's guess.
    let answer = status(
        relay.address,
        &path,
        &[(HEADER, "203.0.113.1"), (HEADER, "203.0.113.2")],
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 400"), "two headers: {answer}");
}

#[tokio::test]
async fn a_trusted_proxys_request_without_the_header_is_charged_to_the_proxy() {
    let mut limits = limits();
    limits.connect_burst = 2;
    limits.connect_refill = Duration::from_secs(3600);
    let relay = Relay::with_config(limits, Some(HEADER)).await;
    let path = format!(
        "{}?node={}",
        vessel_relay::CONNECT_PATH,
        RoutingId::from_bytes([1; 16])
    );
    let url = format!("{}{path}", relay.url);
    for _ in 0..2 {
        let mut client = within(Box::pin(tokio_tungstenite::connect_async(&url)))
            .await
            .unwrap()
            .0;
        assert_eq!(
            close_code(&mut client).await,
            Some(RelayClose::NoSuchNode.code())
        );
    }
    let mut third = within(Box::pin(tokio_tungstenite::connect_async(&url)))
        .await
        .unwrap()
        .0;
    assert_eq!(
        close_code(&mut third).await,
        Some(RelayClose::RateLimited.code())
    );
}
