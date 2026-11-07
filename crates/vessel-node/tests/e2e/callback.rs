//! How a fleet's home calls a member back (`fleet.callback`, Track S's
//! fresh-machine smoke): a member started plainly, with no `--relay` and
//! no `--public`, synced but named no route in its grant, so the home
//! refused the grant and could never call it (`--to`, `vessel mcp`'s
//! `node`, `secret.copy`). It now registers with its home's relays with a
//! ticket for its own key (the review's round 6: never the relays' token);
//! a home with none to share has its members say so, and how to add a
//! route.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use vessel_client::CallError;
use vessel_client::pairing::Paired;
use vessel_client::secure::{RouteTimeouts, pair_over};
use vessel_client::{Connector as _, NodeClient};
use vessel_node::Node;
use vessel_node::fleet::{self, DEFAULT_GRANT, FleetFiles, FleetInit};
use vessel_proto::{
    ErrorCode, GetCallback, GrantHome, HarnessKind, InvitationSecret, ListDevices, ListSessions,
    NodeId, OfferVersion, PairingLink, PairingOffer, RegistrationRefusal, RelayTicket, Scope,
};
use vessel_relay::{LinkState, NodeLink, TlsConfig, unix_now};
use vessel_secure::NodeKeypair;

use crate::across::{Member, home};
use crate::agents::code;
use crate::fleet::{fleet_config, write};
use crate::grants::reaches;
use crate::secure::{Relay, device_via, dialer, direct_route, relayed_with};
use crate::support::{Fixture, client, step, within};

/// A member's node as `vessel node start` runs it: a name, and no route of
/// its own.
async fn plain_member(fixture: &Fixture, name: &str) -> Node {
    let applied = fixture.cwd().join("member-codex");
    fixture
        .start_with(|config| {
            config.name = Some(name.to_owned());
            config.fleet = fleet_config(HarnessKind::Codex, applied);
        })
        .await
}

/// An invitation to `home` for a member named `label`, as `vessel fleet
/// invite` makes one.
async fn invite(home: &Member, label: &str) -> PairingOffer {
    home.owner
        .invite_with_scopes(Some(label.to_owned()), 600, vec![Scope::Sync])
        .await
        .unwrap()
        .link
        .decode()
        .unwrap()
}

/// Join with the defaults, as `vessel init --join` does.
async fn join(fixture: &Fixture, offer: &PairingOffer, label: &str) {
    let _joined = fleet::join(
        &FleetFiles::new(&fixture.data_dir()),
        offer,
        label,
        DEFAULT_GRANT.to_vec(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_started_plainly_is_called_back_through_its_homes_relay() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;

        // `vessel init --join`, then `vessel node start` (or the service).
        let first = Fixture::new();
        join(&first, &invite(&home, "laptop").await, "laptop").await;
        let laptop = plain_member(&first, "laptop").await;
        // `vessel fleet join` while the node runs.
        let second = Fixture::new();
        let desktop = plain_member(&second, "desktop").await;
        join(&second, &invite(&home, "desktop").await, "desktop").await;

        for member in [&laptop, &desktop] {
            let id = member.info().id;
            assert!(reaches(&home, id).await, "the home never reached {id}");
            let linked: Vec<_> = member
                .relay_links()
                .into_iter()
                .map(|(relay, _state)| relay)
                .collect();
            assert_eq!(linked, std::slice::from_ref(&relay.url));
        }
        laptop.stop().await.unwrap();
        desktop.stop().await.unwrap();
        home.node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_of_a_home_without_a_relay_says_it_has_no_route_and_how_to_add_one() {
    within(async {
        let sources = tempfile::tempdir().unwrap();
        let home = direct_home(sources.path()).await;
        let fixture = Fixture::new();
        let mut offer = invite(&home, "laptop").await;
        // The home has no relay and no `--public`: the test names its
        // listener, as `--public` would.
        offer.direct = vec![direct_route(&home.node)];
        join(&fixture, &offer, "laptop").await;
        let member = plain_member(&fixture, "laptop").await;

        let files = FleetFiles::new(&fixture.data_dir());
        let mut said = None;
        for _ in 0..800 {
            said = fleet::status(&files).unwrap().state.grant;
            if said.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let said = said.expect("the member never said why the home did not take its grant");
        assert!(
            said.contains("this node has no route its home could call it back on"),
            "{said}"
        );
        assert!(said.contains("syncing does not need one"), "{said}");
        assert!(
            said.contains("`vesseld --relay URL --relay-token-file FILE`"),
            "{said}"
        );
        assert!(said.contains("`vesseld --public ADDR`"), "{said}");
        assert!(!said.contains("1 to 8 routes"), "{said}");
        assert!(member.relay_links().is_empty());
        member.stop().await.unwrap();
        home.node.stop().await.unwrap();
    })
    .await;
}

/// A home with `fleet.toml` and no relay.
async fn direct_home(sources: &std::path::Path) -> Member {
    let agents = sources.join("AGENTS.md");
    write(&agents, "fleet instructions\n");
    let fixture = Fixture::new();
    let _toml = fleet::init(
        &FleetFiles::new(&fixture.data_dir()),
        &FleetInit {
            instructions: Some(agents),
            skills: None,
            skip: Vec::new(),
        },
    )
    .unwrap();
    let applied = fixture.cwd().join("home-claude");
    let node = fixture
        .start_with(|config| {
            config.name = Some("home".to_owned());
            config.fleet = fleet_config(HarnessKind::ClaudeCode, applied);
        })
        .await;
    let owner = fixture.connect(&node).await;
    Member {
        fixture,
        node,
        owner,
    }
}

/// A device paired on `home` with `sync` only, which never joins or
/// grants, as a client through `relay`; its id there.
async fn stranger(home: &Member, relay: &Relay) -> (NodeClient, vessel_proto::DeviceId) {
    let (paired, id) = pair_stranger(home).await;
    (connect_stranger(&paired, relay).await, id)
}

/// Pair a device with `sync` only on `home`; its credential and id there.
async fn pair_stranger(home: &Member) -> (Paired, vessel_proto::DeviceId) {
    let label = format!("not-a-member-{}", NodeId::random());
    let offer = home
        .owner
        .invite_with_scopes(Some(label.clone()), 600, vec![Scope::Sync])
        .await
        .unwrap()
        .link
        .decode()
        .unwrap();
    let (paired, _route) = pair_over(&dialer(), &offer, "x", RouteTimeouts::default())
        .await
        .unwrap();
    let devices = home.owner.call(ListDevices).await.unwrap();
    let id = devices
        .iter()
        .find(|device| device.label == label)
        .unwrap()
        .id;
    (paired, id)
}

/// A connection as the paired device, through `relay`.
async fn connect_stranger(paired: &Paired, relay: &Relay) -> NodeClient {
    device_via(dialer(), paired, &[], &[&relay.url])
        .connect(client())
        .await
        .unwrap()
}

/// A relay link for `keypair` registering with the tickets on `tickets`,
/// until `stop`; where it stands.
fn ticket_link(
    relay: &Relay,
    keypair: NodeKeypair,
    tickets: watch::Receiver<RelayTicket>,
    stop: &CancellationToken,
) -> watch::Receiver<LinkState> {
    let link = NodeLink::with_tickets(
        relay.url.clone(),
        Arc::new(keypair),
        tickets,
        TlsConfig::with_roots(rustls::RootCertStore::empty()).unwrap(),
    );
    let state = link.state();
    drop(tokio::spawn(link.run(|_carrier| {}, stop.clone())));
    state
}

/// The review's round 6, `poc_r6_who_gets_the_token_and_what_it_registers`:
/// the home handed its relays' token to any `sync` device (one that never
/// joined included), and the device registered any key with it, also after
/// it was revoked. Now no one gets the token: a device that has not
/// granted gets a short ticket for one key its record would take (a
/// member's first grant needs it, its home pairing with it through the
/// relay), and that ticket registers that key and no other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_gets_a_ticket_for_one_key_of_its_own_and_never_the_token() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let own = NodeKeypair::generate().unwrap();
        let asked = GetCallback {
            node: NodeId::random(),
            key: *own.public(),
        };
        // The owner holds no device: no ticket.
        assert_eq!(code(home.owner.call(asked).await), ErrorCode::Forbidden);

        let (stranger, id) = stranger(&home, &relay).await;
        let callback = stranger.call(asked).await.unwrap();
        let text = serde_json::to_string(&callback).unwrap();
        assert!(!text.contains("e2e-relay-token"), "{text}");
        let ticket = callback.ticket.unwrap();
        assert_eq!(ticket.key, *own.public());
        assert!(ticket.expires_at <= unix_now() + 10 * 60);

        // It registers its own key, and none of 25 others.
        let stop = CancellationToken::new();
        let (_issued, tickets) = watch::channel(ticket);
        let mut registered = ticket_link(&relay, own, tickets.clone(), &stop);
        let _registered = step(
            "the ticket's own key registering",
            registered.wait_for(|state| *state == LinkState::Registered),
        )
        .await
        .unwrap();
        for _ in 0..25 {
            let mut other = ticket_link(
                &relay,
                NodeKeypair::generate().unwrap(),
                tickets.clone(),
                &stop,
            );
            let _refused = step(
                "another key refused with the ticket",
                other.wait_for(|state| *state == LinkState::Refused(RegistrationRefusal::Ticket)),
            )
            .await
            .unwrap();
        }
        // A second key, for the same device: refused.
        let second = GetCallback {
            node: NodeId::random(),
            key: *NodeKeypair::generate().unwrap().public(),
        };
        assert_eq!(code(stranger.call(second).await), ErrorCode::Forbidden);
        // Revoked, it asks no more.
        home.owner.revoke_device(id).await.unwrap();
        assert!(stranger.call(asked).await.is_err());
        stop.cancel();
        home.node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

/// The review's round 6, `poc_r6_a_member_moving_homes_keeps_the_old_homes_relay`:
/// a member with no route that left one home for another kept the first
/// home's relay as its only route, never asked the second, and the second
/// home's grant named the first home's relay. Now the second home's
/// relays replace the first's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_moving_homes_drops_the_old_homes_relay() {
    within(async {
        let (first_relay, second_relay) = (Relay::start().await, Relay::start().await);
        let (first_sources, second_sources) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let first = home(&first_relay, first_sources.path()).await;
        let second = home(&second_relay, second_sources.path()).await;
        let fixture = Fixture::new();
        let member = plain_member(&fixture, "laptop").await;
        let id = member.info().id;
        join(&fixture, &invite(&first, "laptop").await, "laptop").await;
        assert!(reaches(&first, id).await, "the first home never reached it");

        assert!(fleet::leave(&FleetFiles::new(&fixture.data_dir())).unwrap());
        join(&fixture, &invite(&second, "laptop").await, "laptop").await;
        assert!(
            reaches(&second, id).await,
            "the second home never reached it"
        );
        let linked: Vec<_> = member
            .relay_links()
            .into_iter()
            .map(|(relay, _state)| relay)
            .collect();
        assert_eq!(linked, std::slice::from_ref(&second_relay.url));
        let grants = FleetFiles::new(&second.fixture.data_dir())
            .dir()
            .join("grants");
        for entry in std::fs::read_dir(&grants).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let grant = std::fs::read_to_string(&path).unwrap();
                assert!(grant.contains(second_relay.url.as_str()), "{grant}");
                assert!(!grant.contains(first_relay.url.as_str()), "{grant}");
            }
        }
        // The first home's relay is not needed any more.
        first_relay.stop().await;
        assert!(second.owner.call_on(id, ListSessions).await.is_ok());
        member.stop().await.unwrap();
        first.node.stop().await.unwrap();
        second.node.stop().await.unwrap();
        second_relay.stop().await;
    })
    .await;
}

/// A home with `fleet.toml` in `fixture`, linked to `relay`, giving first
/// relay tickets for `window` after a device was paired; started again
/// the same way after a stop.
async fn start_home(fixture: &Fixture, relay: &Relay, window: Duration) -> Node {
    if !FleetFiles::new(&fixture.data_dir()).sources().exists() {
        let agents = fixture.cwd().join("sources/AGENTS.md");
        write(&agents, "fleet instructions\n");
        let _toml = fleet::init(
            &FleetFiles::new(&fixture.data_dir()),
            &FleetInit {
                instructions: Some(agents),
                skills: None,
                skip: Vec::new(),
            },
        )
        .unwrap();
    }
    let applied = fixture.cwd().join("home-claude");
    relayed_with(fixture, &[&relay.url], |config| {
        config.name = Some("home".to_owned());
        config.fleet = fleet_config(HarnessKind::ClaudeCode, applied);
        config.fleet.first_ticket_window = window;
    })
    .await
}

/// A random node id and key to ask a ticket for.
fn ask() -> GetCallback {
    GetCallback {
        node: NodeId::random(),
        key: *NodeKeypair::generate().unwrap().public(),
    }
}

/// The review's round 7, `poc_r7_a_stranger_renews_a_first_ticket_forever`:
/// a `sync` device that never joins renewed a ten-minute first ticket for
/// the same key as often as it asked, keeping a relay place for good. A
/// first ticket now comes only within a window after the device was paired
/// (an hour), and never reaches past it.
///
/// Time is moved, not waited for: the device's pairing is set back in the
/// home's record, which the window is measured from, so the test holds on a
/// slow machine (it once slept across a two-second window and failed under
/// load).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_ticket_comes_only_within_its_window_after_pairing() {
    within(async {
        let relay = Relay::start().await;
        let fixture = Fixture::new();
        let window = Duration::from_secs(3600);
        let node = start_home(&fixture, &relay, window).await;
        let home = Member {
            owner: fixture.connect(&node).await,
            fixture,
            node,
        };
        let (stranger, id) = stranger(&home, &relay).await;
        let asked = ask();

        // A minute of the window left: the ticket ends with it, not ten
        // minutes on.
        pair_earlier(
            &home.fixture,
            id,
            window.saturating_sub(Duration::from_secs(60)),
        );
        let first = stranger.call(asked).await.unwrap().ticket.unwrap();
        assert!(
            first.expires_at <= unix_now() + 60,
            "the ticket reaches past the window"
        );
        assert!(
            stranger.call(asked).await.is_ok(),
            "renewed within the window"
        );

        // Two minutes later in the device's life: the window has closed.
        pair_earlier(&home.fixture, id, Duration::from_secs(120));
        assert_eq!(code(stranger.call(asked).await), ErrorCode::Forbidden);
        home.node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

/// Set device `id`'s pairing `by` earlier in the home's database, as if it
/// had been paired that much longer ago. The node reads the pairing time
/// from the database on every ticket it considers, so a running node sees
/// the change.
fn pair_earlier(fixture: &Fixture, id: vessel_proto::DeviceId, by: Duration) {
    let database = rusqlite::Connection::open(fixture.data_dir().database_path()).unwrap();
    database.busy_timeout(Duration::from_secs(10)).unwrap();
    let nanos = i64::try_from(by.as_nanos()).unwrap();
    let changed = database
        .execute(
            "UPDATE devices SET created_at = created_at - ?1 WHERE id = ?2",
            (nanos, id.to_string()),
        )
        .unwrap();
    assert_eq!(changed, 1, "no device {id} in the home's database");
}

/// The review's round 7: the one key a device that has not granted may take
/// a first ticket for was kept in memory, so after the home's restart it
/// could take another. It is kept in the home's record now, and the
/// device's grant must claim that node and key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_devices_first_ticket_key_holds_across_a_restart_and_for_its_grant() {
    within(async {
        let relay = Relay::start().await;
        let fixture = Fixture::new();
        let window = Duration::from_secs(3600);
        let node = start_home(&fixture, &relay, window).await;
        let home = Member {
            owner: fixture.connect(&node).await,
            fixture,
            node,
        };
        let (paired, _id) = pair_stranger(&home).await;
        let stranger = connect_stranger(&paired, &relay).await;
        let (chosen, other) = (ask(), ask());
        assert!(stranger.call(chosen).await.is_ok());
        assert_eq!(code(stranger.call(other).await), ErrorCode::Forbidden);

        // The home restarts: still the same one key.
        drop(stranger);
        let Member { fixture, node, .. } = home;
        node.stop().await.unwrap();
        let node = start_home(&fixture, &relay, window).await;
        let stranger = connect_stranger(&paired, &relay).await;
        assert_eq!(code(stranger.call(other).await), ErrorCode::Forbidden);
        assert!(stranger.call(chosen).await.is_ok());

        // Its grant must be that node's, with that key: a link for another
        // is refused before the home dials anything.
        let link = PairingLink::encode(&PairingOffer {
            v: OfferVersion,
            node: other.node,
            key: other.key,
            relays: vec![relay.url.clone()],
            direct: Vec::new(),
            invite: InvitationSecret::from_bytes([3; 16]),
            expires: chrono::Utc::now() + chrono::TimeDelta::minutes(10),
        })
        .unwrap();
        let refused = stranger.call(GrantHome { link }).await;
        let Err(CallError::Remote(error)) = refused else {
            panic!("a grant for another key was not refused: {refused:?}");
        };
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(
            error
                .message
                .contains("took a relay ticket on this home for another node or key"),
            "{}",
            error.message
        );
        node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}
