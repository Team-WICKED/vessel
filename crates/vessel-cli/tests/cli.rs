//! The `vessel` binary against an in-process node with `fake-tui` installed
//! as `claude` and `fake-acp` as its ACP adapter. The CLI finds the node through `--node` and its token
//! through `--data-dir`, as it would on the node's own machine.
//!
//! Tests wait on the CLI process and on the session's own end, never on
//! time; the timeout only turns a hang into a failure.

use support::{Captured, Fixture, Proxy, within};

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

    /// The safety net starts after the fakes are built: the first build of
    /// a run can take a minute on a cold machine (see
    /// `vessel_testkit::within`).
    /// The test is boxed at once, so the future handed around stays small.
    pub(super) fn within<F: Future>(test: F) -> impl Future<Output = F::Output> {
        let test = Box::pin(test);
        async move {
            vessel_testkit::within(SAFETY_NET, test)
                .await
                .expect("test timed out")
        }
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
            config.bind = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
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
            self.node.address().unwrap().to_string()
        }

        /// Run `vessel <args>` against this fixture's node with `stdin` as
        /// its input.
        pub(super) async fn vessel(&self, args: &[&str], stdin: &[u8]) -> Ran {
            let node = self.node.address().unwrap().to_string();
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

        /// Run `vessel <args>` without `--node` (the node book and project
        /// files decide) in `dir`, with extra environment `env`.
        pub(super) async fn vessel_book(
            &self,
            dir: &Path,
            args: &[&str],
            env: &[(&str, &std::ffi::OsStr)],
        ) -> Ran {
            let mut command = book_command(self.data.path(), dir, args);
            for (key, value) in env {
                let _ = command.env(key, value);
            }
            command.stdin(Stdio::null()).output().await.unwrap().into()
        }

        /// The node book `vessel` uses in these tests.
        pub(super) fn book(&self) -> std::path::PathBuf {
            self.data
                .path()
                .join("config")
                .join("vessel")
                .join("nodes.toml")
        }

        /// Run `vessel <args>` with stdout and stderr going to one file, as
        /// `2>&1` does; returns what was written, in the order written.
        pub(super) async fn vessel_merged(&self, args: &[&str]) -> String {
            let node = self.node.address().unwrap().to_string();
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
            let node = self.node.address().unwrap().to_string();
            command(&node, self.data.path(), self.data.path(), args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        }

        /// Start `vessel --node <node> <args>` with piped stdio.
        pub(super) fn spawn_vessel_at(&self, node: &str, args: &[&str]) -> tokio::process::Child {
            command(node, self.data.path(), self.data.path(), args)
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

    /// A TCP forwarder to a node that the test can cut, as a network drop
    /// would: every open connection closes at once; new ones are still
    /// accepted.
    pub(super) struct Proxy {
        pub(super) address: SocketAddr,
        open: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
        accepting: tokio::task::JoinHandle<()>,
    }

    impl Proxy {
        pub(super) async fn start(to: SocketAddr) -> Self {
            let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let open = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let tracked = std::sync::Arc::clone(&open);
            let accepting = tokio::spawn(async move {
                while let Ok((mut inbound, _peer)) = listener.accept().await {
                    let forwarding = tokio::spawn(async move {
                        let Ok(mut outbound) = tokio::net::TcpStream::connect(to).await else {
                            return;
                        };
                        let _done =
                            tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    tracked.lock().unwrap().push(forwarding.abort_handle());
                }
            });
            Self {
                address,
                open,
                accepting,
            }
        }

        /// Close every connection open now.
        pub(super) fn cut(&self) {
            for connection in self.open.lock().unwrap().drain(..) {
                connection.abort();
            }
        }
    }

    impl Drop for Proxy {
        fn drop(&mut self) {
            self.cut();
            self.accepting.abort();
        }
    }

    /// Write an executable script. The text goes to a side file and `cp`
    /// creates the script, so this process never holds it open for
    /// writing: a test forking on another thread meanwhile would keep that
    /// descriptor and running the script would fail with `ETXTBSY`.
    #[cfg(unix)]
    pub(super) fn executable(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let mut source = path.as_os_str().to_owned();
        source.push(".source");
        std::fs::write(&source, text).unwrap();
        let copied = std::process::Command::new("cp")
            .arg(&source)
            .arg(path)
            .status()
            .unwrap();
        assert!(copied.success());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

        /// Wait until the output contains `needle` `times` times.
        pub(super) async fn wait_for(&mut self, needle: &str, times: usize) {
            let seen = self
                .0
                .wait_for(|text| text.matches(needle).count() >= times)
                .await
                .is_ok();
            assert!(seen, "output ended without {needle:?}: {}", self.text());
        }

        pub(super) fn text(&self) -> String {
            self.0.borrow().clone()
        }
    }

    /// `vessel` against `node` (or, with `None`, whatever the book says),
    /// with `data` as its local data directory and `<data>/config` as its
    /// configuration directory, so no test reads the user's own node book.
    fn command(node: &str, data: &Path, dir: &Path, args: &[&str]) -> Command {
        let mut command = book_command(data, dir, args);
        let _ = command.arg("--node").arg(node);
        command
    }

    fn book_command(data: &Path, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vessel"));
        let _ = command
            .arg("--data-dir")
            .arg(data)
            .args(args)
            .current_dir(dir)
            .env("VESSEL_CONFIG_DIR", data.join("config"))
            .env_remove("VESSEL_TOKEN")
            .env_remove("VESSEL_NODE")
            .env_remove("VESSEL_SSH");
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
        // Compare canonical forms: on Windows the program may report a short
        // (8.3) name while `canonicalize` gives the verbatim `\\?\` form of
        // the same directory.
        // The answer may follow fake-tui's `> ` prompt on the same line, and
        // the echoed command `cwd` (no path) has no space after it.
        let reported = attach
            .stdout
            .lines()
            .find_map(|line| line.split_once("cwd ").map(|(_, path)| path))
            .unwrap_or_else(|| panic!("no cwd line in {}", attach.stdout));
        assert_eq!(
            std::fs::canonicalize(reported.trim()).unwrap(),
            std::fs::canonicalize(&sub).unwrap(),
            "{}",
            attach.stdout
        );
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

/// The node book: a websocket node added by name becomes the default, its
/// id is recorded on first contact, and a different node answering later
/// is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_nodes_are_added_used_and_pinned() {
    within(async {
        let fixture = Fixture::start().await;
        let dir = fixture.data();
        let url = format!("ws://{}", fixture.node_address());
        let token = fixture.data().join("token");
        let token = token.to_str().unwrap();
        let added = fixture
            .vessel_book(
                dir,
                &[
                    "nodes",
                    "add",
                    "here",
                    &url,
                    "--token-file",
                    token,
                    "--default",
                ],
                &[],
            )
            .await;
        assert!(added.ok, "{}", added.stderr);
        let list = fixture.vessel_book(dir, &["nodes", "list"], &[]).await;
        assert!(list.stdout.contains("* here"), "{}", list.stdout);
        let before = fixture
            .vessel_book(dir, &["nodes", "show", "here"], &[])
            .await;
        assert!(before.stdout.contains("not seen yet"), "{}", before.stdout);

        // No --node: the book's default, with its token file.
        let status = fixture.vessel_book(dir, &["status"], &[]).await;
        assert!(status.ok, "{}", status.stderr);
        assert!(status.stdout.contains("claude-code"), "{}", status.stdout);
        let shown = fixture
            .vessel_book(dir, &["nodes", "show", "here"], &[])
            .await;
        let id = shown
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .unwrap()
            .to_owned();
        assert!(
            id.parse::<vessel_proto::NodeId>().is_ok(),
            "{}",
            shown.stdout
        );

        // Another node answering at that address is refused.
        let book = std::fs::read_to_string(fixture.book()).unwrap();
        let other = vessel_proto::NodeId::random().to_string();
        std::fs::write(fixture.book(), book.replace(&id, &other)).unwrap();
        let refused = fixture.vessel_book(dir, &["status"], &[]).await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("a different node answered"),
            "{}",
            refused.stderr
        );

        let removed = fixture
            .vessel_book(dir, &["nodes", "remove", "here"], &[])
            .await;
        assert!(removed.ok, "{}", removed.stderr);
        let empty = fixture.vessel_book(dir, &["nodes", "list"], &[]).await;
        assert!(empty.stdout.contains("no nodes yet"), "{}", empty.stdout);
        fixture.stop().await;
    })
    .await;
}

/// A project's `.vessel/config.toml` picks one of the user's nodes, and
/// may do nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_file_only_picks_among_the_users_nodes() {
    within(async {
        let fixture = Fixture::start().await;
        let url = format!("ws://{}", fixture.node_address());
        let token = fixture.data().join("token");
        let added = fixture
            .vessel_book(
                fixture.data(),
                &[
                    "nodes",
                    "add",
                    "lab",
                    &url,
                    "--token-file",
                    token.to_str().unwrap(),
                ],
                &[],
            )
            .await;
        assert!(added.ok, "{}", added.stderr);

        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("src");
        std::fs::create_dir_all(project.path().join(".vessel")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let config = project.path().join(".vessel").join("config.toml");
        std::fs::write(&config, "default_node = \"lab\"\n").unwrap();
        let status = fixture.vessel_book(&nested, &["status"], &[]).await;
        assert!(status.ok, "{}", status.stderr);

        std::fs::write(&config, "[nodes.evil]\nssh = \"attacker.example\"\n").unwrap();
        let refused = fixture.vessel_book(&nested, &["status"], &[]).await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("may only set `default_node`"),
            "{}",
            refused.stderr
        );
        std::fs::write(&config, "default_node = \"elsewhere\"\n").unwrap();
        let unknown = fixture.vessel_book(&nested, &["status"], &[]).await;
        assert!(!unknown.ok);
        assert!(
            unknown.stderr.contains("not a node in your book"),
            "{}",
            unknown.stderr
        );
        fixture.stop().await;
    })
    .await;
}

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

/// `vessel node install` uploads a local build to an SSH node and names it
/// by content; a second install finds it. Through a stand-in `ssh` that
/// runs the command in a temporary "remote" home, and a stand-in `vesseld`
/// (Unix only: both are shell scripts).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_install_uploads_a_local_build_over_ssh() {
    within(async {
        let fixture = Fixture::start().await;
        let remote = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let ssh = tools.path().join("ssh");
        support::executable(
            &ssh,
            "#!/bin/sh\n\
             while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             shift\n\
             cd \"$FAKE_SSH_HOME\" || exit 255\n\
             HOME=\"$FAKE_SSH_HOME\" exec sh -c \"$*\"\n",
        );
        let os = if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "unknown-linux-gnu"
        };
        let triple = format!("{}-{os}", std::env::consts::ARCH);
        let vesseld = tools.path().join("vesseld");
        support::executable(
            &vesseld,
            &format!(
                "#!/bin/sh\ncase \"$1\" in version) echo '0.2.0 {triple}' ;; cleanup) ;; *) exit 2 ;; esac\n"
            ),
        );
        let env = [
            ("VESSEL_SSH", ssh.as_os_str()),
            ("FAKE_SSH_HOME", remote.path().as_os_str()),
        ];
        let dir = fixture.data();
        let added = fixture
            .vessel_book(dir, &["nodes", "add", "gpu", "me@gpu-box"], &env)
            .await;
        assert!(added.ok, "{}", added.stderr);
        let args = [
            "--node",
            "gpu",
            "node",
            "install",
            "--vesseld",
            vesseld.to_str().unwrap(),
        ];
        let installed = fixture.vessel_book(dir, &args, &env).await;
        assert!(installed.ok, "{}", installed.stderr);
        assert!(
            installed.stdout.contains("installed ~/.vessel/bin/vesseld-0.2.0-dev-")
                && installed.stdout.contains(" on gpu"),
            "{}",
            installed.stdout
        );
        let bin = remote.path().join(".vessel").join("bin");
        let names: Vec<_> = std::fs::read_dir(&bin)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("vesseld-0.2.0-dev-"), "{names:?}");

        let again = fixture.vessel_book(dir, &args, &env).await;
        assert!(again.ok, "{}", again.stderr);
        assert!(again.stdout.contains("already installed"), "{}", again.stdout);

        let ws = format!("ws://{}", fixture.node_address());
        let not_ssh = fixture
            .vessel_book(dir, &["--node", &ws, "node", "install"], &env)
            .await;
        assert!(!not_ssh.ok);
        assert!(
            not_ssh.stderr.contains("is not an SSH node"),
            "{}",
            not_ssh.stderr
        );
        fixture.stop().await;
    })
    .await;
}
