//! The SSH path end to end (ADR 0007), with real processes: the `vesseld`
//! binary's `attach` (which starts the node in the background) under a
//! temporary home, reached directly or through `fake-ssh`. Unix only: the
//! remote side of milestone 2 is (Windows remotes are out of scope), and
//! `fake-ssh` needs `sh`.
//!
//! Every test stops the node it started; a guard kills it if a test fails
//! first, so no node outlives its temporary home.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tempfile::TempDir;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use vessel_client::NodeClient;
use vessel_harness::fakes::{Role, install};
use vessel_proto::{
    ErrorCode, EventRecord, EventSeq, GetNode, HarnessKind, ProxyExit, SessionMode, SessionSpec,
    StopNode, StopWhen, TerminalSize,
};
use vessel_rpc::{ClientConfig, Connection, Framed, MAX_FRAME};
use vessel_store::{DataDir, NodeLock};
use vessel_testkit::Fake;

use crate::support::{client, collect_until, keys, output_of, seqs, within};

/// A "remote machine": a home directory whose `.profile` puts the fake
/// harnesses on `PATH` (as a user's profile would put real ones), and the
/// default data directory `~/.vessel` inside it.
struct Remote {
    home: TempDir,
    _bin: TempDir,
}

impl Remote {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        // A login shell reads this; the node then finds `claude` exactly as
        // it would find a real install under `~/.local/bin`.
        let profile = format!("export PATH='{}':\"$PATH\"\n", bin.path().display());
        std::fs::write(home.path().join(".profile"), profile).unwrap();
        Self { home, _bin: bin }
    }

    fn data_dir(&self) -> DataDir {
        DataDir::new(self.home.path().join(".vessel"))
    }

    /// The environment of a command run on this machine over SSH.
    fn environment(&self, command: &mut Command) {
        let _ = command
            .env("HOME", self.home.path())
            .env("SHELL", "/bin/sh")
            .env_remove("VESSEL_DATA_DIR")
            .env_remove("VESSEL_BIND")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
    }

    /// `vesseld attach`, run directly on this machine.
    fn attach(&self) -> Child {
        let mut command = Command::new(vesseld());
        let _ = command.arg("attach");
        self.environment(&mut command);
        command.spawn().unwrap()
    }

    /// `ssh -T host -- '<vesseld>' attach` through `fake-ssh`, optionally
    /// dropping the connection after `drop_after` bytes from the node.
    fn ssh(&self, drop_after: Option<u64>) -> Child {
        let mut command = Command::new(Fake::Ssh.path().unwrap());
        let quoted = format!("'{}'", vesseld().display());
        let _ = command
            .args(["-T", "-o", "BatchMode=yes", "gpu-box", "--"])
            .arg(quoted)
            .arg("attach")
            .env("FAKE_SSH_HOME", self.home.path());
        if let Some(bytes) = drop_after {
            let _ = command.env("FAKE_SSH_DROP_AFTER", bytes.to_string());
        }
        self.environment(&mut command);
        command.spawn().unwrap()
    }

    /// Stop the node through a fresh attach, and wait until it released
    /// the data directory (its process is then ending).
    async fn stop_node(&self) {
        let mut child = self.attach();
        let control = control(&mut child).await;
        assert_eq!(
            control
                .call(StopNode {
                    when: StopWhen::Now
                })
                .await
                .unwrap(),
            vessel_proto::Ack
        );
        drop(control);
        let lock = self.data_dir().lock_path();
        tokio::task::spawn_blocking(move || {
            std::fs::File::open(lock).unwrap().lock().unwrap();
        })
        .await
        .unwrap();
        drop(child.wait().await);
    }
}

impl Drop for Remote {
    /// If a test failed before stopping its node, end it, so it does not
    /// keep running on a deleted home. A failing test also prints the
    /// node's log, which is otherwise deleted with the home.
    #[expect(
        clippy::print_stderr,
        reason = "the node's log is the only clue to a failure on CI"
    )]
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = self.data_dir().log_dir().join("vesseld.log");
            if let Ok(text) = std::fs::read_to_string(&log) {
                eprintln!("--- {} ---\n{text}--- end of node log ---", log.display());
            }
        }
        if let Ok(Some(owner)) = NodeLock::holder(&self.data_dir()) {
            drop(
                std::process::Command::new("kill")
                    .arg(owner.pid.to_string())
                    .status(),
            );
        }
    }
}

fn vesseld() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vesseld"))
}

/// The child's stdio as a protocol transport.
fn stdio(child: &mut Child) -> Framed<ChildStdout, ChildStdin> {
    Framed::new(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        MAX_FRAME,
    )
}

async fn connect(child: &mut Child) -> NodeClient {
    NodeClient::over(stdio(child), client(), &ClientConfig::default())
        .await
        .unwrap()
}

async fn control(child: &mut Child) -> Connection {
    Connection::connect(stdio(child), client(), &ClientConfig::default())
        .await
        .unwrap()
}

fn terminal(home: &Path) -> SessionSpec {
    SessionSpec {
        harness: HarnessKind::ClaudeCode,
        cwd: Some(home.to_path_buf()),
        mode: SessionMode::Terminal {
            args: Vec::new(),
            size: TerminalSize::default(),
        },
    }
}

/// `attach` finds no node, starts one in the background through a login
/// shell (whose profile puts the harness on `PATH`), and bridges to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_starts_a_node_that_finds_harnesses_through_the_login_profile() {
    within(async {
        let remote = Remote::new();
        let mut attach = remote.attach();
        let client = connect(&mut attach).await;
        assert!(
            client
                .harnesses()
                .await
                .unwrap()
                .iter()
                .any(|harness| harness.kind == HarnessKind::ClaudeCode),
            "the login profile's PATH did not reach the node"
        );
        let id = client
            .create_session(terminal(remote.home.path()))
            .await
            .unwrap()
            .id;
        let mut stream = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut stream, &mut seen, "> ").await;
        client.send(id, keys("over attach\r")).await.unwrap();
        collect_until(&mut stream, &mut seen, "echo: over attach").await;
        assert!(remote.data_dir().log_dir().join("vesseld.log").exists());

        drop(stream);
        drop(client);
        remote.stop_node().await;
        assert_eq!(
            attach.wait().await.unwrap().code(),
            Some(i32::from(ProxyExit::Closed.code()))
        );
    })
    .await;
}

/// Two `attach` processes racing on a machine with no node start one node
/// between them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn racing_attaches_start_a_single_node() {
    within(async {
        let remote = Remote::new();
        let mut first = remote.attach();
        let mut second = remote.attach();
        let (one, two) = tokio::join!(control(&mut first), control(&mut second));
        let (one, two) = (one.welcome().node.clone(), two.welcome().node.clone());
        assert_eq!(one.id, two.id);
        assert_eq!(one.started_at, two.started_at);
        remote.stop_node().await;
    })
    .await;
}

/// The proxy's exit status says why it could not bridge: 82 when the node
/// refuses the token it read, 81 when it cannot read one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_or_unreadable_token_has_its_own_exit_code() {
    within(async {
        let remote = Remote::new();
        let mut first = remote.attach();
        let held = control(&mut first).await;
        let token = remote.data_dir().token_path();

        // A valid token, but not this node's.
        let other = tempfile::tempdir().unwrap();
        let wrong = DataDir::new(other.path()).load_or_create_token().unwrap();
        std::fs::write(&token, wrong.expose()).unwrap();
        let mut refused = remote.attach();
        let answer = NodeClient::over(stdio(&mut refused), client(), &ClientConfig::default()).await;
        assert!(
            matches!(&answer, Err(vessel_rpc::ConnectError::Rejected(error)) if error.code == ErrorCode::Unauthorized),
            "{answer:?}"
        );
        assert_eq!(
            refused.wait().await.unwrap().code(),
            Some(i32::from(ProxyExit::Refused.code()))
        );

        // No token at all.
        std::fs::remove_file(&token).unwrap();
        std::fs::create_dir(&token).unwrap();
        let mut unreadable = remote.attach();
        drop(unreadable.stdin.take());
        assert_eq!(
            unreadable.wait().await.unwrap().code(),
            Some(i32::from(ProxyExit::DataDirUnusable.code()))
        );

        // The node itself is unaffected.
        assert_eq!(held.call(GetNode).await.unwrap().id, held.welcome().node.id);
        assert_eq!(
            held.call(StopNode { when: StopWhen::Now }).await.unwrap(),
            vessel_proto::Ack
        );
        drop(held);
        let lock = remote.data_dir().lock_path();
        tokio::task::spawn_blocking(move || std::fs::File::open(lock).unwrap().lock().unwrap())
            .await
            .unwrap();
    })
    .await;
}

/// Over (fake) SSH: a connection that drops mid-stream loses nothing; the
/// session keeps running, and a new connection resumes from the cursor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_ssh_a_dropped_connection_resumes_from_its_cursor() {
    within(async {
        let remote = Remote::new();
        let mut ssh = remote.ssh(None);
        let client = connect(&mut ssh).await;
        let id = client
            .create_session(terminal(remote.home.path()))
            .await
            .unwrap()
            .id;
        let mut stream = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut stream, &mut seen, "> ").await;
        for line in 0..20 {
            client
                .send(
                    id,
                    keys(Box::leak(format!("line {line}\r").into_boxed_str())),
                )
                .await
                .unwrap();
        }
        collect_until(&mut stream, &mut seen, "echo: line 19").await;

        // A second connection drops after 4 KiB of node output, in the
        // middle of replaying the history.
        let mut dropping = remote.ssh(Some(4096));
        let flaky = connect(&mut dropping).await;
        let mut partial: Vec<EventRecord> = Vec::new();
        let mut replay = flaky.follow(id, EventSeq::ZERO).await.unwrap();
        while let Some(Ok(record)) = replay.next().await {
            partial.push(record);
        }
        assert_eq!(dropping.wait().await.unwrap().code(), Some(255));
        let cursor = EventSeq::new(u64::try_from(partial.len()).unwrap());
        assert!(cursor < EventSeq::new(u64::try_from(seen.len()).unwrap()));

        // The session never noticed; a new connection resumes at the cursor.
        client.send(id, keys("after the drop\r")).await.unwrap();
        let mut again = remote.ssh(None);
        let resumed = connect(&mut again).await;
        let mut rest = resumed.follow(id, cursor).await.unwrap();
        let mut tail = Vec::new();
        collect_until(&mut rest, &mut tail, "echo: after the drop").await;
        let whole: Vec<EventRecord> = partial.into_iter().chain(tail).collect();
        let expected: Vec<u64> = (0..u64::try_from(whole.len()).unwrap()).collect();
        assert_eq!(seqs(&whole), expected);
        assert!(output_of(&whole).contains("echo: line 19"));

        drop((stream, rest, client, resumed));
        remote.stop_node().await;
    })
    .await;
}
