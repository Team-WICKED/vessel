//! A remote machine reached through `fake-ssh`, and `vessel` run against it.

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
