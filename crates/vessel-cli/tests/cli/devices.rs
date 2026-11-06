//! `vessel node pair` and `vessel node devices` against a node (ADR 0010).

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
