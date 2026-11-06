//! The security review's round 9: a crowd of clients through a relay, each
//! holding a Noise handshake open without a preamble, against one node
//! behind it. The relay lets one address block hold only a few of the
//! node's clients while the node admits them, and the node waits on
//! relayed channels in a budget of their own: its direct route keeps all
//! of its room.

use std::time::Duration;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use vessel_node::Node;
use vessel_proto::{NodePublicKey, RelayToken, RouteUrl};
use vessel_relay::{Limits, RelayConfig, TlsConfig};

use crate::secure::{direct_route, relayed_with};
use crate::support::{Fixture, within};

fn tls() -> TlsConfig {
    TlsConfig::with_roots(rustls::RootCertStore::empty()).unwrap()
}

/// A relay on loopback with its limits changed by `adjust`; it stops with
/// the token returned.
async fn relay_with(adjust: impl FnOnce(&mut RelayConfig)) -> (RouteUrl, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: RouteUrl = format!("ws://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let mut config = RelayConfig::new(RelayToken::new("e2e-relay-token").unwrap());
    // Every client here comes from one address: let it connect as often as
    // it likes, so only the limits under test hold it back.
    config.limits.connect_burst = 10_000;
    adjust(&mut config);
    let stop = CancellationToken::new();
    let serving = stop.clone();
    drop(tokio::spawn(async move {
        vessel_relay::serve(listener, config, serving)
            .await
            .unwrap();
    }));
    (url, stop)
}

/// Of `n` tries, the channels to the node of `key` through the relay at
/// `url` held open with their handshake done and no preamble.
async fn hold(url: &RouteUrl, key: &NodePublicKey, n: usize) -> Vec<impl Sized> {
    let routing = vessel_secure::routing_id(key);
    let mut held = Vec::new();
    for _ in 0..n {
        let Ok(carrier) = vessel_relay::dial(url, &routing, &tls()).await else {
            continue;
        };
        let handshake = vessel_secure::initiate(carrier, key);
        if let Ok(Ok(channel)) = tokio::time::timeout(Duration::from_secs(3), handshake).await {
            held.push(channel);
        }
    }
    held
}

/// Whether a client's handshake with `node` over its direct route works.
async fn direct_handshake(node: &Node, key: &NodePublicKey) -> bool {
    let direct = format!("{}/secure", direct_route(node));
    let Ok(carrier) = vessel_relay::connect(&direct, &tls()).await else {
        return false;
    };
    let handshake = vessel_secure::initiate(carrier, key);
    matches!(
        tokio::time::timeout(Duration::from_secs(3), handshake).await,
        Ok(Ok(_))
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crowd_through_a_relay_takes_few_of_a_nodes_places_and_none_of_its_direct_ones() {
    within(async {
        // As the review had it: one address with a /48's share of clients.
        let (url, stop) = relay_with(|config| {
            config.limits.max_clients_per_address = 64;
            config.limits.max_connections_per_address = 72;
            config.check_limits = false;
        })
        .await;
        let fixture = Fixture::new();
        let node = relayed_with(&fixture, &[&url], |_config| {}).await;
        let key = node.info().public_key.unwrap();
        // A few more tries than the cap, quickly: well within the node's
        // `RELAYED_ADMISSION`, so none held has been dropped yet.
        let cap = Limits::default().max_node_clients_per_address;
        let held = hold(&url, &key, cap.saturating_mul(3)).await;
        assert!(held.len() <= cap, "{} held through the relay", held.len());
        assert!(direct_handshake(&node, &key).await, "the direct route");
        drop(held);
        node.stop().await.unwrap();
        stop.cancel();

        // A relay that lets more through (one from before the cap, or set
        // otherwise): the node's own budget for relayed channels holds, and
        // the direct route keeps its room.
        let (url, stop) = relay_with(|config| {
            config.limits.max_clients_per_address = 64;
            config.limits.max_connections_per_address = 72;
            config.limits.max_node_clients_per_address = 64;
            config.check_limits = false;
        })
        .await;
        let fixture = Fixture::new();
        let node = relayed_with(&fixture, &[&url], |_config| {}).await;
        let key = node.info().public_key.unwrap();
        let held = hold(
            &url,
            &key,
            vessel_relay::RELAYED_UNADMITTED.saturating_add(8),
        )
        .await;
        assert!(
            held.len() <= vessel_relay::RELAYED_UNADMITTED,
            "{} held through the relay",
            held.len()
        );
        assert!(direct_handshake(&node, &key).await, "the direct route");
        drop(held);
        node.stop().await.unwrap();
        stop.cancel();
    })
    .await;
}
