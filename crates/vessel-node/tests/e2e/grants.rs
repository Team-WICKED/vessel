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

use vessel_client::Connector as _;
use vessel_client::secure::{RouteTimeouts, pair_over};
use vessel_node::fleet::DEFAULT_GRANT;
use vessel_proto::{
    ErrorCode, GrantHome, ListPeers, ListSessions, PairingLink, PairingOffer, RouteUrl, Scope,
};

use crate::across::{Member, home, member_granting};
use crate::secure::{Relay, closed_route, device_via, dialer, relayed_with};
use crate::support::{Fixture, client, structured_spec, within};

/// A node of its own on `relay`, named `name`, whose `node.id` is `id`
/// when given (a stock node: only the file differs).
async fn stock_node(
    relay: &Relay,
    name: &str,
    id: Option<vessel_proto::NodeId>,
) -> (Fixture, vessel_node::Node, vessel_client::NodeClient) {
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
async fn link(owner: &vessel_client::NodeClient, label: &str) -> PairingLink {
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

/// A device on `home` with only `sync`, connected.
async fn sync_device(
    relay: &Relay,
    home: &Member,
) -> std::sync::Arc<impl vessel_client::Connector> {
    let offer = home
        .owner
        .invite_with_scopes(Some("someone".to_owned()), 600, vec![Scope::Sync])
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
        let (_fixture, _impostor, impostor) =
            stock_node(&relay, "desktop-too", Some(victim.id())).await;
        let refused = someone
            .call(GrantHome {
                link: link(&impostor, "bait").await,
            })
            .await;
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
