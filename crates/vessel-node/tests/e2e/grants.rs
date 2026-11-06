//! Who a member device speaks for on its home (protocol 6, the review of
//! 2026-11-06): a grant names a node by an id the node chose, so the home
//! binds each member device to the id and key of its first grant and
//! refuses grants that would let one node stand in for another.
//!
//! The first test is the review's proof of concept turned around: a
//! device holding only `sync` (what every member's device holds) grants a
//! link from a stock node whose `node.id` names another member. Before the
//! fix the home kept it, listed that member twice, and its calls to the
//! member went to the impostor.

use std::time::Duration;

use chrono::Utc;
use vessel_client::secure::{RouteTimeouts, pair_over};
use vessel_client::{Connector as _, NodeClient};
use vessel_node::fleet::{self, DEFAULT_GRANT, FleetFiles};
use vessel_proto::{
    ErrorCode, GrantHome, HarnessKind, ListPeers, ListSessions, PairingLink, PairingOffer,
    RouteUrl, Scope,
};

use crate::across::{Member, home, member_granting};
use crate::agents::vessel_mcp;
use crate::fleet::fleet_config;
use crate::secure::{Relay, closed_route, device_via, dialer, relayed_with};
use crate::support::{Fixture, client, structured_spec, within};

/// A node of its own on `relay`, named `name`, whose `node.id` is `id`
/// when given (a stock node: only the file differs).
pub(crate) async fn stock_node(
    relay: &Relay,
    name: &str,
    id: Option<vessel_proto::NodeId>,
) -> (Fixture, vessel_node::Node, NodeClient) {
    let fixture = Fixture::new();
    if let Some(id) = id {
        std::fs::write(fixture.cwd().join("node.id"), format!("{id}\n")).unwrap();
    }
    let node = relayed_with(&fixture, &[&relay.url], |config| {
        config.name = Some(name.to_owned());
    })
    .await;
    let owner = fixture.connect(&node).await;
    (fixture, node, owner)
}

/// A link to `owner`'s node, as a member grants its home.
pub(crate) async fn link(owner: &NodeClient, label: &str) -> PairingLink {
    owner
        .invite_with_scopes(
            Some(label.to_owned()),
            600,
            vec![Scope::Read, Scope::Operate],
        )
        .await
        .unwrap()
        .link
}

/// A link that claims node `id`, from a node holding another key, and that
/// node (kept running while the link is used). On Unix it is the review's
/// impostor, a stock node whose `node.id` holds `id`. On Windows two nodes
/// of one id cannot run on one machine (the id names the node's pipe), so
/// there it is another node's link rewritten to name `id`: the home
/// refuses the claim before it dials, either way.
async fn impostor_link(
    relay: &Relay,
    id: vessel_proto::NodeId,
) -> (Fixture, vessel_node::Node, PairingLink) {
    #[cfg(unix)]
    {
        let (fixture, node, owner) = stock_node(relay, "desktop-too", Some(id)).await;
        assert_eq!(node.info().id, id);
        let link = link(&owner, "bait").await;
        (fixture, node, link)
    }
    #[cfg(not(unix))]
    {
        let (fixture, node, owner) = stock_node(relay, "desktop-too", None).await;
        let mut offer = link(&owner, "bait").await.decode().unwrap();
        offer.node = id;
        (fixture, node, PairingLink::encode(&offer).unwrap())
    }
}

/// A device on `home` with only `sync`, connected.
async fn sync_device(
    relay: &Relay,
    home: &Member,
) -> std::sync::Arc<impl vessel_client::Connector> {
    device_with(relay, home, vec![Scope::Sync]).await
}

/// A device on `home` with `scopes`, connected.
pub(crate) async fn device_with(
    relay: &Relay,
    home: &Member,
    scopes: Vec<Scope>,
) -> std::sync::Arc<impl vessel_client::Connector> {
    let offer = home
        .owner
        .invite_with_scopes(Some("someone".to_owned()), 600, scopes)
        .await
        .unwrap()
        .link
        .decode()
        .unwrap();
    let (paired, _route) = pair_over(&dialer(), &offer, "someone", RouteTimeouts::default())
        .await
        .unwrap();
    device_via(dialer(), &paired, &[], &[&relay.url])
}

fn code<T: std::fmt::Debug>(result: Result<T, vessel_rpc::CallError>) -> (ErrorCode, String) {
    match result {
        Err(vessel_rpc::CallError::Remote(error)) => (error.code, error.message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_device_cannot_speak_for_another_members_node() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let someone = sync_device(&relay, &home)
            .await
            .connect(client())
            .await
            .unwrap();
        let victim = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        let theirs = victim
            .owner
            .create_session(structured_spec(&victim.fixture, None))
            .await
            .unwrap();

        // A stock node claiming the victim's id: refused, and the victim
        // is still the one the home lists and reaches.
        let (_fixture, _impostor, bait) = impostor_link(&relay, victim.id()).await;
        let refused = someone.call(GrantHome { link: bait }).await;
        let (refusal, message) = code(refused);
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");
        let peers = home.owner.call(ListPeers).await.unwrap();
        assert_eq!(
            peers.iter().filter(|peer| peer.node == victim.id()).count(),
            1,
            "{peers:?}"
        );
        let listed = home.owner.call_on(victim.id(), ListSessions).await.unwrap();
        assert!(
            listed.iter().any(|session| session.id == theirs.id),
            "{listed:?}"
        );

        // The device's own node is granted; then that device keeps it.
        let (_fixture, _own, own) = stock_node(&relay, "tablet", None).await;
        let _ack = someone
            .call(GrantHome {
                link: link(&own, "home").await,
            })
            .await
            .unwrap();
        let (_fixture, _other, other) = stock_node(&relay, "phone", None).await;
        let (refusal, message) = code(
            someone
                .call(GrantHome {
                    link: link(&other, "home").await,
                })
                .await,
        );
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");
        assert!(message.contains("never changes"), "{message}");
        relay.stop().await;
    })
    .await;
}

/// A grant's link names the routes the home dials: a few, and a failure to
/// pair says nothing about them to the device that sent it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_cannot_make_the_home_dial_where_it_likes() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let someone = sync_device(&relay, &home)
            .await
            .connect(client())
            .await
            .unwrap();
        let (_fixture, _node, owner) = stock_node(&relay, "tablet", None).await;
        let offer: PairingOffer = link(&owner, "home").await.decode().unwrap();
        let closed = closed_route().await;
        let rerouted = |direct: Vec<RouteUrl>| {
            let mut offer = offer.clone();
            offer.direct = direct;
            offer.relays = Vec::new();
            PairingLink::encode(&offer).unwrap()
        };

        let many = someone
            .call(GrantHome {
                link: rerouted(vec![closed.clone(); 9]),
            })
            .await;
        let (refusal, message) = code(many);
        assert_eq!(refusal, ErrorCode::InvalidRequest, "{message}");
        let unreachable = someone
            .call(GrantHome {
                link: rerouted(vec![closed.clone()]),
            })
            .await;
        let (refusal, message) = code(unreachable);
        assert_eq!(refusal, ErrorCode::Unavailable, "{message}");
        let port = closed
            .as_str()
            .rsplit(':')
            .next()
            .unwrap()
            .trim_end_matches('/');
        assert!(
            !message.contains("127.0.0.1") && !message.contains(port),
            "the refusal describes the route: {message}"
        );
        relay.stop().await;
    })
    .await;
}

/// The live devices `owner`'s node made for a fleet home.
async fn home_devices(owner: &NodeClient) -> Vec<vessel_proto::DeviceId> {
    owner
        .devices()
        .await
        .unwrap()
        .into_iter()
        .filter(|device| device.revoked_at.is_none() && device.label.starts_with("fleet home "))
        .map(|device| device.id)
        .collect()
}

/// Wait until `member`'s device on `home` reported after `since`: its pull
/// loop connected again and got past granting (which comes first).
async fn reported_after(home: &Member, label: &str, since: chrono::DateTime<Utc>) {
    for _ in 0..800 {
        let members = home.owner.fleet_members().await.unwrap();
        if members
            .iter()
            .any(|member| member.label == label && member.reported_at.is_some_and(|at| at > since))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{label} never reported again");
}

/// The review's item 2: the member's owner revokes the home's device; a
/// restart (or a home dropping the connection) used to grant a new one.
/// Joining again is what grants again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_the_homes_device_on_a_member_holds_until_it_joins_again() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let member = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        assert!(home.owner.call_on(member.id(), ListSessions).await.is_ok());
        let granted = home_devices(&member.owner).await;
        assert_eq!(granted.len(), 1, "{granted:?}");
        member.owner.revoke_device(granted[0]).await.unwrap();
        assert!(home.owner.call_on(member.id(), ListSessions).await.is_err());

        // A restart: the pull loop connects and reports again, and grants
        // nothing.
        let Member { fixture, node, .. } = member;
        let id = node.info().id;
        node.stop().await.unwrap();
        let restarted_at = Utc::now();
        let applied = fixture.cwd().join("member-codex");
        let node = relayed_with(&fixture, &[&relay.url], |config| {
            config.name = Some("desktop".to_owned());
            config.fleet = fleet_config(HarnessKind::Codex, applied);
            config.vessel_mcp = Some(vessel_mcp(&fixture));
        })
        .await;
        reported_after(&home, "desktop", restarted_at).await;
        let owner = fixture.connect(&node).await;
        assert!(home_devices(&owner).await.is_empty());
        assert!(home.owner.call_on(id, ListSessions).await.is_err());

        // Leaving and joining again grants again.
        assert!(fleet::leave(&FleetFiles::new(&fixture.data_dir())).unwrap());
        let offer = home
            .owner
            .invite_with_scopes(Some("desktop".to_owned()), 600, vec![Scope::Sync])
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        let _joined = fleet::join(
            &FleetFiles::new(&fixture.data_dir()),
            &offer,
            "desktop",
            DEFAULT_GRANT.to_vec(),
        )
        .await
        .unwrap();
        let mut back = false;
        for _ in 0..800 {
            if home.owner.call_on(id, ListSessions).await.is_ok() {
                back = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(back, "joining again did not grant the home access");
        assert_eq!(home_devices(&owner).await.len(), 1);
        relay.stop().await;
    })
    .await;
}

/// The review's item 4: a device made for a fleet home that the member has
/// no record of is revoked, whether a grant failed after the home redeemed
/// it (here: the home refuses a second member named like the first) or it
/// came from anywhere else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_keeps_no_device_for_a_home_it_has_no_record_of() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let first = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;

        // Paired by hand under the reserved label: swept.
        let offer = first
            .owner
            .invite_device(Some(format!("fleet home {}", home.id())), 600)
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        let (_paired, _route) = pair_over(&dialer(), &offer, "stray", RouteTimeouts::default())
            .await
            .unwrap();
        let mut swept = false;
        for _ in 0..800 {
            if home_devices(&first.owner).await.len() == 1 {
                swept = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(swept, "{:?}", home_devices(&first.owner).await);
        assert!(home.owner.call_on(first.id(), ListSessions).await.is_ok());

        // A second node named like the first: the home pairs with it, then
        // refuses the grant; the device it made there is revoked at once.
        let fixture = Fixture::new();
        let applied = fixture.cwd().join("member-codex");
        let second = relayed_with(&fixture, &[&relay.url], |config| {
            config.name = Some("desktop".to_owned());
            config.fleet = fleet_config(HarnessKind::Codex, applied);
        })
        .await;
        let since = Utc::now();
        let offer = home
            .owner
            .invite_with_scopes(Some("other desktop".to_owned()), 600, vec![Scope::Sync])
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        let _joined = fleet::join(
            &FleetFiles::new(&fixture.data_dir()),
            &offer,
            "other desktop",
            DEFAULT_GRANT.to_vec(),
        )
        .await
        .unwrap();
        reported_after(&home, "other desktop", since).await;
        let owner = fixture.connect(&second).await;
        assert!(home_devices(&owner).await.is_empty());
        let refused = home.owner.call_on(second.info().id, ListSessions).await;
        assert!(refused.is_err(), "{refused:?}");
        // The member says why, and how to fix it (`vessel fleet status`).
        let status = fleet::status(&FleetFiles::new(&fixture.data_dir())).unwrap();
        let why = status.state.grant.unwrap_or_default();
        assert!(
            why.contains("named `desktop`") && why.contains("--name <name>"),
            "{why}"
        );
        relay.stop().await;
    })
    .await;
}

/// A member's node on `fixture`, named `name`, linked to `relay`.
async fn member_node(relay: &Relay, fixture: &Fixture, name: &str) -> vessel_node::Node {
    let applied = fixture.cwd().join("member-codex");
    relayed_with(fixture, &[&relay.url], |config| {
        config.name = Some(name.to_owned());
        config.fleet = fleet_config(HarnessKind::Codex, applied);
        config.vessel_mcp = Some(vessel_mcp(fixture));
    })
    .await
}

/// Wait until the home reaches node `id` (a call there succeeds).
async fn reaches(home: &Member, id: vessel_proto::NodeId) -> bool {
    for _ in 0..800 {
        if home.owner.call_on(id, ListSessions).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Leave the fleet and join it again through a new invitation labelled
/// `label`.
async fn join_again(home: &Member, fixture: &Fixture, label: &str) {
    let files = FleetFiles::new(&fixture.data_dir());
    assert!(fleet::leave(&files).unwrap());
    let offer = home
        .owner
        .invite_with_scopes(Some(label.to_owned()), 600, vec![Scope::Sync])
        .await
        .unwrap()
        .link
        .decode()
        .unwrap();
    let _joined = fleet::join(&files, &offer, label, DEFAULT_GRANT.to_vec())
        .await
        .unwrap();
}

/// The review's round 2, finding A: the home's owner revokes a member's
/// device (to pair it again, say); nothing live holds the member's id
/// then, and a `sync` device bound it to an impostor, so the member's own
/// grant was refused when it joined again. The home's record keeps the id
/// with its key for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_members_id_stays_bound_to_its_key() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let someone = sync_device(&relay, &home)
            .await
            .connect(client())
            .await
            .unwrap();
        let victim = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        let member_device = home
            .owner
            .devices()
            .await
            .unwrap()
            .into_iter()
            .find(|device| device.label == "desktop")
            .unwrap()
            .id;
        home.owner.revoke_device(member_device).await.unwrap();

        let (_fixture, _impostor, bait) = impostor_link(&relay, victim.id()).await;
        let (refusal, message) = code(someone.call(GrantHome { link: bait }).await);
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");

        // The member joins again, with its own key: granted.
        join_again(&home, &victim.fixture, "desktop again").await;
        assert!(
            reaches(&home, victim.id()).await,
            "the member's own grant was refused"
        );
        relay.stop().await;
    })
    .await;
}

/// A node reinstalled with a new key keeps its id: the home refuses its
/// grant until its owner runs `vessel fleet forget` (here
/// `fleet::forget`), which drops the binding and the old grants, and the
/// node is invited and joins again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reinstalled_node_grants_again_once_the_home_forgets_it() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let old = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        let id = old.id();
        old.node.stop().await.unwrap();

        // The same id, a new key, a new data directory.
        let fixture = Fixture::new();
        std::fs::write(fixture.cwd().join("node.id"), format!("{id}\n")).unwrap();
        let node = member_node(&relay, &fixture, "desktop").await;
        assert_eq!(node.info().id, id);
        let since = Utc::now();
        let offer = home
            .owner
            .invite_with_scopes(Some("reinstalled".to_owned()), 600, vec![Scope::Sync])
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        let files = FleetFiles::new(&fixture.data_dir());
        let _joined = fleet::join(&files, &offer, "reinstalled", DEFAULT_GRANT.to_vec())
            .await
            .unwrap();
        reported_after(&home, "reinstalled", since).await;
        assert!(home.owner.call_on(id, ListSessions).await.is_err());

        let forgot = fleet::forget(&FleetFiles::new(&home.fixture.data_dir()), id)
            .unwrap()
            .unwrap();
        assert_eq!(forgot.devices.len(), 1, "{forgot:?}");
        // Its device was paired before the forget: a restart is not enough
        // (round 3: the forgotten id goes only to a device paired since).
        node.stop().await.unwrap();
        let node = member_node(&relay, &fixture, "desktop").await;
        let since = Utc::now();
        reported_after(&home, "reinstalled", since).await;
        assert!(home.owner.call_on(id, ListSessions).await.is_err());
        // Invited again and joined from there: granted.
        join_again(&home, &fixture, "reinstalled again").await;
        assert!(
            reaches(&home, id).await,
            "still refused after the home forgot it"
        );
        drop(node);
        relay.stop().await;
    })
    .await;
}

/// The review's round 2, finding D: the home held its grant lock while it
/// paired, so a `sync` device whose link named unresponsive routes kept
/// every member's grant waiting for a minute and more. Now pairing is not
/// under the lock, and a device has one grant in progress at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_grant_holds_up_no_other() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let someone = sync_device(&relay, &home)
            .await
            .connect(client())
            .await
            .unwrap();
        let (silent, _dialed) = silent_routes(8).await;
        let (_fixture, _node, owner) = stock_node(&relay, "slow", None).await;
        let mut offer: PairingOffer = link(&owner, "home").await.decode().unwrap();
        offer.direct = Vec::new();
        offer.relays = silent;
        let slow_link = PairingLink::encode(&offer).unwrap();
        let slow = {
            let someone = someone.clone();
            let link = slow_link.clone();
            tokio::spawn(async move { someone.call(GrantHome { link }).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The same device again, meanwhile: refused at once.
        let (refusal, message) = code(someone.call(GrantHome { link: slow_link }).await);
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");
        assert!(message.contains("in progress"), "{message}");

        // Another node joins and is granted while the slow one pairs.
        let started = std::time::Instant::now();
        let late = member_granting(
            &relay,
            &home,
            "late",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "granted after {:?}",
            started.elapsed()
        );
        assert!(!slow.is_finished(), "the slow grant ended early");
        assert!(home.owner.call_on(late.id(), ListSessions).await.is_ok());
        slow.abort();
        relay.stop().await;
    })
    .await;
}

/// `count` websocket routes that accept connections and never answer, and
/// how many connections they took so far.
async fn silent_routes(
    count: usize,
) -> (
    Vec<RouteUrl>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let hole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = hole.local_addr().unwrap().port();
    let dialed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&dialed);
    drop(tokio::spawn(async move {
        let mut kept = Vec::new();
        while let Ok((socket, _peer)) = hole.accept().await {
            let _before = counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            kept.push(socket);
        }
    }));
    let routes = (0..count)
        .map(|index| format!("ws://127.0.0.1:{port}/r{index}").parse().unwrap())
        .collect();
    (routes, dialed)
}

/// The review's round 2, finding C: a member's grant waited on its home
/// without a bound and without heeding the stop signal, holding the grant
/// lock (so `reconcile` stopped revoking). Here the home pairs with the
/// member over routes that never answer: the member's grant is in flight,
/// and stopping the member does not wait for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_a_member_does_not_wait_for_its_grant() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let (silent, dialed) = silent_routes(8).await;
        let fixture = Fixture::new();
        let applied = fixture.cwd().join("member-codex");
        // Reachable only at routes that never answer: what its grant names.
        let node = fixture
            .start_with(|config| {
                config.name = Some("unreachable".to_owned());
                config.public = silent;
                config.fleet = fleet_config(HarnessKind::Codex, applied);
            })
            .await;
        let offer = home
            .owner
            .invite_with_scopes(Some("unreachable".to_owned()), 600, vec![Scope::Sync])
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        let _joined = fleet::join(
            &FleetFiles::new(&fixture.data_dir()),
            &offer,
            "unreachable",
            DEFAULT_GRANT.to_vec(),
        )
        .await
        .unwrap();
        // The home is pairing with it: its grant is in flight.
        for _ in 0..800 {
            if dialed.load(std::sync::atomic::Ordering::Acquire) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(dialed.load(std::sync::atomic::Ordering::Acquire) > 0);
        let stopped = tokio::time::timeout(Duration::from_secs(5), node.stop()).await;
        assert!(stopped.is_ok(), "stopping waited for the grant");
        relay.stop().await;
    })
    .await;
}

/// The review's round 3, finding 1: while a member's device is revoked, a
/// `sync` device's own node (its own id and key) took the member's name;
/// the member was then refused for the clash when it joined again, and
/// calls by that name went to the other node. The name stays the member's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_members_name_stays_reserved() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let someone = sync_device(&relay, &home)
            .await
            .connect(client())
            .await
            .unwrap();
        let victim = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        let member_device = home
            .owner
            .devices()
            .await
            .unwrap()
            .into_iter()
            .find(|device| device.label == "desktop")
            .unwrap()
            .id;
        home.owner.revoke_device(member_device).await.unwrap();

        let (_fixture, _own, owner) = stock_node(&relay, "desktop", None).await;
        let (refusal, message) = code(
            someone
                .call(GrantHome {
                    link: link(&owner, "home").await,
                })
                .await,
        );
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");
        assert!(message.contains("named `desktop`"), "{message}");

        join_again(&home, &victim.fixture, "desktop again").await;
        assert!(
            reaches(&home, victim.id()).await,
            "the member's own grant was refused"
        );
        let named: Vec<vessel_proto::NodeId> = home
            .owner
            .call(ListPeers)
            .await
            .unwrap()
            .into_iter()
            .filter(|peer| peer.name == "desktop")
            .map(|peer| peer.node)
            .collect();
        assert_eq!(named, [victim.id()]);
        relay.stop().await;
    })
    .await;
}

/// The review's round 3, finding 2: right after `vessel fleet forget` the
/// id and the name are known and free, and an impostor's grant took them
/// before the reinstalled node's. Now only a device paired after the
/// forget takes them: devices from before are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_forget_no_earlier_device_takes_the_id_or_the_name() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let someone = sync_device(&relay, &home)
            .await
            .connect(client())
            .await
            .unwrap();
        let victim = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        let _forgot = fleet::forget(&FleetFiles::new(&home.fixture.data_dir()), victim.id())
            .unwrap()
            .unwrap();

        let (_fixture, _impostor, bait) = impostor_link(&relay, victim.id()).await;
        let (refusal, message) = code(someone.call(GrantHome { link: bait }).await);
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");
        assert!(message.contains("forgot"), "{message}");
        let (_fixture, _own, owner) = stock_node(&relay, "desktop", None).await;
        let (refusal, message) = code(
            someone
                .call(GrantHome {
                    link: link(&owner, "home").await,
                })
                .await,
        );
        assert_eq!(refusal, ErrorCode::Conflict, "{message}");
        assert!(message.contains("forgot"), "{message}");

        // The member itself, invited again: granted.
        join_again(&home, &victim.fixture, "desktop again").await;
        assert!(reaches(&home, victim.id()).await);
        relay.stop().await;
    })
    .await;
}
