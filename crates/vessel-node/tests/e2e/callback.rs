//! How a fleet's home calls a member back (`fleet.callback`, Track S's
//! fresh-machine smoke): a member started plainly, with no `--relay` and
//! no `--public`, synced but named no route in its grant, so the home
//! refused the grant and could never call it (`--to`, `vessel mcp`'s
//! `node`, `secret.copy`). It now registers with its home's relays; a home
//! with none to share has its members say so, and how to add a route.

use std::time::Duration;

use vessel_node::Node;
use vessel_node::fleet::{self, DEFAULT_GRANT, FleetFiles, FleetInit};
use vessel_proto::{HarnessKind, PairingOffer, Scope};

use crate::across::{Member, home};
use crate::fleet::{fleet_config, write};
use crate::grants::reaches;
use crate::secure::{Relay, direct_route};
use crate::support::{Fixture, within};

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
