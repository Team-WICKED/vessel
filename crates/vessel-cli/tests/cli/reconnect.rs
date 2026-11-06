//! An attach that survives a lost connection (milestone 2, C1/C6).

use crate::support::{Captured, Fixture, Proxy, within};

/// A connection lost during a terminal attach (the network between client
/// and node dropped): the CLI says so, reconnects, and goes on where it
/// was, with nothing shown twice or skipped and input working again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_attach_survives_a_lost_connection() {
    use tokio::io::AsyncWriteExt as _;

    within(async {
        let fixture = Fixture::start().await;
        let id = fixture.run_detached().await;
        let proxy = Proxy::start(fixture.node_address().parse().unwrap()).await;
        let node = proxy.address.to_string();
        let mut child = fixture.spawn_vessel_at(&node, &["attach", &id]);
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = Captured::start(child.stdout.take().unwrap());
        let mut stderr = Captured::start(child.stderr.take().unwrap());

        stdin.write_all(b"one\r").await.unwrap();
        stdout.wait_for("echo: one", 1).await;

        proxy.cut();
        stderr.wait_for("connection lost; reconnecting", 1).await;
        stderr.wait_for("reconnected", 1).await;

        stdin.write_all(b"two\r").await.unwrap();
        stdout.wait_for("echo: two", 1).await;
        stdin.write_all(b"exit\r").await.unwrap();
        drop(stdin);
        let status = child.wait().await.unwrap();
        assert!(status.success(), "{}", stderr.text());
        let shown = stdout.text();
        assert_eq!(shown.matches("echo: one").count(), 1, "{shown}");
        assert_eq!(shown.matches("echo: two").count(), 1, "{shown}");
        assert!(
            stderr.text().contains("session exited (0)"),
            "{}",
            stderr.text()
        );
        fixture.stop().await;
    })
    .await;
}
