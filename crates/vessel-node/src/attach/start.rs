//! Starting a node in the background when no node answers (see the
//! parent module's Decision Log): under the start lock, through a login
//! shell on Unix, detached, and announced by a readiness line.

use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader};
use vessel_store::{DataDir, StartLock};

use super::{AttachError, READY, connect};

/// How long a starting node may take before the proxy gives up.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes of other output (a login shell's banner) read while waiting
/// for the readiness line.
const START_OUTPUT_LIMIT: u64 = 64 * 1024;
/// Lines of the node log shown when a start fails.
const LOG_TAIL_LINES: usize = 10;

/// Start a node in the background and wait until it serves. Racing proxies
/// take turns on the start lock; whoever comes second finds the node the
/// first one started.
pub(super) async fn start_node(dir: &DataDir) -> Result<(), AttachError> {
    let locking = dir.clone();
    let start_lock = tokio::task::spawn_blocking(move || {
        locking.prepare()?;
        StartLock::acquire(&locking)
    })
    .await
    .map_err(|error| AttachError::Spawn(io::Error::other(error)))?
    .map_err(AttachError::DataDir)?;
    if connect(dir).await.is_ok() {
        drop(start_lock);
        return Ok(());
    }
    let logging = dir.clone();
    let (log, log_file) = tokio::task::spawn_blocking(move || logging.open_node_log())
        .await
        .map_err(|error| AttachError::Spawn(io::Error::other(error)))?
        .map_err(AttachError::DataDir)?;
    let executable = std::env::current_exe().map_err(AttachError::Spawn)?;
    let mut command = node_command(&executable, dir.root());
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_file))
        .kill_on_drop(false);
    detach(&mut command);
    let mut child = command.spawn().map_err(AttachError::Spawn)?;
    tracing::info!(pid = ?child.id(), log = %log.display(), "started a node");
    let ready = match child.stdout.take() {
        Some(stdout) => tokio::time::timeout(START_TIMEOUT, wait_ready(stdout))
            .await
            .unwrap_or(false),
        None => false,
    };
    drop(start_lock);
    if ready {
        return Ok(());
    }
    // Stuck or failed: make sure it is gone (a no-op if it exited).
    if let Err(error) = child.start_kill() {
        tracing::debug!(%error, "starting node already gone");
    }
    let tail = tokio::task::spawn_blocking({
        let log = log.clone();
        move || log_tail(&log)
    })
    .await
    .unwrap_or_default();
    Err(AttachError::NotReady { log, tail })
}

/// The command that runs the node: `vesseld --bind none --detached
/// --data-dir DIR`, through a login shell on Unix (see the module docs).
///
/// `--bind none`: a node started over SSH needs no TCP port, and on a
/// machine shared by several users a fixed port would collide.
fn node_command(executable: &Path, root: &Path) -> tokio::process::Command {
    let node_args = [
        executable.as_os_str(),
        "--bind".as_ref(),
        "none".as_ref(),
        "--detached".as_ref(),
        "--data-dir".as_ref(),
        root.as_os_str(),
    ];
    #[cfg(unix)]
    {
        let mut command = tokio::process::Command::new(login_shell());
        // `exec "$0" "$@"` runs the arguments as given: no quoting, no
        // word splitting, whatever the paths contain.
        let _ = command
            .arg("-l")
            .arg("-c")
            .arg(r#"exec "$0" "$@""#)
            .args(node_args);
        command
    }
    #[cfg(not(unix))]
    {
        let mut command = tokio::process::Command::new(executable);
        let _ = command.args(node_args.iter().skip(1));
        command
    }
}

/// The user's shell if it is a POSIX-style one (it reads the user's
/// profile), else `/bin/sh`. Shells such as fish do not take `"$@"`.
#[cfg(unix)]
fn login_shell() -> PathBuf {
    const POSIX: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| {
            shell.is_absolute()
                && shell
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| POSIX.contains(&name))
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Let the node outlive this proxy and its SSH session.
#[cfg(unix)]
fn detach(command: &mut tokio::process::Command) {
    let _ = command.process_group(0);
}

/// Let the node outlive this proxy and its SSH session.
#[cfg(windows)]
fn detach(command: &mut tokio::process::Command) {
    /// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`.
    const FLAGS: u32 = 0x0000_0008 | 0x0000_0200;
    let _ = command.creation_flags(FLAGS);
}

/// Read the starting node's stdout until its readiness line. A login
/// shell may print a banner first; at most [`START_OUTPUT_LIMIT`] bytes of
/// that are skipped.
async fn wait_ready(stdout: impl AsyncRead + Unpin) -> bool {
    let mut lines = BufReader::new(stdout.take(START_OUTPUT_LIMIT)).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.starts_with(READY) => return true,
            Ok(Some(_banner)) => {}
            Ok(None) | Err(_) => return false,
        }
    }
}

/// The last lines of the node log, for the error message.
fn log_tail(log: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(log) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines.get(first..).unwrap_or_default().join("\n")
}
