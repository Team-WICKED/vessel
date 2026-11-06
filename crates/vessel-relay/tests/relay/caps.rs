//! The relay's resource caps (`Limits`, the security review's round 6):
//! registered nodes, control connections still registering, relayed
//! clients per node and in all, the byte rate of a relayed stream, and the
//! idle timeout of one. Each cap refuses with a reason the peer can read
//! (`rejected {reason: "full"}`, close `4503`), and a place frees when its
//! holder goes.

use std::io;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::time::Instant;
use vessel_proto::{NodeToRelay, RegistrationRefusal, RelayClose, RelayToNode, RoutingId};
use vessel_relay::{Limits, LinkState, WsCarrier, dial, relay_close};
use vessel_secure::{NodeKeypair, SecureChannel, answer_proof, routing_id};

use crate::support::{
    EchoNode, RawNode, Relay, channel, close_code, connect_url, raw, receive_json, send_json, tls,
    token, within,
};

/// Default limits, except that one address may connect as often as these
/// tests do: each cap is tested alone.
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.connect_burst = 10_000;
    limits
}

/// Register `keypair` by hand and return the relay's last answer
/// (`registered` or `rejected`).
async fn try_register(relay: &Relay, keypair: &NodeKeypair) -> RelayToNode {
    let mut control = raw(&format!("{}{}", relay.url, vessel_relay::NODE_PATH)).await;
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
    receive_json(&mut control).await
}

/// Reach `node` through `relay`: the channel, or the error the relay's
/// close came as.
async fn try_channel(
    relay: &Relay,
    node: &EchoNode,
) -> Result<SecureChannel<WsCarrier>, io::Error> {
    let carrier = within(dial(&relay.url, &node.routing_id(), &tls()))
        .await
        .map_err(io::Error::other)?;
    within(vessel_secure::initiate(carrier, node.keypair.public()))
        .await
        .map_err(io::Error::other)
}

/// The relay's close code for one more client of `node`, which must be
/// refused.
async fn refused_client(relay: &Relay, node: &RoutingId) -> Option<RelayClose> {
    let mut carrier = within(dial(&relay.url, node, &tls())).await.unwrap();
    let error = within(carrier.next()).await.unwrap().unwrap_err();
    relay_close(&error)
}

/// Retry `attempt` until it gives `Some`, for a place that frees once the
/// relay noticed a connection end.
async fn eventually<T, F: Future<Output = Option<T>>>(mut attempt: impl FnMut() -> F) -> T {
    within(async {
        loop {
            if let Some(done) = attempt().await {
                return done;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

#[tokio::test]
async fn nodes_past_the_cap_are_refused_as_full_and_a_node_keeps_its_own_place() {
    let mut limits = limits();
    limits.max_nodes = 2;
    let relay = Relay::with_limits(limits).await;
    let first = NodeKeypair::generate().unwrap();
    let second = NodeKeypair::generate().unwrap();
    let _first = RawNode::register(&relay.url, &first).await;
    let second_node = RawNode::register(&relay.url, &second).await;

    // A third key is refused, and says why.
    let third = NodeKeypair::generate().unwrap();
    assert_eq!(
        try_register(&relay, &third).await,
        RelayToNode::Rejected {
            reason: RegistrationRefusal::Full
        }
    );
    // So is a node's link, which shows it in its state.
    let mut linked = EchoNode::start(&relay.url);
    linked
        .wait_for(LinkState::Refused(RegistrationRefusal::Full))
        .await;
    // A registered node registering again takes its own place back.
    assert_eq!(try_register(&relay, &first).await, RelayToNode::Registered);

    // Once a node leaves, its place is free.
    drop(second_node);
    let registered = Box::pin(eventually(|| async {
        let answer = try_register(&relay, &third).await;
        (answer == RelayToNode::Registered).then_some(answer)
    }))
    .await;
    assert_eq!(registered, RelayToNode::Registered);
}

#[tokio::test]
async fn control_connections_still_registering_are_capped() {
    let mut limits = limits();
    limits.max_registering = 1;
    let relay = Relay::with_limits(limits).await;
    let node_url = format!("{}{}", relay.url, vessel_relay::NODE_PATH);
    // One connection sits in its registration; the next is refused.
    let mut waiting = raw(&node_url).await;
    let mut refused = raw(&node_url).await;
    assert_eq!(
        close_code(&mut refused).await,
        Some(RelayClose::Full.code())
    );
    // Once it registers, the place is free for the next one.
    let keypair = NodeKeypair::generate().unwrap();
    let key = *keypair.public();
    send_json(
        &mut waiting,
        &NodeToRelay::Register {
            routing_id: routing_id(&key),
            key,
            token: token(),
        },
    )
    .await;
    let RelayToNode::Prove { message } = receive_json(&mut waiting).await else {
        panic!("expected a challenge");
    };
    let message = answer_proof(&keypair, &message).unwrap();
    send_json(&mut waiting, &NodeToRelay::Proof { message }).await;
    assert_eq!(receive_json(&mut waiting).await, RelayToNode::Registered);
    let other = NodeKeypair::generate().unwrap();
    assert_eq!(try_register(&relay, &other).await, RelayToNode::Registered);
}

#[tokio::test]
async fn clients_of_one_node_past_the_cap_close_with_4503_until_one_ends() {
    let mut limits = limits();
    limits.max_clients_per_node = 2;
    let relay = Relay::with_limits(limits).await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let mut other = EchoNode::start(&relay.url);
    other.wait_for(LinkState::Registered).await;

    let first = channel(&relay.url, &node).await;
    let mut second = channel(&relay.url, &node).await;
    assert_eq!(
        refused_client(&relay, &node.routing_id()).await,
        Some(RelayClose::Full)
    );
    // Another node's clients are not held to this one's cap.
    let mut elsewhere = channel(&relay.url, &other).await;
    within(elsewhere.send("still room".to_owned()))
        .await
        .unwrap();
    assert_eq!(
        within(elsewhere.next()).await.unwrap().unwrap(),
        "still room"
    );
    // The two keep working; once one ends, a new client gets its place.
    within(second.send("busy".to_owned())).await.unwrap();
    assert_eq!(within(second.next()).await.unwrap().unwrap(), "busy");
    drop(first);
    let mut third = Box::pin(eventually(|| async {
        try_channel(&relay, &node).await.ok()
    }))
    .await;
    within(third.send("my turn".to_owned())).await.unwrap();
    assert_eq!(within(third.next()).await.unwrap().unwrap(), "my turn");
}

#[tokio::test]
async fn clients_past_the_relays_cap_close_with_4503() {
    let mut limits = limits();
    limits.max_clients = 2;
    let relay = Relay::with_limits(limits).await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let mut other = EchoNode::start(&relay.url);
    other.wait_for(LinkState::Registered).await;

    let _one = channel(&relay.url, &node).await;
    let _two = channel(&relay.url, &other).await;
    assert_eq!(
        refused_client(&relay, &node.routing_id()).await,
        Some(RelayClose::Full)
    );
    assert_eq!(
        refused_client(&relay, &other.routing_id()).await,
        Some(RelayClose::Full)
    );
    // A client of a node that is not there is told so, not "full".
    let mut nobody = raw(&connect_url(&relay.url, &RoutingId::from_bytes([3; 16]))).await;
    assert_eq!(
        close_code(&mut nobody).await,
        Some(RelayClose::NoSuchNode.code())
    );
}

/// In real time, checked from below only: a slow machine makes it slower,
/// never faster. (Paused time would let tokio skip ahead through the
/// handshakes while loopback I/O is in flight.)
#[tokio::test]
async fn a_relayed_stream_is_held_to_its_byte_rate() {
    const RATE: u64 = 64 * 1024;
    let mut limits = limits();
    limits.max_bytes_per_second = RATE;
    let relay = Relay::with_limits(limits).await;
    let mut node = EchoNode::start(&relay.url);
    node.wait_for(LinkState::Registered).await;
    let mut channel = channel(&relay.url, &node).await;

    // Three seconds' worth: the first passes at once, the rest at the
    // rate, in each direction.
    let frame = "x".repeat(usize::try_from(3 * RATE).unwrap());
    let started = Instant::now();
    within(channel.send(frame.clone())).await.unwrap();
    let echoed = within(channel.next()).await.unwrap().unwrap();
    let took = started.elapsed();
    assert_eq!(echoed.len(), frame.len());
    assert!(took >= Duration::from_secs(2), "took {took:?}");
}

#[tokio::test]
async fn a_relayed_stream_silent_past_the_idle_timeout_is_dropped() {
    let mut limits = limits();
    // Long enough for the node's control connection, as silent, to last
    // until the client is announced.
    limits.idle_timeout = Duration::from_secs(1);
    limits.ping_interval = Duration::from_secs(3600);
    let relay = Relay::with_limits(limits).await;
    let keypair = NodeKeypair::generate().unwrap();
    let mut node = RawNode::register(&relay.url, &keypair).await;
    let mut client = raw(&connect_url(&relay.url, &routing_id(keypair.public()))).await;
    let conn = node.incoming().await;
    let mut data = node.accept(conn).await;
    // Spliced, and nobody says anything: both ends are closed.
    assert_eq!(close_code(&mut client).await, None);
    assert_eq!(close_code(&mut data).await, None);
}
