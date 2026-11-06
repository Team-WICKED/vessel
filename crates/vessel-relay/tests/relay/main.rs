//! The relay, end to end on loopback: registration and its proof, pairing
//! and splicing, the limits and close codes, the node's handshake close,
//! a relay restart under a registered node, and a node's own `/secure`
//! (`direct`).
//!
//! Tests wait on events (a link state, a message, a close); the timeouts
//! under test are shortened through `Limits`, never slept on.

#[cfg(test)]
mod direct;
#[cfg(test)]
mod support;
#[cfg(test)]
mod tamper;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::Message;
use vessel_proto::{
    HandshakeMessage, NodeClose, NodeToRelay, RegistrationRefusal, RelayClose, RelayToNode,
    RelayToken, RoutingId,
};
use vessel_relay::{Limits, LinkState, NodeLink, dial, node_close, relay_close};
use vessel_secure::{NodeKeypair, SecureError, initiate, routing_id};

use crate::support::{
    EchoNode, Forwarder, RawNode, Relay, channel, close_code, connect_url, raw, receive_json,
    send_json, tls, token, within,
};

#[tokio::test]
async fn a_client_reaches_a_registered_node_through_the_relay() {
    let relay = Relay::start().await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let mut channel = channel(&relay.url, &node).await;
    for frame in ["hello", "", &"x".repeat(200_000)] {
        within(channel.send(frame.to_owned())).await.unwrap();
        assert!(within(channel.next()).await.unwrap().unwrap() == frame);
    }
}

#[tokio::test]
async fn health_answers() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let relay = Relay::start().await;
    let mut stream = tokio::net::TcpStream::connect(relay.address).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = String::new();
    let _read = within(stream.read_to_string(&mut answer)).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("ok"), "{answer}");
}

#[tokio::test]
async fn the_relay_never_sees_plaintext() {
    let relay = Relay::start().await;
    let tap = Forwarder::start(relay.address).await;
    // Both the node and the client reach the relay through the tap.
    let mut node = EchoNode::start(&tap.url);
    node.wait_for(LinkState::Registered).await;
    let mut channel = channel(&tap.url, &node).await;
    let marker = "plaintext-marker-6f1c2d";
    within(channel.send(format!("{marker} {marker}")))
        .await
        .unwrap();
    let echoed = within(channel.next()).await.unwrap().unwrap();
    assert!(echoed.contains(marker));
    let captured = tap.captured();
    assert!(captured.len() > 2 * marker.len(), "the tap saw the traffic");
    assert!(
        !captured
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()),
        "the relay's view holds the plaintext"
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused() {
    let relay = Relay::start().await;
    let mut node = EchoNode::start_with(
        &relay.url,
        Arc::new(NodeKeypair::generate().unwrap()),
        RelayToken::new("not-the-token").unwrap(),
    );
    node.wait_for(LinkState::Refused(RegistrationRefusal::Token))
        .await;
}

#[tokio::test]
async fn an_impostor_cannot_register_someone_elses_key() {
    let relay = Relay::start().await;
    let mut victim = EchoNode::start(&relay.url);
    victim.wait_for(LinkState::Registered).await;
    let key = *victim.keypair.public();
    let control = format!("{}{}", relay.url, vessel_relay::NODE_PATH);

    // Claiming the victim's key: the challenge cannot be answered.
    let mut impostor = raw(&control).await;
    send_json(
        &mut impostor,
        &NodeToRelay::Register {
            routing_id: routing_id(&key),
            key,
            token: token(),
        },
    )
    .await;
    let RelayToNode::Prove { message: _ } = receive_json(&mut impostor).await else {
        panic!("expected a challenge");
    };
    let guess = HandshakeMessage::new(vec![0x42; 48]).unwrap();
    send_json(&mut impostor, &NodeToRelay::Proof { message: guess }).await;
    assert_eq!(
        receive_json(&mut impostor).await,
        RelayToNode::Rejected {
            reason: RegistrationRefusal::Proof
        }
    );

    // Claiming the victim's routing id with its own key: refused at once.
    let own = NodeKeypair::generate().unwrap();
    let mut impostor = raw(&control).await;
    send_json(
        &mut impostor,
        &NodeToRelay::Register {
            routing_id: routing_id(&key),
            key: *own.public(),
            token: token(),
        },
    )
    .await;
    assert_eq!(
        receive_json(&mut impostor).await,
        RelayToNode::Rejected {
            reason: RegistrationRefusal::Proof
        }
    );

    // The victim is still the one clients reach.
    let mut channel = channel(&relay.url, &victim).await;
    within(channel.send("still me".to_owned())).await.unwrap();
    assert_eq!(within(channel.next()).await.unwrap().unwrap(), "still me");
}

#[tokio::test]
async fn an_unknown_node_closes_with_4404() {
    let relay = Relay::start().await;
    let mut carrier = within(dial(&relay.url, &RoutingId::from_bytes([7; 16]), &tls()))
        .await
        .unwrap();
    let error = within(carrier.next()).await.unwrap().unwrap_err();
    assert_eq!(relay_close(&error), Some(RelayClose::NoSuchNode));
}

#[tokio::test]
async fn waiting_clients_are_capped_and_time_out_with_4408() {
    let mut limits = Limits::default();
    limits.waiting_per_node = 2;
    limits.accept_timeout = Duration::from_millis(300);
    let relay = Relay::with_limits(limits).await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let url = connect_url(&relay.url, &routing_id(keypair.public()));

    // Two clients wait: the node hears of both and accepts neither.
    let mut first = raw(&url).await;
    let mut second = raw(&url).await;
    let _first_conn = node.incoming().await;
    let _second_conn = node.incoming().await;
    // A third is one too many.
    let mut third = raw(&url).await;
    assert_eq!(
        close_code(&mut third).await,
        Some(RelayClose::RateLimited.code())
    );
    // The two waiting ones give up after the accept timeout.
    assert_eq!(
        close_code(&mut first).await,
        Some(RelayClose::NotAccepted.code())
    );
    assert_eq!(
        close_code(&mut second).await,
        Some(RelayClose::NotAccepted.code())
    );
}

#[tokio::test]
async fn connections_are_rate_limited_per_address() {
    let mut limits = Limits::default();
    limits.connect_burst = 2;
    limits.connect_refill = Duration::from_secs(3600);
    let relay = Relay::with_limits(limits).await;
    let url = connect_url(&relay.url, &RoutingId::from_bytes([1; 16]));
    for _ in 0..2 {
        let mut client = raw(&url).await;
        assert_eq!(
            close_code(&mut client).await,
            Some(RelayClose::NoSuchNode.code())
        );
    }
    let mut client = raw(&url).await;
    assert_eq!(
        close_code(&mut client).await,
        Some(RelayClose::RateLimited.code())
    );
}

#[tokio::test]
async fn a_client_is_not_read_before_pairing_and_nothing_is_lost() {
    let relay = Relay::start().await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let mut client = raw(&connect_url(&relay.url, &routing_id(keypair.public()))).await;
    let conn = node.incoming().await;
    // The client speaks first, while the node has not accepted yet.
    for index in 0..3_u8 {
        client
            .send(Message::Binary(Bytes::from(vec![index; 1000])))
            .await
            .unwrap();
    }
    let mut accepted = node.accept(conn).await;
    for index in 0..3_u8 {
        match within(accepted.next()).await {
            Some(Ok(Message::Binary(record))) => assert_eq!(record, vec![index; 1000]),
            other => panic!("expected record {index}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_message_over_one_record_closes_with_4413() {
    let relay = Relay::start().await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let mut client = raw(&connect_url(&relay.url, &routing_id(keypair.public()))).await;
    let conn = node.incoming().await;
    let mut accepted = node.accept(conn).await;
    client
        .send(Message::Binary(Bytes::from(vec![0; 65_536])))
        .await
        .unwrap();
    assert_eq!(
        close_code(&mut client).await,
        Some(RelayClose::TooLarge.code())
    );
    // The node's side is closed too, without the relay's code.
    assert_eq!(close_code(&mut accepted).await, None);
}

#[tokio::test]
async fn a_node_refusing_the_handshake_reaches_the_client_as_4401() {
    let relay = Relay::start().await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let carrier = within(dial(&relay.url, &node.routing_id(), &tls()))
        .await
        .unwrap();
    // The client pinned another key.
    let other = NodeKeypair::generate().unwrap();
    let error = within(initiate(carrier, other.public())).await.unwrap_err();
    let SecureError::Carrier(io) = &error else {
        panic!("expected the carrier to report the node's close, got {error:?}");
    };
    assert_eq!(node_close(io), Some(NodeClose::HandshakeFailed));
    assert!(error.is_transient(), "transient as a carrier error...");
    assert_eq!(
        relay_close(io),
        None,
        "...but the node's code says otherwise"
    );
}

#[tokio::test]
async fn a_silent_control_connection_is_dropped() {
    let mut limits = Limits::default();
    limits.idle_timeout = Duration::from_millis(300);
    limits.ping_interval = Duration::from_secs(60);
    let relay = Relay::with_limits(limits).await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    // The node says nothing and nobody pings: the relay drops it.
    assert_eq!(close_code(&mut node.control).await, None);
    let mut client = raw(&connect_url(&relay.url, &routing_id(keypair.public()))).await;
    assert_eq!(
        close_code(&mut client).await,
        Some(RelayClose::NoSuchNode.code())
    );
}

#[tokio::test]
async fn a_new_registration_replaces_the_old_one() {
    let relay = Relay::start().await;
    let keypair = Arc::new(NodeKeypair::generate().unwrap());
    let mut old = RawNode::register(&relay.url, &keypair).await;
    let mut node = EchoNode::start_with(&relay.url, Arc::clone(&keypair), token());
    node.wait_for(LinkState::Registered).await;
    assert_eq!(close_code(&mut old.control).await, None);
    let mut channel = channel(&relay.url, &node).await;
    within(channel.send("new".to_owned())).await.unwrap();
    assert_eq!(within(channel.next()).await.unwrap().unwrap(), "new");
}

#[tokio::test]
async fn a_node_registers_again_after_a_relay_restart() {
    let first = Relay::start().await;
    let front = Forwarder::start(first.address).await;
    let mut node = EchoNode::start(&front.url);
    node.wait_for(LinkState::Registered).await;

    // The relay goes away and another takes its address.
    let second = Relay::start().await;
    front.redirect(second.address);
    first.stop().await;
    node.wait_for(LinkState::Connecting).await;
    node.wait_for(LinkState::Registered).await;

    let mut channel = channel(&front.url, &node).await;
    within(channel.send("back".to_owned())).await.unwrap();
    assert_eq!(within(channel.next()).await.unwrap().unwrap(), "back");
}

#[tokio::test]
async fn a_link_stops_on_shutdown() {
    let relay = Relay::start().await;
    let link = NodeLink::new(
        relay.url.clone(),
        Arc::new(NodeKeypair::generate().unwrap()),
        token(),
        tls(),
    );
    let mut state = link.state();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(link.run(|_carrier| {}, shutdown.clone()));
    let registered = within(state.wait_for(|state| *state == LinkState::Registered))
        .await
        .unwrap();
    drop(registered);
    shutdown.cancel();
    within(task).await.unwrap();
    assert_eq!(*state.borrow(), LinkState::Stopped);
}
