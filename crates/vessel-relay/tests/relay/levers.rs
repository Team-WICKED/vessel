//! The security review's round 8: limits that one host, or one IPv6 `/48`,
//! could still turn against everyone. New connections from one address are
//! rate-limited from `accept`, and sockets that have not authenticated fill
//! only their share of the relay; IPv6 counts by its `/64`, `/56` and `/48`;
//! a place kept for known nodes is taken only by a node that names itself
//! as one; and the client-address header is believed only from a trusted
//! proxy. Client addresses are stood for by `X-Real-IP` from a trusted
//! loopback proxy (`Relay::with_config`), so the tests run everywhere.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use vessel_proto::{NodeToRelay, RegistrationRefusal, RelayClose, RelayToNode};
use vessel_relay::{Limits, LinkState, RelayConfig};
use vessel_secure::{NodeKeypair, answer_proof, routing_id};

use crate::support::{
    EchoNode, RawNode, RawSocket, Relay, close_code, connect_url, raw_from, receive_json,
    send_json, token, within,
};

const HEADER: &str = "x-real-ip";

/// Long enough for anything the relay should do at once.
const SOON: Duration = Duration::from_secs(3);

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
/// `wait`.
async fn ended(stream: &mut TcpStream, wait: Duration) -> Option<String> {
    let mut said = Vec::new();
    let read = timeout(wait, stream.read_to_end(&mut said)).await.ok()?;
    if read.is_err() {
        said.clear();
    }
    Some(String::from_utf8_lossy(&said).into_owned())
}

/// Whether a new connection is refused at `accept` (`503` at once).
async fn refused_at_accept(address: SocketAddr) -> bool {
    let mut stream = within(TcpStream::connect(address)).await.unwrap();
    ended(&mut stream, SOON)
        .await
        .is_some_and(|said| said.starts_with("HTTP/1.1 503"))
}

/// The relay's last answer to a registration of `keypair` by hand over
/// `control`.
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
async fn new_connections_from_one_address_are_rate_limited_from_accept() {
    let mut limits = Limits::default();
    limits.connect_burst = 3;
    limits.connect_refill = Duration::from_secs(3600);
    limits.header_read_timeout = Duration::from_secs(600);
    let relay = Relay::with_limits(limits).await;
    // Three that never authenticate spend the burst, even once gone.
    for _ in 0..3 {
        drop(half_request(relay.address).await);
    }
    assert!(refused_at_accept(relay.address).await);
}

#[tokio::test]
async fn a_socket_that_authenticates_gives_its_rate_back() {
    let mut limits = Limits::default();
    limits.connect_burst = 2;
    limits.connect_refill = Duration::from_secs(3600);
    let relay = Relay::with_limits(limits).await;
    // Registered nodes have authenticated: they cost the rate nothing.
    let mut nodes = Vec::new();
    for _ in 0..4 {
        nodes.push(RawNode::register(&relay.url, &NodeKeypair::generate().unwrap()).await);
    }
}

#[tokio::test]
async fn unauthenticated_sockets_fill_only_their_share() {
    let mut limits = limits();
    limits.max_connections = 8;
    limits.max_unauthenticated = 2;
    limits.header_read_timeout = Duration::from_secs(600);
    let relay = Relay::with_limits(limits).await;
    // Nodes register one after another, each giving back its place among
    // the unauthenticated once registered.
    let mut nodes = Vec::new();
    for _ in 0..3 {
        nodes.push(RawNode::register(&relay.url, &NodeKeypair::generate().unwrap()).await);
    }
    // Two strangers fill the share; a third is refused, though the relay
    // has room in all.
    let _first = half_request(relay.address).await;
    let _second = half_request(relay.address).await;
    assert!(refused_at_accept(relay.address).await);
}

#[tokio::test]
async fn ipv6_counts_by_its_64_56_and_48() {
    let mut clients = limits();
    clients.max_clients_per_address = 1;
    clients.first_message_timeout = Duration::from_secs(600);
    let relay = Relay::with_config(clients, Some(HEADER)).await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let url = connect_url(&relay.url, &node.routing_id());
    let mut held = Vec::new();
    // A /48 holds four times the limit: one from each of four /56s...
    for from in [
        "2001:db8:5:100::1",
        "2001:db8:5:200::1",
        "2001:db8:5:300::1",
        "2001:db8:5:400::1",
    ] {
        held.push(raw_from(&url, HEADER, from).await);
    }
    // ...and a fifth /56 of it is refused.
    let mut fifth = raw_from(&url, HEADER, "2001:db8:5:500::1").await;
    assert_eq!(close_code(&mut fifth).await, Some(RelayClose::Full.code()));
    // A /56 holds twice the limit.
    held.push(raw_from(&url, HEADER, "2001:db8:6:101::1").await);
    held.push(raw_from(&url, HEADER, "2001:db8:6:102::1").await);
    let mut third = raw_from(&url, HEADER, "2001:db8:6:103::1").await;
    assert_eq!(close_code(&mut third).await, Some(RelayClose::Full.code()));

    // Registering places count the same way.
    let mut registering_limits = limits();
    registering_limits.max_registering_per_address = 1;
    registering_limits.first_message_timeout = Duration::from_secs(600);
    registering_limits.registration_timeout = Duration::from_secs(600);
    let relay = Relay::with_config(registering_limits, Some(HEADER)).await;
    let node_url = node_url(&relay);
    let mut registering = Vec::new();
    for from in [
        "2001:db8:7:100::1",
        "2001:db8:7:200::1",
        "2001:db8:7:300::1",
        "2001:db8:7:400::1",
    ] {
        registering.push(raw_from(&node_url, HEADER, from).await);
    }
    let mut fifth = raw_from(&node_url, HEADER, "2001:db8:7:500::1").await;
    assert_eq!(close_code(&mut fifth).await, Some(RelayClose::Full.code()));
}

#[tokio::test]
async fn a_known_node_gets_back_in_while_silent_strangers_hold_every_open_place() {
    let mut limits = limits();
    limits.max_registering = 4;
    limits.reserved_registering = 2;
    limits.first_message_timeout = Duration::from_secs(600);
    limits.registration_timeout = Duration::from_secs(600);
    let relay = Relay::with_config(limits, Some(HEADER)).await;
    let url = node_url(&relay);
    let known = NodeKeypair::generate().unwrap();
    drop(RawNode::register(&relay.url, &known).await);
    // Silent strangers, more than there are places of either kind: they
    // take the open places and wait for nothing else.
    let mut strangers = Vec::new();
    for index in 1..=6 {
        strangers.push(raw_from(&url, HEADER, &format!("192.0.2.{index}")).await);
    }
    // The known node gets in at once, through a kept place.
    let mut returning = raw_from(&url, HEADER, "198.51.100.1").await;
    let answer = timeout(SOON, register_over(&mut returning, &known)).await;
    assert_eq!(answer.ok(), Some(RelayToNode::Registered));
    // A node new here, with no open place, is refused once it names itself.
    let mut new = raw_from(&url, HEADER, "198.51.100.2").await;
    let answer = timeout(
        SOON,
        register_over(&mut new, &NodeKeypair::generate().unwrap()),
    )
    .await;
    assert_eq!(
        answer.ok(),
        Some(RelayToNode::Rejected {
            reason: RegistrationRefusal::Full
        })
    );
}

#[tokio::test]
async fn the_address_header_is_believed_only_from_a_trusted_proxy() {
    let mut limits = limits();
    limits.max_clients_per_address = 2;
    limits.first_message_timeout = Duration::from_secs(600);
    // The proxy is elsewhere: this machine is just a client.
    let relay = Relay::behind(limits, Some(HEADER), &["192.0.2.250"]).await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let url = connect_url(&relay.url, &node.routing_id());
    // Each claims another address; all are counted as this one.
    let _one = raw_from(&url, HEADER, "203.0.113.1").await;
    let _two = raw_from(&url, HEADER, "203.0.113.2").await;
    let mut three = raw_from(&url, HEADER, "203.0.113.3").await;
    assert_eq!(close_code(&mut three).await, Some(RelayClose::Full.code()));
}

#[tokio::test]
async fn a_relay_with_an_address_header_and_no_trusted_proxy_does_not_start() {
    let mut config = RelayConfig::new(token());
    config.client_address_header = Some(HEADER.parse().unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let served = timeout(
        SOON,
        vessel_relay::serve(listener, config, CancellationToken::new()),
    )
    .await;
    let error = served.expect("it kept serving").unwrap_err();
    assert!(error.to_string().contains("trusted"), "{error}");
}
