//! `vessel node pair` and `vessel node devices` against a node (ADR 0010).

use crate::support::{Fixture, within};

/// A node with no devices lists none; revoking an unknown id says so; and a
/// node without a public key yet (until `vessel-secure` derives it) refuses
/// to make a pairing link with a reason rather than a broken link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices_are_listed_and_pairing_needs_a_node_key() {
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
        assert!(!paired.ok);
        assert!(
            paired.stderr.contains("cannot pair devices"),
            "{}",
            paired.stderr
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
