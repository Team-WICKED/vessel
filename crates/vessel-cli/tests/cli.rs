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

        /// Run `vessel <args>` with `stdin` as its input.
        pub(super) async fn vessel(&self, args: &[&str], stdin: &[u8]) -> Ran {
            let mut child = Command::new(env!("CARGO_BIN_EXE_vessel"))
                .arg("--node")
                .arg(self.node.address().to_string())
                .arg("--data-dir")
                .arg(self.data.path())
                .args(args)
                .env_remove("VESSEL_TOKEN")
                .env_remove("VESSEL_NODE")
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
