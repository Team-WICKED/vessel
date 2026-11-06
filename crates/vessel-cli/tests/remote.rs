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
mod support {
    use std::ffi::OsString;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt as _;
    use tokio::process::{Child, Command};
    use vessel_harness::fakes::{Role, install};
    use vessel_proto::HarnessKind;
    use vessel_store::{DataDir, NodeLock};
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(120);

    /// Build what the tests run (the fakes, and `vesseld` next to `vessel`)
    /// before the clock starts.
    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        let vesseld = vessel_testkit::vesseld().unwrap();
        assert_eq!(
            vesseld.parent(),
            Path::new(env!("CARGO_BIN_EXE_vessel")).parent(),
            "vesseld must sit next to vessel for `--from auto`"
        );
        vessel_testkit::within(SAFETY_NET, Box::pin(test))
            .await
            .expect("test timed out")
    }

    pub(super) struct Ran {
        pub(super) ok: bool,
        pub(super) stdout: String,
        pub(super) stderr: String,
    }

    impl From<Output> for Ran {
        fn from(output: Output) -> Self {
            Self {
                ok: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
    }

    /// A remote machine (a home with `fake-tui` installed as `claude`, on the
    /// `PATH` its login profile sets) and this machine's `vessel` state.
    pub(super) struct Machine {
        pub(super) home: TempDir,
        _bin: TempDir,
        pub(super) local: TempDir,
    }

    impl Machine {
        pub(super) fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let bin = tempfile::tempdir().unwrap();
            let tui = Fake::Tui.path().unwrap();
            drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
            let profile = format!("export PATH='{}':\"$PATH\"\n", bin.path().display());
            std::fs::write(home.path().join(".profile"), profile).unwrap();
            Self {
                home,
                _bin: bin,
                local: tempfile::tempdir().unwrap(),
            }
        }

        pub(super) fn remote_data(&self) -> DataDir {
            DataDir::new(self.home.path().join(".vessel"))
        }

        pub(super) fn book(&self) -> PathBuf {
            self.local
                .path()
                .join("config")
                .join("vessel")
                .join("nodes.toml")
        }

        fn command(&self, args: &[&str], env: &[(&str, OsString)]) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_vessel"));
            let _ = command
                .arg("--data-dir")
                .arg(self.local.path().join("data"))
                .args(args)
                .current_dir(self.local.path())
                .env("VESSEL_CONFIG_DIR", self.local.path().join("config"))
                .env("VESSEL_SSH", Fake::Ssh.path().unwrap())
                .env("FAKE_SSH_HOME", self.home.path())
                .env_remove("VESSEL_NODE")
                .env_remove("VESSEL_TOKEN")
                .env_remove("FAKE_SSH_DROP_AFTER")
                .kill_on_drop(true);
            for (key, value) in env {
                let _ = command.env(key, value);
            }
            command
        }

        pub(super) async fn vessel(&self, args: &[&str], stdin: &[u8]) -> Ran {
            let mut child = self
                .command(args, &[])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            input.write_all(stdin).await.unwrap();
            drop(input);
            child.wait_with_output().await.unwrap().into()
        }

        pub(super) fn spawn(&self, args: &[&str], env: &[(&str, OsString)]) -> Child {
            self.command(args, env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        }

        /// The names in the remote `~/.vessel/bin`, sorted.
        pub(super) fn installed(&self) -> Vec<String> {
            let mut names: Vec<_> = std::fs::read_dir(self.remote_data().bin_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        /// Wait until no node holds the remote data directory.
        pub(super) async fn node_gone(&self) {
            let lock = self.remote_data().lock_path();
            tokio::task::spawn_blocking(move || {
                std::fs::File::open(lock).unwrap().lock().unwrap();
            })
            .await
            .unwrap();
        }

        /// Stop the node (`vessel node update` with nothing new to install
        /// does not; this asks the node directly through its own command).
        pub(super) async fn stop_node(&self) {
            let lock = NodeLock::holder(&self.remote_data()).unwrap();
            if let Some(owner) = lock {
                drop(
                    std::process::Command::new("kill")
                        .arg(owner.pid.to_string())
                        .status(),
                );
            }
            self.node_gone().await;
        }
    }

    /// Everything a stream has produced so far, readable while it runs.
    pub(super) struct Captured(tokio::sync::watch::Receiver<String>);

    impl Captured {
        pub(super) fn start(stream: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> Self {
            use tokio::io::AsyncReadExt as _;

            let (sender, receiver) = tokio::sync::watch::channel(String::new());
            drop(tokio::spawn(async move {
                let mut stream = stream;
                let mut buffer = [0_u8; 4096];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    sender.send_modify(|text| text.push_str(&chunk));
                }
            }));
            Self(receiver)
        }

        /// Wait until the output contains `needle`.
        pub(super) async fn wait_for(&mut self, needle: &str) {
            let seen = self.0.wait_for(|text| text.contains(needle)).await.is_ok();
            assert!(seen, "output ended without {needle:?}: {}", self.text());
        }

        pub(super) fn text(&self) -> String {
            self.0.borrow().clone()
        }
    }

    impl Drop for Machine {
        fn drop(&mut self) {
            if let Ok(Some(owner)) = NodeLock::holder(&self.remote_data()) {
                drop(
                    std::process::Command::new("kill")
                        .arg(owner.pid.to_string())
                        .status(),
                );
            }
        }
    }
}

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
