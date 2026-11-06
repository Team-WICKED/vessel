//! The security review's rounds 9 and 10: crowds of clients, each holding
//! a Noise handshake open without a preamble, against one node. The relay
//! lets one address block hold only a few of the node's clients while the
//! node admits them; the node waits on each relay link's channels in a
//! budget of the link's own, apart from the direct route's, and on direct
//! channels a few per address.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
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

/// The header the proxied relays here read a client's address from.
const HEADER: &str = "x-real-ip";

/// A relay behind a trusted proxy on loopback, at its default limits: a
/// client says where it comes from in [`HEADER`].
async fn proxied_relay() -> (RouteUrl, CancellationToken) {
    relay_with(|config| {
        config.limits = Limits::default();
        config.client_address_header = Some(HEADER.parse().unwrap());
        config.trusted_proxies = vec!["127.0.0.1".parse().unwrap()];
    })
    .await
}

/// A relayed websocket as a carrier: records are binary messages.
struct Proxied(WebSocketStream<MaybeTlsStream<TcpStream>>);

impl Stream for Proxied {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let next = match Pin::new(&mut self.0).poll_next(cx) {
                Poll::Ready(next) => next,
                Poll::Pending => return Poll::Pending,
            };
            return Poll::Ready(match next {
                Some(Ok(Message::Binary(record))) => Some(Ok(record)),
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Text(_))) => Some(Err(io::Error::other("text from a relay"))),
                Some(Ok(Message::Close(_))) | None => None,
                Some(Err(error)) => Some(Err(io::Error::other(error))),
            });
        }
    }
}

impl Sink<Bytes> for Proxied {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0)
            .poll_ready(cx)
            .map_err(io::Error::other)
    }

    fn start_send(mut self: Pin<&mut Self>, record: Bytes) -> io::Result<()> {
        Pin::new(&mut self.0)
            .start_send(Message::Binary(record))
            .map_err(io::Error::other)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0)
            .poll_flush(cx)
            .map_err(io::Error::other)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0)
            .poll_close(cx)
            .map_err(io::Error::other)
    }
}

/// A channel to the node of `key` through the proxied relay at `url`, its
/// handshake done and no preamble, from `address`; `None` if refused.
async fn handshake_from(url: &RouteUrl, key: &NodePublicKey, address: &str) -> Option<impl Sized> {
    let routing = vessel_secure::routing_id(key);
    let target = format!("{url}{}?node={routing}", vessel_relay::CONNECT_PATH);
    handshake_at(&target, key, address).await.ok()
}

/// A channel to the node of `key` at the websocket URL `target`, its
/// handshake done and no preamble, saying in [`HEADER`] that it comes from
/// `address`; when refused, the HTTP status it was refused with, if any.
async fn handshake_at(
    target: &str,
    key: &NodePublicKey,
    address: &str,
) -> Result<impl Sized + use<>, Option<u16>> {
    let mut request = target.into_client_request().unwrap();
    let _previous = request
        .headers_mut()
        .insert(HEADER, address.parse().unwrap());
    let socket = match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _response)) => socket,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            return Err(Some(response.status().as_u16()));
        }
        Err(_) => return Err(None),
    };
    let handshake = vessel_secure::initiate(Proxied(socket), key);
    match tokio::time::timeout(Duration::from_secs(3), handshake).await {
        Ok(Ok(channel)) => Ok(channel),
        Ok(Err(_)) | Err(_) => Err(None),
    }
}

/// One IPv6 /48's share of one node's clients at a relay's defaults: four
/// from each of four /64s, in two /56s.
const ONE_48: [&str; 16] = [
    "2001:db8:5:1::1",
    "2001:db8:5:1::2",
    "2001:db8:5:1::3",
    "2001:db8:5:1::4",
    "2001:db8:5:2::1",
    "2001:db8:5:2::2",
    "2001:db8:5:2::3",
    "2001:db8:5:2::4",
    "2001:db8:5:101::1",
    "2001:db8:5:101::2",
    "2001:db8:5:101::3",
    "2001:db8:5:101::4",
    "2001:db8:5:102::1",
    "2001:db8:5:102::2",
    "2001:db8:5:102::3",
    "2001:db8:5:102::4",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_48_through_each_of_three_relays_leaves_every_link_room() {
    within(async {
        let mut urls = Vec::new();
        let mut stops = Vec::new();
        for _ in 0..3 {
            let (url, stop) = proxied_relay().await;
            urls.push(url);
            stops.push(stop);
        }
        let fixture = Fixture::new();
        let relays: Vec<&RouteUrl> = urls.iter().collect();
        let node = relayed_with(&fixture, &relays, |_config| {}).await;
        let key = node.info().public_key.unwrap();
        // The /48's whole share through every relay, held unadmitted.
        let mut held = Vec::new();
        for url in &urls {
            for from in ONE_48 {
                if let Some(channel) = handshake_from(url, &key, from).await {
                    held.push(channel);
                }
            }
        }
        assert_eq!(held.len(), 48, "the node refused some of the /48's share");
        // Someone else still gets in through the third relay, and directly.
        let other = handshake_from(&urls[2], &key, "2001:db8:6::1").await;
        assert!(
            other.is_some(),
            "a client from another /48 through the third relay"
        );
        assert!(direct_handshake(&node, &key).await, "the direct route");
        drop((held, other));
        node.stop().await.unwrap();
        for stop in stops {
            stop.cancel();
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_host_holds_few_of_a_nodes_direct_places() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let key = node.info().public_key.unwrap();
        let direct = format!("{}/secure", direct_route(&node));
        let mut held = Vec::new();
        for _ in 0..16 {
            let Ok(carrier) = vessel_relay::connect(&direct, &tls()).await else {
                continue;
            };
            let handshake = vessel_secure::initiate(carrier, &key);
            if let Ok(Ok(channel)) = tokio::time::timeout(Duration::from_secs(3), handshake).await {
                held.push(channel);
            }
        }
        assert!(
            held.len() < 16 && !held.is_empty(),
            "{} direct handshakes held from one host",
            held.len()
        );
        drop(held);
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_behind_a_proxy_counts_its_direct_clients_by_the_proxys_header() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture
            .start_with(|config| {
                config.client_address_header = Some(HEADER.parse().unwrap());
                config.trusted_proxies = vec!["127.0.0.1".parse().unwrap()];
            })
            .await;
        let key = node.info().public_key.unwrap();
        let secure = format!("{}/secure", direct_route(&node));
        // One client behind the proxy holds its share of the direct places...
        let mut held = Vec::new();
        for _ in 0..16 {
            if let Ok(channel) = handshake_at(&secure, &key, "203.0.113.1").await {
                held.push(channel);
            }
        }
        assert!(held.len() < 16, "{} held by one client", held.len());
        // ...and another behind the same proxy still gets in.
        let other = handshake_at(&secure, &key, "203.0.113.2").await;
        assert!(other.is_ok(), "another client behind the proxy");
        // A header that is not one bare address is refused before the
        // upgrade.
        let listed = handshake_at(&secure, &key, "203.0.113.3, 10.0.0.1").await;
        assert_eq!(listed.err(), Some(Some(400)));
        drop((held, other));
        node.stop().await.unwrap();

        // The header alone, with no proxy to believe it from: the node does
        // not start.
        let fixture = Fixture::new();
        let mut config = vessel_node::Config::new(fixture.data_dir(), fixture.harness_env());
        config.bind = Some(std::net::SocketAddr::from((
            std::net::Ipv4Addr::LOCALHOST,
            0,
        )));
        config.client_address_header = Some(HEADER.parse().unwrap());
        let refused = Node::start(config)
            .await
            .err()
            .map(|error| error.to_string());
        assert!(
            refused
                .as_deref()
                .is_some_and(|text| text.contains("--trusted-proxy")),
            "{refused:?}"
        );
    })
    .await;
}
