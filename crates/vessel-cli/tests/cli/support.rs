//! The fixture: a node in this process, and `vessel` run against it.

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
        Self::start_with(|_, _| {}).await
    }

    /// Start the node with `adjust` applied to its config.
    pub(super) async fn start_with(adjust: impl FnOnce(&mut Config, &Path)) -> Self {
        let data = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        let acp = Fake::Acp.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
        let mut config = Config::new(DataDir::new(data.path()));
        config.bind = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        config.harness_lookup = Lookup::new([bin.path().to_path_buf()]);
        adjust(&mut config, data.path());
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

    pub(super) const fn node(&self) -> &Node {
        &self.node
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

    /// Run `vessel <args>` without `--node`, in the data directory, with
    /// `stdin` as its input.
    pub(super) async fn vessel_book_input(&self, args: &[&str], stdin: &[u8]) -> Ran {
        let mut child = book_command(self.data.path(), self.data.path(), args)
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
                    let _done = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
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

    /// Wait until the output contains `needle` `times` times; fail with
    /// what was seen if it does not within 40 s (well inside the safety
    /// net, so the failure says where the test stopped).
    pub(super) async fn wait_for(&mut self, needle: &str, times: usize) {
        // The match drops the borrow `wait_for` returns before `text`
        // borrows again.
        let seen = matches!(
            tokio::time::timeout(
                Duration::from_secs(40),
                self.0
                    .wait_for(|text| text.matches(needle).count() >= times),
            )
            .await,
            Ok(Ok(_))
        );
        assert!(seen, "no {needle:?} in: {}", self.text());
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
