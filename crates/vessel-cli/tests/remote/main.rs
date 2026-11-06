//! `vessel` with SSH nodes, end to end: the real `vesseld` (built by
//! `vessel_testkit::vesseld`, next to the `vessel` binary, which is how a
//! development checkout installs itself) on a "remote" that is a temporary
//! home reached through `fake-ssh`.
//!
//! Unix only: the remote side of milestone 2 is (Windows remotes are out of
//! scope), and `fake-ssh` needs `sh`. Every test stops the node it started;
//! a guard kills it if the test fails first.
#![cfg(unix)]

use std::ffi::OsString;

use support::{Captured, Machine, within};

#[cfg(test)]
mod support;

/// A fresh machine: the first command installs `vesseld` (the local
/// build, by content hash), `attach` starts the node, and `run` and
/// `attach` work as on a local node. The node's id is pinned; a different
/// node answering later is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_machine_is_installed_started_and_used() {
    within(async {
        let machine = Machine::new();
        let added = machine
            .vessel(&["nodes", "add", "test", "me@gpu-box"], b"")
            .await;
        assert!(added.ok, "{}", added.stderr);

        let home = machine.home.path().to_str().unwrap().to_owned();
        let run = machine
            .vessel(
                &[
                    "--node", "test", "run", "claude", "--detach", "--cwd", &home,
                ],
                b"",
            )
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();
        let installed = machine.installed();
        assert_eq!(installed.len(), 1, "{installed:?}");
        assert!(installed[0].contains("-dev-"), "{installed:?}");

        let attach = machine
            .vessel(&["--node", "test", "attach", &id], b"over ssh\rexit\r")
            .await;
        assert!(attach.ok, "{}", attach.stderr);
        assert!(
            attach.stdout.contains("echo: over ssh"),
            "{}",
            attach.stdout
        );

        let status = machine
            .vessel(&["--node", "test", "node", "status"], b"")
            .await;
        assert!(status.ok, "{}", status.stderr);
        assert!(
            status.stdout.contains("state     serving"),
            "{}",
            status.stdout
        );

        let shown = machine.vessel(&["nodes", "show", "test"], b"").await;
        let id_line = shown
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .unwrap()
            .to_owned();
        let book = std::fs::read_to_string(machine.book()).unwrap();
        let other = vessel_proto::NodeId::random().to_string();
        std::fs::write(machine.book(), book.replace(&id_line, &other)).unwrap();
        let refused = machine.vessel(&["--node", "test", "sessions"], b"").await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("a different node answered"),
            "{}",
            refused.stderr
        );
        machine.stop_node().await;
    })
    .await;
}

/// `vessel node update` with a new build: installed side by side (the
/// running one stays), refused while a session is live, and with
/// `--when-idle` the node drains and stops once the session ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_installs_side_by_side_refuses_then_drains() {
    within(async {
        let machine = Machine::new();
        let home = machine.home.path().to_str().unwrap().to_owned();
        let run = machine
            .vessel(
                &[
                    "--node",
                    "me@gpu-box",
                    "run",
                    "claude",
                    "--detach",
                    "--cwd",
                    &home,
                ],
                b"",
            )
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();
        let running = machine.installed();

        // A different build of the same version: trailing bytes change its
        // hash and nothing else.
        let next = machine.local.path().join("vesseld-next");
        let mut bytes = std::fs::read(vessel_testkit::vesseld().unwrap()).unwrap();
        bytes.extend_from_slice(b"another build");
        std::fs::write(&next, &bytes).unwrap();
        std::fs::set_permissions(&next, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let next = next.to_str().unwrap().to_owned();

        let refused = machine
            .vessel(
                &["--node", "me@gpu-box", "node", "update", "--vesseld", &next],
                b"",
            )
            .await;
        assert!(!refused.ok);
        assert!(refused.stdout.contains(&id), "{}", refused.stdout);
        assert!(
            refused.stderr.contains("live session"),
            "{}",
            refused.stderr
        );
        let both = machine.installed();
        assert_eq!(both.len(), 2, "{both:?}");
        assert!(
            both.contains(&running[0]),
            "cleanup kept the running node's binary"
        );

        let draining = machine
            .vessel(
                &[
                    "--node",
                    "me@gpu-box",
                    "node",
                    "update",
                    "--vesseld",
                    &next,
                    "--when-idle",
                ],
                b"",
            )
            .await;
        assert!(draining.ok, "{}", draining.stderr);
        assert!(draining.stdout.contains("draining"), "{}", draining.stdout);
        let status = machine
            .vessel(&["--node", "me@gpu-box", "node", "status"], b"")
            .await;
        assert!(status.stdout.contains("draining"), "{}", status.stdout);
        let refused_run = machine
            .vessel(
                &[
                    "--node",
                    "me@gpu-box",
                    "run",
                    "claude",
                    "--detach",
                    "--cwd",
                    &home,
                ],
                b"",
            )
            .await;
        assert!(!refused_run.ok, "a draining node takes no new session");

        // The last session ends; the node stops by itself.
        let killed = machine
            .vessel(&["--node", "me@gpu-box", "kill", &id], b"")
            .await;
        assert!(killed.ok, "{}", killed.stderr);
        machine.node_gone().await;
    })
    .await;
}

/// The SSH connection drops in the middle of a terminal attach: the CLI
/// says so, reconnects (a new `ssh`, a new `attach`), and the screen goes
/// on with nothing repeated or skipped.
///
/// The session's output is produced first, over a connection that does
/// not drop; the dropping attach then replays it and drops part way. No
/// input is typed while connections drop: input in flight at a drop may
/// be lost (it is reported, never resent), which would make the test
/// depend on timing. The session is ended with `vessel kill`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_ssh_connection_during_attach_resumes() {
    within(async {
        let machine = Machine::new();
        let home = machine.home.path().to_str().unwrap().to_owned();
        let node = ["--node", "me@gpu-box"];
        let run = machine
            .vessel(
                &[&node[..], &["run", "claude", "--detach", "--cwd", &home]].concat(),
                b"",
            )
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();
        for line in 0..4 {
            let text = format!("big{line}-{}", "x".repeat(4000));
            let sent = machine
                .vessel(&[&node[..], &["send", &id, &text]].concat(), b"")
                .await;
            assert!(sent.ok, "{}", sent.stderr);
        }

        // Every connection of this attach drops after 24 KiB of node
        // output: the four echoes are more than that, any one of them
        // much less.
        let drop_after = [("FAKE_SSH_DROP_AFTER", OsString::from("24576"))];
        let mut child = machine.spawn(&[&node[..], &["attach", &id]].concat(), &drop_after);
        let stdin = child.stdin.take().unwrap();
        let mut stdout = Captured::start(child.stdout.take().unwrap());
        let mut stderr = Captured::start(child.stderr.take().unwrap());
        for line in 0..4 {
            stdout.wait_for(&format!("echo: big{line}-")).await;
        }
        stderr.wait_for("connection lost; reconnecting").await;
        stderr.wait_for("reconnected").await;

        let killed = machine
            .vessel(&[&node[..], &["kill", &id]].concat(), b"")
            .await;
        assert!(killed.ok, "{}", killed.stderr);
        drop(stdin);
        let status = child.wait().await.unwrap();
        assert!(status.success(), "{}", stderr.text());
        let shown = stdout.text();
        for line in 0..4 {
            let echo = format!("echo: big{line}-");
            assert_eq!(shown.matches(&echo).count(), 1, "{echo} in {shown}");
        }
        machine.stop_node().await;
    })
    .await;
}

/// `vessel node update --now` interrupts live sessions only when
/// confirmed: not at a terminal that takes `--yes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_now_needs_a_confirmation_then_stops_the_node() {
    within(async {
        let machine = Machine::new();
        let home = machine.home.path().to_str().unwrap().to_owned();
        let node = ["--node", "me@gpu-box"];
        let run = machine
            .vessel(
                &[&node[..], &["run", "claude", "--detach", "--cwd", &home]].concat(),
                b"",
            )
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();

        let next = machine.local.path().join("vesseld-next");
        let mut bytes = std::fs::read(vessel_testkit::vesseld().unwrap()).unwrap();
        bytes.extend_from_slice(b"yet another build");
        std::fs::write(&next, &bytes).unwrap();
        std::fs::set_permissions(&next, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let next = next.to_str().unwrap().to_owned();
        let update = |extra: &'static [&'static str]| {
            [
                &node[..],
                &["node", "update", "--vesseld", &next, "--now"],
                extra,
            ]
            .concat()
        };

        let unconfirmed = machine.vessel(&update(&[]), b"").await;
        assert!(!unconfirmed.ok);
        assert!(
            unconfirmed.stderr.contains("--yes"),
            "{}",
            unconfirmed.stderr
        );

        let stopped = machine.vessel(&update(&["--yes"]), b"").await;
        assert!(stopped.ok, "{}", stopped.stderr);
        assert!(
            stopped.stdout.contains("stopped vesseld"),
            "{}",
            stopped.stdout
        );
        machine.node_gone().await;

        // The next command starts the node again; the session it had was
        // interrupted.
        let sessions = machine
            .vessel(&[&node[..], &["sessions"]].concat(), b"")
            .await;
        assert!(sessions.ok, "{}", sessions.stderr);
        let line = sessions
            .stdout
            .lines()
            .find(|line| line.contains(&id))
            .unwrap_or_else(|| panic!("{}", sessions.stdout));
        assert!(line.contains("interrupted"), "{line}");
        machine.stop_node().await;
    })
    .await;
}
