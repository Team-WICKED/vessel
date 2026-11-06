//! The `vessel` binary against an in-process node with `fake-tui` installed
//! as `claude` and `fake-acp` as its ACP adapter. The CLI finds the node through `--node` and its token
//! through `--data-dir`, as it would on the node's own machine.
//!
//! Tests wait on the CLI process and on the session's own end, never on
//! time; the timeout only turns a hang into a failure.

use support::{Fixture, within};

#[cfg(test)]
mod support {
    use std::future::Future;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::Path;
    use std::process::{Output, Stdio};
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt as _;
    use tokio::process::Command;
    use vessel_harness::Lookup;
    use vessel_harness::fakes::{Role, install};
    use vessel_node::{Config, DataDir, Node};
    use vessel_proto::HarnessKind;
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(60);

    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        tokio::time::timeout(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    /// What a CLI run printed.
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

    pub(super) struct Fixture {
        data: TempDir,
        _bin: TempDir,
        node: Node,
    }

    impl Fixture {
        pub(super) async fn start() -> Self {
            let data = tempfile::tempdir().unwrap();
            let bin = tempfile::tempdir().unwrap();
            let tui = Fake::Tui.path().unwrap();
            drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
            let acp = Fake::Acp.path().unwrap();
            drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
            let mut config = Config::new(DataDir::new(data.path()));
            config.bind = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            config.harness_lookup = Lookup::new([bin.path().to_path_buf()]);
            let node = Node::start(config).await.unwrap();
            Self {
                data,
                _bin: bin,
                node,
            }
        }

        pub(super) fn cwd(&self) -> String {
            self.data.path().display().to_string()
        }

        pub(super) fn data(&self) -> &Path {
            self.data.path()
        }

        pub(super) fn node_address(&self) -> String {
            self.node.address().to_string()
        }

        /// Run `vessel <args>` against this fixture's node with `stdin` as
        /// its input.
        pub(super) async fn vessel(&self, args: &[&str], stdin: &[u8]) -> Ran {
            let node = self.node.address().to_string();
            self.vessel_at(&node, self.data.path(), args, stdin).await
        }

        /// Run `vessel --node <node> <args>` in the directory `dir`.
        pub(super) async fn vessel_at(
            &self,
            node: &str,
            dir: &Path,
            args: &[&str],
            stdin: &[u8],
        ) -> Ran {
            let mut child = command(node, self.data.path(), dir, args)
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

        /// Run `vessel <args>` with stdout and stderr going to one file, as
        /// `2>&1` does; returns what was written, in the order written.
        pub(super) async fn vessel_merged(&self, args: &[&str]) -> String {
            let node = self.node.address().to_string();
            let merged = self.data.path().join("merged-output");
            let file = std::fs::File::create(&merged).unwrap();
            let status = command(&node, self.data.path(), self.data.path(), args)
                .stdin(Stdio::null())
                .stdout(file.try_clone().unwrap())
                .stderr(file)
                .status()
                .await
                .unwrap();
            assert!(status.success());
            std::fs::read_to_string(&merged).unwrap()
        }

        /// Start `vessel <args>` against this fixture's node with piped
        /// stdio, for tests that interact with it while it runs.
        #[cfg(unix)]
        pub(super) fn spawn_vessel(&self, args: &[&str]) -> tokio::process::Child {
            let node = self.node.address().to_string();
            command(&node, self.data.path(), self.data.path(), args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        }

        /// `vessel run claude --detach`; returns the new session's id.
        pub(super) async fn run_detached(&self) -> String {
            let cwd = self.cwd();
            let run = self
                .vessel(&["run", "claude", "--detach", "--cwd", &cwd], b"")
                .await;
            assert!(run.ok, "{}", run.stderr);
            run.stdout.trim().to_owned()
        }

        pub(super) async fn stop(self) {
            self.node.stop().await.unwrap();
        }
    }

    fn command(node: &str, data: &Path, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vessel"));
        let _ = command
            .arg("--node")
            .arg(node)
            .arg("--data-dir")
            .arg(data)
            .args(args)
            .current_dir(dir)
            .env_remove("VESSEL_TOKEN")
            .env_remove("VESSEL_NODE");
        command
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_runs_attaches_to_and_reads_a_terminal_session() {
    within(async {
        let fixture = Fixture::start().await;

        let status = fixture.vessel(&["status"], b"").await;
        assert!(status.ok, "{}", status.stderr);
        assert!(status.stdout.contains("claude-code"), "{}", status.stdout);

        let run = fixture
            .vessel(&["run", "claude", "--detach", "--cwd", &fixture.cwd()], b"")
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();
        let prefix = id.get(..8).unwrap();

        // Piped input: the line and `exit` go in, then the CLI keeps showing
        // output until the session ends.
        let attach = fixture
            .vessel(&["attach", prefix], b"hello cli\rexit\r")
            .await;
        assert!(attach.ok, "{}", attach.stderr);
        assert!(
            attach.stdout.contains("echo: hello cli"),
            "{}",
            attach.stdout
        );
        assert!(
            attach.stderr.contains("session exited (0)"),
            "{}",
            attach.stderr
        );

        let log = fixture.vessel(&["log", prefix], b"").await;
        assert!(log.ok, "{}", log.stderr);
        assert!(log.stdout.contains("echo: hello cli"), "{}", log.stdout);
        assert!(
            log.stderr.contains("[session exited (0)]"),
            "{}",
            log.stderr
        );

        let sessions = fixture.vessel(&["sessions"], b"").await;
        assert!(sessions.stdout.contains(&id), "{}", sessions.stdout);
        assert!(
            sessions.stdout.contains("exited (0)"),
            "{}",
            sessions.stdout
        );
        fixture.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_detach_and_kill() {
    within(async {
        let fixture = Fixture::start().await;
        // `send` writes a line; the program reads it before the `exit` that
        // attaching sends after it.
        let id = fixture.run_detached().await;
        let sent = fixture.vessel(&["send", &id, "from send"], b"").await;
        assert!(sent.ok, "{}", sent.stderr);
        let attach = fixture.vessel(&["attach", &id], b"exit\r").await;
        assert!(
            attach.stdout.contains("echo: from send"),
            "{}",
            attach.stdout
        );

        // `Ctrl-]` detaches; the session keeps running until killed.
        let id = fixture.run_detached().await;
        let detached = fixture.vessel(&["attach", &id], b"\x1d").await;
        assert!(detached.ok, "{}", detached.stderr);
        assert!(detached.stderr.contains("detached"), "{}", detached.stderr);
        let killed = fixture.vessel(&["kill", &id], b"").await;
        assert!(killed.ok, "{}", killed.stderr);
        // Attaching to the killed session replays it and ends.
        let replay = fixture.vessel(&["attach", &id], b"").await;
        assert!(replay.ok, "{}", replay.stderr);
        assert!(
            replay.stderr.contains("[vessel] session exited"),
            "{}",
            replay.stderr
        );
        fixture.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_are_reported_and_fail_the_command() {
    within(async {
        let fixture = Fixture::start().await;
        let missing = fixture.vessel(&["attach", "abcdef"], b"").await;
        assert!(!missing.ok);
        assert!(
            missing.stderr.contains("no session matches"),
            "{}",
            missing.stderr
        );

        let refused = fixture
            .vessel(&["--token", "wrong-token", "sessions"], b"")
            .await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("refused the token"),
            "{}",
            refused.stderr
        );

        let not_installed = fixture.vessel(&["run", "codex", "--detach"], b"").await;
        assert!(!not_installed.ok);
        assert!(
            not_installed.stderr.contains("not installed"),
            "{}",
            not_installed.stderr
        );
        fixture.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_piped_prompt_shows_the_whole_reply_then_detaches() {
    within(async {
        let fixture = Fixture::start().await;
        let cwd = fixture.cwd();
        let run = fixture
            .vessel(
                &["run", "claude", "--structured", "--detach", "--cwd", &cwd],
                b"",
            )
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();

        let attach = fixture.vessel(&["attach", &id], b"plain\n").await;
        assert!(attach.ok, "{}", attach.stderr);
        assert!(attach.stdout.contains("> plain"), "{}", attach.stdout);
        assert!(attach.stdout.contains("echo: plain"), "{}", attach.stdout);
        assert!(
            attach.stdout.contains("[turn ended: done]"),
            "{}",
            attach.stdout
        );
        assert!(attach.stderr.contains("detached"), "{}", attach.stderr);

        let log = fixture.vessel(&["log", &id], b"").await;
        assert!(log.stdout.contains("echo: plain"), "{}", log.stdout);
        // Merged as `2>&1` does, status lines (stderr) and the conversation
        // (stdout) come out in record order.
        let merged = fixture.vessel_merged(&["log", &id]).await;
        let at = |needle: &str| {
            merged
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {merged}"))
        };
        assert!(at("[session starting]") < at("[ready"), "{merged}");
        assert!(at("[ready") < at("[session running]"), "{merged}");
        assert!(at("[session running]") < at("> plain"), "{merged}");
        assert!(at("> plain") < at("echo: plain"), "{merged}");
        let sessions = fixture.vessel(&["sessions"], b"").await;
        assert!(
            sessions.stdout.contains(" structured "),
            "{}",
            sessions.stdout
        );
        fixture.stop().await;
    })
    .await;
}

/// The local token file goes only to a node on this machine, and a relative
/// `--cwd` means something only there. For a remote node both fail before
/// any connection is attempted (nothing here touches the network).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_nodes_need_an_explicit_token_and_an_absolute_cwd() {
    within(async {
        let fixture = Fixture::start().await;
        let remote = "gpu-box.invalid:7077";
        let sessions = fixture
            .vessel_at(remote, fixture.data(), &["sessions"], b"")
            .await;
        assert!(!sessions.ok);
        assert!(
            sessions.stderr.contains("is not on this machine")
                && sessions.stderr.contains("--token"),
            "{}",
            sessions.stderr
        );

        let relative = fixture
            .vessel_at(
                remote,
                fixture.data(),
                &[
                    "--token", "abc", "run", "claude", "--detach", "--cwd", "sub",
                ],
                b"",
            )
            .await;
        assert!(!relative.ok);
        assert!(
            relative.stderr.contains("is relative"),
            "{}",
            relative.stderr
        );

        // On this machine a relative `--cwd` is taken from where `vessel`
        // runs.
        let sub = fixture.data().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let node = fixture.node_address();
        let run = fixture
            .vessel_at(
                &node,
                fixture.data(),
                &["run", "claude", "--detach", "--cwd", "sub"],
                b"",
            )
            .await;
        assert!(run.ok, "{}", run.stderr);
        let id = run.stdout.trim().to_owned();
        let attach = fixture.vessel(&["attach", &id], b"cwd\rexit\r").await;
        let expected = format!("cwd {}", std::fs::canonicalize(&sub).unwrap().display());
        assert!(attach.stdout.contains(&expected), "{}", attach.stdout);
        fixture.stop().await;
    })
    .await;
}

/// Under the kitty keyboard protocol (which Codex turns on) `Ctrl-]`
/// arrives as `CSI 93;5u`. It detaches like the plain byte, and the keys
/// typed before it still reach the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kitty_encoded_ctrl_bracket_detaches_after_the_keys_before_it() {
    within(async {
        let fixture = Fixture::start().await;
        let id = fixture.run_detached().await;
        let detached = fixture
            .vessel(&["attach", &id], b"before detach\r\x1b[93;5u")
            .await;
        assert!(detached.ok, "{}", detached.stderr);
        assert!(detached.stderr.contains("detached"), "{}", detached.stderr);
        let attach = fixture.vessel(&["attach", &id], b"exit\r").await;
        assert!(
            attach.stdout.contains("echo: before detach"),
            "{}",
            attach.stdout
        );
        fixture.stop().await;
    })
    .await;
}

/// `SIGTERM` detaches cleanly (exit 0, the terminal restored) instead of
/// killing the CLI mid-attach.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_detaches_cleanly() {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};

    within(async {
        let fixture = Fixture::start().await;
        let id = fixture.run_detached().await;
        let mut child = fixture.spawn_vessel(&["attach", &id]);
        // Output is shown only once the attach loop runs, and its signal
        // handlers are installed before that.
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("ready") {
            line.clear();
            assert!(stdout.read_line(&mut line).await.unwrap() > 0, "no output");
        }
        let pid = child.id().unwrap().to_string();
        let killed = tokio::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await
            .unwrap();
        assert!(killed.success());
        let status = child.wait().await.unwrap();
        let mut stderr = String::new();
        let _read = child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .await
            .unwrap();
        assert!(status.success(), "{status:?} {stderr}");
        assert!(stderr.contains("detached"), "{stderr}");
        drop(child.stdin.take());
        fixture.stop().await;
    })
    .await;
}
