//! `vessel node pair` and `vessel node devices` against a node (ADR 0010).

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use vessel_proto::{RelayToken, RouteUrl};
use vessel_relay::{LinkState, RelayConfig};

use crate::support::{Fixture, within};

/// A node with no devices lists none; revoking an unknown id says so;
/// `node pair` prints a link for this node's key (alone on stdout when it
/// is not a terminal, so it pipes cleanly) and the instructions on stderr;
/// and `node status` shows the key's fingerprint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices_are_listed_and_pairing_makes_a_link_for_this_node() {
    within(async {
        let fixture = Fixture::start().await;
        let listed = fixture.vessel(&["node", "devices"], &[]).await;
        assert!(listed.ok, "{}", listed.stderr);
        assert!(
            listed.stdout.contains("no paired devices"),
            "{}",
            listed.stdout
        );

        let revoked = fixture
            .vessel(&["node", "devices", "revoke", "3fa9"], &[])
            .await;
        assert!(!revoked.ok);
        assert!(
            revoked.stderr.contains("no device matches"),
            "{}",
            revoked.stderr
        );

        let paired = fixture
            .vessel(&["node", "pair", "--label", "phone", "--ttl", "5m"], &[])
            .await;
        assert!(paired.ok, "{}", paired.stderr);
        let link: vessel_proto::PairingLink = paired.stdout.trim().parse().unwrap();
        assert_eq!(paired.stdout.lines().count(), 1, "{}", paired.stdout);
        let offer = link.decode().unwrap();
        assert!(paired.stderr.contains("valid once"), "{}", paired.stderr);

        let status = fixture.vessel(&["node", "status"], &[]).await;
        assert!(status.ok, "{}", status.stderr);
        assert!(
            status
                .stdout
                .contains(&vessel_client::fingerprint(&offer.key)),
            "{}",
            status.stdout
        );

        let bad_ttl = fixture.vessel(&["node", "pair", "--ttl", "2d"], &[]).await;
        assert!(!bad_ttl.ok);
        assert!(
            bad_ttl.stderr.contains("not a duration"),
            "{}",
            bad_ttl.stderr
        );
        fixture.stop().await;
    })
    .await;
}

/// Plan P4: a device pairs from a link piped into `vessel nodes add --pair
/// --yes` (never from the command line), and then reaches the node
/// through a relay by its book name: status, run, sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_pairs_from_a_piped_link_and_runs_through_the_relay() {
    within(async {
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
        let fixture = Fixture::start_with(|config, data| {
            let token = data.join("relay-token");
            std::fs::write(&token, "cli-relay\n").unwrap();
            config.relays = vec![relay.clone()];
            config.relay_token_file = Some(token);
        })
        .await;
        for (_relay, mut state) in fixture.node().relay_links() {
            let _registered = state
                .wait_for(|state| *state == LinkState::Registered)
                .await
                .unwrap();
        }

        let invited = fixture.vessel(&["node", "pair", "--ttl", "5m"], &[]).await;
        assert!(invited.ok, "{}", invited.stderr);
        assert!(invited.stderr.contains("--pair\n"), "{}", invited.stderr);
        let link = invited.stdout.trim().to_owned();
        let key = link
            .parse::<vessel_proto::PairingLink>()
            .unwrap()
            .decode()
            .unwrap()
            .key;

        // Piped, there is no terminal to confirm the key: --yes is needed.
        let add = ["nodes", "add", "gpu-box", "--pair"];
        let unconfirmed = fixture.vessel_book_input(&add, link.as_bytes()).await;
        assert!(!unconfirmed.ok);
        assert!(
            unconfirmed.stderr.contains("--yes"),
            "{}",
            unconfirmed.stderr
        );
        let add = ["nodes", "add", "gpu-box", "--pair", "--yes"];
        let added = fixture
            .vessel_book_input(&add, format!("{link}\n").as_bytes())
            .await;
        assert!(added.ok, "{}", added.stderr);
        let fingerprint = vessel_client::fingerprint(&key);
        assert!(added.stderr.contains(&fingerprint), "{}", added.stderr);
        assert!(added.stderr.contains("paired with"), "{}", added.stderr);
        let book = std::fs::read_to_string(fixture.book()).unwrap();
        assert!(book.contains(&format!("relay = [\"{relay}\"]")), "{book}");
        let id = fixture.node().info().id;
        let credential = fixture
            .data()
            .join("config")
            .join("vessel")
            .join("credentials")
            .join(format!("{id}.secret"));
        assert!(credential.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&credential).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let status = fixture
            .vessel_book_input(&["--node", "gpu-box", "node", "status"], b"")
            .await;
        assert!(status.ok, "{}", status.stderr);
        assert!(status.stdout.contains(&fingerprint), "{}", status.stdout);
        let cwd = fixture.cwd();
        let run = [
            "--node", "gpu-box", "run", "claude", "--detach", "--cwd", &cwd,
        ];
        let run = fixture.vessel_book_input(&run, b"").await;
        assert!(run.ok, "{}", run.stderr);
        let session = run.stdout.trim().to_owned();
        let sessions = fixture
            .vessel_book_input(&["--node", "gpu-box", "sessions"], b"")
            .await;
        assert!(sessions.ok, "{}", sessions.stderr);
        assert!(
            sessions.stdout.contains(session.get(..8).unwrap()),
            "{}",
            sessions.stdout
        );
        let devices = fixture.vessel(&["node", "devices"], &[]).await;
        assert!(devices.stdout.lines().count() >= 2, "{}", devices.stdout);

        fixture.stop().await;
        shutdown.cancel();
        serving.await.unwrap().unwrap();
    })
    .await;
}
