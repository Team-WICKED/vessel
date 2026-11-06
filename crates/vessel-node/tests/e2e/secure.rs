//! Paired devices end to end (milestone 3 done criteria): a real relay on
//! loopback (`vessel_relay::serve`), the testkit's misbehaving relay in
//! front of one, a node linked to them, and devices pairing and connecting
//! with `vessel-client`'s `pair_over` and `SecureConnector`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vessel_client::pairing::Paired;
use vessel_client::secure::{RouteError, RouteTimeouts, SecureConnector, WsDialer, pair_over};
use vessel_client::{
    Backoff, Connector, NodeClient, Pinned, ResumingStream, SecureTarget, StreamError,
    follow_resuming,
};
use vessel_node::Node;
use vessel_proto::{
    EventRecord, EventSeq, HarnessKind, PairingOffer, RelayClose, RelayToken, RouteUrl, SessionId,
};
use vessel_relay::{Keepalive, LinkState, RelayConfig, TlsConfig};
use vessel_rpc::CallError;
use vessel_secure::NodeKeypair;
use vessel_testkit::relay::{MisbehavingRelay, Tamper, Toward};

use crate::support::{Fixture, client, keys, output_of, seqs, terminal_spec, within};

const TOKEN: &str = "e2e-relay-token";

/// A relay on a loopback port until stopped.
struct Relay {
    url: RouteUrl,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl Relay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let shutdown = CancellationToken::new();
        let config = RelayConfig::new(RelayToken::new(TOKEN).unwrap());
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                vessel_relay::serve(listener, config, shutdown)
                    .await
                    .unwrap();
            }
        });
        Self {
            url,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap();
    }
}

/// The node of `fixture`, linked to `relays`, once registered with all.
async fn relayed(fixture: &Fixture, relays: &[&RouteUrl]) -> Node {
    let token_file = fixture.cwd().join("relay-token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let node = fixture
        .start_with(|config| {
            config.relays = relays.iter().map(|&relay| relay.clone()).collect();
            config.relay_token_file = Some(token_file);
        })
        .await;
    for (_relay, mut state) in node.relay_links() {
        let _registered = state
            .wait_for(|state| *state == LinkState::Registered)
            .await
            .unwrap();
    }
    node
}

fn dialer() -> WsDialer {
    WsDialer::new(TlsConfig::with_roots(rustls::RootCertStore::empty()).unwrap())
}

async fn invite(owner: &NodeClient, ttl_secs: u32) -> PairingOffer {
    owner
        .invite_device(None, ttl_secs)
        .await
        .unwrap()
        .link
        .decode()
        .unwrap()
}

async fn pair(owner: &NodeClient, name: &str) -> Paired {
    let offer = invite(owner, 600).await;
    let (paired, _route) = pair_over(&dialer(), &offer, name, RouteTimeouts::default())
        .await
        .unwrap();
    paired
}

type Device = Pinned<SecureConnector<WsDialer>>;

fn device(paired: &Paired, direct: &[&RouteUrl], relays: &[&RouteUrl]) -> Arc<Device> {
    device_via(dialer(), paired, direct, relays)
}

fn device_via(
    dialer: WsDialer,
    paired: &Paired,
    direct: &[&RouteUrl],
    relays: &[&RouteUrl],
) -> Arc<Device> {
    let target = SecureTarget {
        key: paired.credential.key,
        direct: direct.iter().map(|&route| route.clone()).collect(),
        relays: relays.iter().map(|&route| route.clone()).collect(),
    };
    let node = paired.node.id;
    let connector = SecureConnector::new(dialer, target, node, paired.credential.clone()).unwrap();
    Arc::new(Pinned::new(connector, Some(node)))
}

fn direct_route(node: &Node) -> RouteUrl {
    format!("ws://{}", node.address().unwrap()).parse().unwrap()
}

/// A loopback address nothing listens on.
async fn closed_route() -> RouteUrl {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("ws://{address}").parse().unwrap()
}

const FAST: Backoff = Backoff {
    first: Duration::from_millis(10),
    max: Duration::from_millis(100),
};

/// Read `stream` until the output holds `needle`, keeping records and
/// notices apart.
async fn until<C: Connector + 'static>(
    stream: &mut ResumingStream<C>,
    records: &mut Vec<EventRecord>,
    notices: &mut Vec<StreamError>,
    needle: &str,
) {
    while !output_of(records).contains(needle) {
        match stream.next().await {
            Some(Ok(record)) => records.push(record),
            Some(Err(notice)) if notice.is_fatal() => panic!("{notice}; before {needle:?}"),
            Some(Err(notice)) => notices.push(notice),
            None => panic!("the stream ended before {needle:?}"),
        }
    }
}

fn gap_free(records: &[EventRecord]) -> bool {
    let expected: Vec<u64> = (0..u64::try_from(records.len()).unwrap()).collect();
    seqs(records) == expected
}

async fn terminal(fixture: &Fixture, owner: &NodeClient) -> SessionId {
    owner
        .create_session(terminal_spec(fixture, HarnessKind::ClaudeCode))
        .await
        .unwrap()
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_through_the_relay_then_connecting_directly_or_through_it() {
    within(async {
        let relay = Relay::start().await;
        let fixture = Fixture::new();
        let node = relayed(&fixture, &[&relay.url]).await;
        let owner = fixture.connect(&node).await;

        let offer = invite(&owner, 600).await;
        assert_eq!(offer.relays, std::slice::from_ref(&relay.url));
        let (paired, route) = pair_over(&dialer(), &offer, "laptop", RouteTimeouts::default())
            .await
            .unwrap();
        assert_eq!(route.to_string(), format!("relay {}", relay.url));
        assert_eq!(paired.node.id, node.info().id);

        // The same link again is refused, for good.
        let again = pair_over(&dialer(), &offer, "again", RouteTimeouts::default())
            .await
            .unwrap_err();
        assert!(
            again.is_final() && again.to_string().contains("not valid"),
            "{again}"
        );

        // A link with another key: the node closes its direct route with
        // 4401, and the relay has no node under that key's routing id.
        let direct = direct_route(&node);
        let mut forged = invite(&owner, 600).await;
        forged.key = *NodeKeypair::generate().unwrap().public();
        forged.direct = vec![direct.clone()];
        let wrong = pair_over(&dialer(), &forged, "x", RouteTimeouts::default())
            .await
            .unwrap_err();
        let failures = wrong.failures();
        assert!(matches!(failures[0].error, RouteError::WrongKey), "{wrong}");
        assert!(
            matches!(failures[1].error, RouteError::Relay(RelayClose::NoSuchNode)),
            "{wrong}"
        );
        assert!(wrong.is_final());

        // An expired link.
        let short = invite(&owner, 1).await;
        let left = (short.expires - Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(left + Duration::from_millis(100)).await;
        let expired = pair_over(&dialer(), &short, "late", RouteTimeouts::default())
            .await
            .unwrap_err();
        assert!(expired.is_final(), "{expired}");

        // An unreachable direct address, then the relay.
        let closed = closed_route().await;
        let laptop = device(&paired, &[&closed], &[&relay.url]);
        let connection = laptop.connect(client()).await.unwrap();
        assert!(connection.sessions().await.unwrap().is_empty());

        // A reachable direct address is used first, relay or not.
        relay.stop().await;
        let laptop = device(&paired, &[&direct], &[]);
        let connection = laptop.connect(client()).await.unwrap();
        assert_eq!(connection.node().id, node.info().id);
        node.stop().await.unwrap();
    })
    .await;
}

/// P5: the first relay drops a record toward the device and goes away.
/// The channel notices the gap, the stream dials again, the first relay is
/// gone, and the second carries the rest from the cursor: nothing is
/// repeated or skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_drop_during_an_attach_resumes_on_the_next_relay() {
    within(async {
        let first = Relay::start().await;
        let second = Relay::start().await;
        let flaky = MisbehavingRelay::start(first.url.as_str()).await.unwrap();
        let flaky_url: RouteUrl = flaky.url().parse().unwrap();
        let fixture = Fixture::new();
        let node = relayed(&fixture, &[&flaky_url, &second.url]).await;
        let owner = fixture.connect(&node).await;
        let paired = pair(&owner, "laptop").await;
        let session = terminal(&fixture, &owner).await;

        let laptop = device(&paired, &[], &[&flaky_url, &second.url]);
        let mut stream =
            follow_resuming(laptop, client(), session, EventSeq::ZERO).with_backoff(FAST);
        let (mut records, mut notices) = (Vec::new(), Vec::new());
        until(&mut stream, &mut records, &mut notices, "> ").await;
        assert!(notices.is_empty());

        // The channel notices a dropped record only when the next one
        // arrives, so more output must follow the drop: under load the
        // output of two lines can come as one record, and if that is the
        // one dropped, nothing would. Wait for the drop, then type again.
        flaky.tamper(Toward::Client, Tamper::Drop);
        owner.send(session, keys("one\r")).await.unwrap();
        flaky.applied(1).await;
        drop(flaky);
        owner.send(session, keys("two\r")).await.unwrap();
        until(&mut stream, &mut records, &mut notices, "echo: two").await;

        assert!(gap_free(&records), "{:?}", seqs(&records));
        assert_eq!(output_of(&records).matches("echo: one").count(), 1);
        let resumed = notices
            .iter()
            .filter(|notice| matches!(notice, StreamError::Resumed { .. }))
            .count();
        assert_eq!(resumed, 1, "{notices:?}");
        assert!(
            matches!(notices[0], StreamError::Reconnecting { attempt: 1, .. }),
            "{notices:?}"
        );
        node.stop().await.unwrap();
        second.stop().await;
        first.stop().await;
    })
    .await;
}

/// A revoked device is refused with `Revoked`, its open attach ends with
/// that reason, and another device carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_device_loses_its_attach_and_others_carry_on() {
    within(async {
        let relay = Relay::start().await;
        let fixture = Fixture::new();
        let node = relayed(&fixture, &[&relay.url]).await;
        let owner = fixture.connect(&node).await;
        let laptop_pairing = pair(&owner, "laptop").await;
        let phone_pairing = pair(&owner, "phone").await;
        let session = terminal(&fixture, &owner).await;

        let laptop = device(&laptop_pairing, &[], &[&relay.url]);
        let mut stream = follow_resuming(Arc::clone(&laptop), client(), session, EventSeq::ZERO)
            .with_backoff(FAST);
        let (mut records, mut notices) = (Vec::new(), Vec::new());
        until(&mut stream, &mut records, &mut notices, "> ").await;
        let phone = device(&phone_pairing, &[], &[&relay.url])
            .connect(client())
            .await
            .unwrap();

        owner
            .revoke_device(laptop_pairing.credential.device)
            .await
            .unwrap();
        let ended = loop {
            match stream.next().await {
                Some(Ok(_record)) => {}
                Some(Err(StreamError::Unreachable { reason })) => break reason,
                Some(Err(notice)) if notice.is_fatal() => panic!("{notice}"),
                Some(Err(_notice)) => {}
                None => panic!("ended without a reason"),
            }
        };
        assert!(ended.contains("revoked"), "{ended}");
        assert!(stream.next().await.is_none());

        let refused = laptop.connect(client()).await.unwrap_err();
        assert!(!laptop.is_transient(&refused), "{refused}");
        assert_eq!(phone.sessions().await.unwrap().len(), 1);
        node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

/// A relay that corrupts, replays or reorders a device's records ends the
/// connection, and no tampered frame reaches the node's RPC layer: a
/// replayed input is never applied twice (its genuine first copy may be,
/// or may be dropped with the connection, ADR 0007), a corrupted or
/// reordered one never.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampering_relay_ends_the_connection_and_no_frame_gets_through() {
    within(async {
        let relay = Relay::start().await;
        let flaky = MisbehavingRelay::start(relay.url.as_str()).await.unwrap();
        let flaky_url: RouteUrl = flaky.url().parse().unwrap();
        let fixture = Fixture::new();
        let node = relayed(&fixture, &[&flaky_url]).await;
        let owner = fixture.connect(&node).await;
        let paired = pair(&owner, "laptop").await;
        let session = terminal(&fixture, &owner).await;
        let mut watch = owner.follow(session, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        crate::support::collect_until(&mut watch, &mut seen, "> ").await;
        let laptop = device(&paired, &[], &[&flaky_url]);
        let markers = ["marker 0\r", "marker 1\r", "marker 2\r"];
        let firsts = |seen: &[EventRecord]| output_of(seen).matches("echo: first").count();

        for (round, tamper) in [Tamper::Replay, Tamper::Corrupt, Tamper::Reorder]
            .into_iter()
            .enumerate()
        {
            let connection = laptop.connect(client()).await.unwrap();
            flaky.tamper(Toward::Node, tamper);
            // The first input's record is the one tampered with (taken by
            // the relay before the second is sent); the second follows.
            let first = tokio::spawn({
                let connection = connection.clone();
                async move { connection.send(session, keys("first\r")).await }
            });
            flaky.applied(u64::try_from(round).unwrap() + 1).await;
            let second = connection.send(session, keys("second\r")).await;
            let first = first.await.unwrap();
            assert!(matches!(second, Err(CallError::Disconnected)), "{tamper:?}");
            let before = firsts(&seen);
            // A marker from the owner: whatever the device's records did
            // reached the log before it.
            owner.send(session, keys(markers[round])).await.unwrap();
            let marker = format!("echo: marker {round}");
            crate::support::collect_until(&mut watch, &mut seen, &marker).await;
            let applied = firsts(&seen).checked_sub(before).unwrap();
            match tamper {
                // The genuine copy may have been applied before the
                // replayed one ended the connection: if the call says so,
                // it was, once; if it failed, it may or may not have been.
                // The copy is never applied.
                Tamper::Replay if first.is_ok() => assert_eq!(applied, 1),
                Tamper::Replay => assert!(applied <= 1, "applied {applied} times"),
                // Nothing of a corrupted or reordered record is applied.
                Tamper::Corrupt | Tamper::Reorder | Tamper::Drop | Tamper::Delay(_) => {
                    assert!(matches!(first, Err(CallError::Disconnected)), "{tamper:?}");
                    assert_eq!(applied, 0, "{tamper:?}");
                }
            }
            let output = output_of(&seen);
            assert_eq!(output.matches("echo: second").count(), 0, "{tamper:?}");
        }
        node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

/// A TCP forwarder that can freeze the connections it carries: they stay
/// open but carry nothing to the client, and what the node sends is read
/// and dropped, as on a path that went half-open (a laptop asleep, a NAT
/// mapping gone). New connections still pass. It reports each frozen
/// connection the node then closes.
struct Freezer {
    address: std::net::SocketAddr,
    freeze: tokio::sync::watch::Sender<u64>,
    closed_by_node: tokio::sync::mpsc::Receiver<()>,
    task: JoinHandle<()>,
}

impl Freezer {
    async fn start(upstream: std::net::SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (freeze, _) = tokio::sync::watch::channel(0_u64);
        let (report, closed_by_node) = tokio::sync::mpsc::channel(8);
        let generations = freeze.clone();
        let task = tokio::spawn(async move {
            while let Ok((down, _peer)) = listener.accept().await {
                let up = tokio::net::TcpStream::connect(upstream).await.unwrap();
                let frozen = generations.subscribe();
                drop(tokio::spawn(carry(down, up, frozen, report.clone())));
            }
        });
        Self {
            address,
            freeze,
            closed_by_node,
            task,
        }
    }

    fn route(&self) -> RouteUrl {
        format!("ws://{}", self.address).parse().unwrap()
    }

    /// Freeze every connection open now.
    fn freeze(&self) {
        self.freeze
            .send_modify(|generation| *generation = generation.saturating_add(1));
    }
}

impl Drop for Freezer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn carry(
    down: tokio::net::TcpStream,
    up: tokio::net::TcpStream,
    mut frozen: tokio::sync::watch::Receiver<u64>,
    closed_by_node: tokio::sync::mpsc::Sender<()>,
) {
    use tokio::io::AsyncReadExt as _;
    let born = *frozen.borrow();
    let (mut down_read, mut down_write) = down.into_split();
    let (mut up_read, mut up_write) = up.into_split();
    let carrying = async {
        tokio::try_join!(
            tokio::io::copy(&mut down_read, &mut up_write),
            tokio::io::copy(&mut up_read, &mut down_write),
        )
    };
    tokio::select! {
        _done = carrying => return,
        _frozen = frozen.wait_for(|generation| *generation > born) => {}
    }
    // Frozen: the client hears nothing more; the node's pings are read and
    // dropped until the node gives up and closes.
    let mut sink = [0_u8; 4096];
    while matches!(up_read.read(&mut sink).await, Ok(read) if read > 0) {}
    let _reported = closed_by_node.send(()).await;
    // The client's side stays open, silent, until the test ends.
    std::future::pending::<()>().await;
}

/// P5 on a direct route: the path freezes mid-attach. The node's pings
/// stop arriving, the client's carrier gives up after the silence limit,
/// and the attach resumes on a new connection with nothing lost or
/// repeated; the node, hearing no pongs, drops its end too. (Keepalive in
/// milliseconds here; 10 s and 30 s by default.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frozen_direct_route_is_dropped_by_keepalive_and_the_attach_resumes() {
    within(async {
        // Short, with room for a loaded machine: a ping is late by up to
        // 800 ms before the quiet connection below counts as silent.
        let fast = Keepalive {
            ping: Duration::from_millis(200),
            silence: Duration::from_secs(1),
        };
        let fixture = Fixture::new();
        let node = fixture.start_with(|config| config.keepalive = fast).await;
        let owner = fixture.connect(&node).await;
        let mut offer = invite(&owner, 600).await;
        offer.direct = vec![direct_route(&node)];
        let (paired, _route) = pair_over(&dialer(), &offer, "laptop", RouteTimeouts::default())
            .await
            .unwrap();
        let session = terminal(&fixture, &owner).await;

        let mut path = Freezer::start(node.address().unwrap()).await;
        let route = path.route();
        let laptop = device_via(dialer().with_keepalive(fast), &paired, &[&route], &[]);
        let mut stream =
            follow_resuming(laptop, client(), session, EventSeq::ZERO).with_backoff(FAST);
        let (mut records, mut notices) = (Vec::new(), Vec::new());
        until(&mut stream, &mut records, &mut notices, "> ").await;
        // Quiet past the silence limit: the node's pings keep it up.
        tokio::time::sleep(fast.silence.saturating_mul(2)).await;
        owner.send(session, keys("before\r")).await.unwrap();
        until(&mut stream, &mut records, &mut notices, "echo: before").await;
        assert!(notices.is_empty(), "{notices:?}");

        path.freeze();
        owner.send(session, keys("after\r")).await.unwrap();
        until(&mut stream, &mut records, &mut notices, "echo: after").await;
        assert!(gap_free(&records), "{:?}", seqs(&records));
        assert!(
            matches!(
                notices.as_slice(),
                [
                    StreamError::Reconnecting { attempt: 1, .. },
                    StreamError::Resumed { .. }
                ]
            ),
            "{notices:?}"
        );
        assert!(
            path.closed_by_node.recv().await.is_some(),
            "the node dropped the frozen connection"
        );
        node.stop().await.unwrap();
    })
    .await;
}
