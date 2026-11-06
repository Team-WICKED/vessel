//! `vessel fleet` (ADR 0012): making a node the home, inviting a node,
//! joining from a piped link through a relay, listing members, leaving.

use std::path::Path;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use vessel_node::{FleetConfig, FleetTargets};
use vessel_proto::{HarnessKind, RelayToken, RouteUrl};
use vessel_relay::{LinkState, RelayConfig};

use crate::support::{Fixture, vessel_in, within};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A node linked to an in-process relay, writing fleet files into
/// `claude` only.
pub(crate) struct RelayedHome {
    pub(crate) fixture: Fixture,
    shutdown: CancellationToken,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RelayedHome {
    pub(crate) async fn start(claude: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay: RouteUrl = format!("ws://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let shutdown = CancellationToken::new();
        let serving = tokio::spawn(vessel_relay::serve(
            listener,
            RelayConfig::new(RelayToken::new("cli-relay").unwrap()),
            shutdown.clone(),
        ));
        std::fs::create_dir_all(claude).unwrap();
        let fixture = Fixture::start_with(|config, data| {
            let token = data.join("relay-token");
            std::fs::write(&token, "cli-relay\n").unwrap();
            config.relays = vec![relay.clone()];
            config.relay_token_file = Some(token);
            config.fleet = FleetConfig {
                targets: FleetTargets::Only(vec![(HarnessKind::ClaudeCode, claude.to_owned())]),
                poll: Duration::from_millis(50),
                ..FleetConfig::default()
            };
        })
        .await;
        for (_relay, mut state) in fixture.node().relay_links() {
            let _registered = state
                .wait_for(|state| *state == LinkState::Registered)
                .await
                .unwrap();
        }
        Self {
            fixture,
            shutdown,
            serving,
        }
    }

    pub(crate) async fn stop(self) {
        self.fixture.stop().await;
        self.shutdown.cancel();
        self.serving.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_home_invites_a_node_that_joins_from_a_piped_link() {
    within(async {
        let homes = tempfile::tempdir().unwrap();
        let home_claude = homes.path().join("home-claude");
        let home = RelayedHome::start(&home_claude).await;
        let fixture = &home.fixture;

        init_home(fixture, &homes.path().join("sources"), &home_claude).await;

        // `invite` prints a link alone on stdout; `join` takes it piped.
        let invited = fixture
            .vessel(&["fleet", "invite", "--label", "laptop"], &[])
            .await;
        assert!(invited.ok, "{}", invited.stderr);
        assert!(
            invited.stderr.contains("vessel fleet join"),
            "{}",
            invited.stderr
        );
        let link = invited.stdout.trim().to_owned();
        let member = tempfile::tempdir().unwrap();
        let unconfirmed = vessel_in(member.path(), &["fleet", "join"], link.as_bytes()).await;
        assert!(!unconfirmed.ok);
        assert!(
            unconfirmed.stderr.contains("--yes"),
            "{}",
            unconfirmed.stderr
        );
        let joined = vessel_in(
            member.path(),
            &["fleet", "join", "--yes"],
            format!("{link}\n").as_bytes(),
        )
        .await;
        assert!(joined.ok, "{}", joined.stderr);
        assert!(
            joined.stderr.contains("joined the fleet of"),
            "{}",
            joined.stderr
        );
        let id = fixture.node().info().id.to_string();
        let status = vessel_in(member.path(), &["fleet", "status"], &[]).await;
        assert!(
            status
                .stdout
                .contains(&format!("role       member of {id}")),
            "{}",
            status.stdout
        );
        let refused = vessel_in(member.path(), &["fleet", "init", "--skills", "/x"], &[]).await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("member of a fleet"),
            "{}",
            refused.stderr
        );

        // The home lists it; it has not reported (its node is not running).
        let members = fixture.vessel(&["fleet", "members"], &[]).await;
        assert!(members.ok, "{}", members.stderr);
        assert!(members.stdout.contains("laptop"), "{}", members.stdout);
        assert!(
            members.stdout.contains("never reported"),
            "{}",
            members.stdout
        );

        let left = vessel_in(member.path(), &["fleet", "leave"], &[]).await;
        assert!(left.ok, "{}", left.stderr);
        assert!(left.stderr.contains("left the fleet"), "{}", left.stderr);
        let status = vessel_in(member.path(), &["fleet", "status"], &[]).await;
        assert!(
            status.stdout.contains("role       none"),
            "{}",
            status.stdout
        );

        home.stop().await;
    })
    .await;
}

/// `vessel fleet init` makes the running node of `fixture` the home of
/// `sources`; `fleet status` then shows it, once it applied to `claude`.
pub(crate) async fn init_home(fixture: &Fixture, sources: &Path, home_claude: &Path) {
    write(&sources.join("AGENTS.md"), "fleet instructions\n");
    std::fs::create_dir_all(sources.join("skills")).unwrap();
    let empty = vessel_in(fixture.data(), &["fleet", "init"], &[]).await;
    assert!(!empty.ok);
    assert!(
        empty.stderr.contains("give the instructions file"),
        "{}",
        empty.stderr
    );
    let made = vessel_in(
        fixture.data(),
        &[
            "fleet",
            "init",
            "--instructions",
            sources.join("AGENTS.md").to_str().unwrap(),
            "--skills",
            sources.join("skills").to_str().unwrap(),
            "--skip",
            "paseo*",
        ],
        &[],
    )
    .await;
    assert!(made.ok, "{}", made.stderr);
    let toml = std::fs::read_to_string(fixture.data().join("fleet.toml")).unwrap();
    assert!(toml.contains("skip = [\"paseo*\"]"), "{toml}");
    // The state is written after the files it records: wait for it.
    let files = vessel_node::fleet::FleetFiles::new(&vessel_node::DataDir::new(fixture.data()));
    for _ in 0..400 {
        let applied = vessel_node::fleet::status(&files)
            .is_ok_and(|status| status.state.generation.is_some());
        if applied {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(home_claude.join("CLAUDE.md").is_file());
    let status = vessel_in(fixture.data(), &["fleet", "status"], &[]).await;
    assert!(status.ok, "{}", status.stderr);
    assert!(
        status.stdout.contains("role       home ("),
        "{}",
        status.stdout
    );
    assert!(status.stdout.contains("generation 1"), "{}", status.stdout);
}
