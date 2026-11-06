//! `vesseld attach`: the per-connection proxy that SSH runs (ADR 0007).
//!
//! It connects to this machine's node through the local socket, presents
//! the node's token as the first frame, and then copies bytes between its
//! own stdin/stdout and the socket without decoding them. If no node
//! answers, it starts one in the background first. Its exit status is a
//! [`ProxyExit`], the client's only side channel once the stream is gone.
//!
//! [Decision Log]
//! - Purpose: a client with nothing but SSH access reaches the node, which
//!   needs no open port and may not even be running yet (coordinator
//!   decision 2: a new machine is productive in minutes).
//! - Starting the node: under a start lock, so racing proxies start one
//!   node, and through a login shell on Unix, because SSH's non-interactive
//!   commands get a minimal `PATH` without the user's harness installs
//!   (`~/.local/bin`, `nvm`, Homebrew). The node detaches into its own
//!   process group with stdin from `/dev/null` and its log in
//!   `logs/vesseld.log`, and announces readiness with a marker line on a
//!   pipe; the proxy waits for that line instead of polling for a socket.
//! - Bytes, not frames: both sides use the same framing (PROTOCOL.md
//!   version 3), so the proxy decodes only the node's first frame, to tell a
//!   refused token (exit 82) from a working connection.
//! - Stdin is read on a plain thread: a blocking read on the runtime's pool
//!   would keep the process from exiting when the node side ends first.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader,
};
use tokio::sync::mpsc;
use vessel_proto::{ErrorCode, ProxyExit, ServerFrame};
use vessel_rpc::MAX_FRAME;
use vessel_store::{DataDir, NodeToken, StartLock, StoreError};

use crate::ipc::{self, Endpoint};

/// The line a node started by `attach` prints on stdout once it serves,
/// followed by its pid.
pub const READY: &str = "vesseld-ready";

/// How long a starting node may take before the proxy gives up.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes of other output (a login shell's banner) read while waiting
/// for the readiness line.
const START_OUTPUT_LIMIT: u64 = 64 * 1024;
/// Stdin is forwarded in chunks of at most this size.
const STDIN_CHUNK: usize = 64 * 1024;
/// Chunks buffered between the stdin thread and the socket.
const STDIN_QUEUE: usize = 16;
/// Lines of the node log shown when a start fails.
const LOG_TAIL_LINES: usize = 10;

/// Why `attach` could not bridge a connection.
#[derive(Debug, thiserror::Error)]
enum AttachError {
    #[error("the data directory is unusable")]
    DataDir(#[source] StoreError),
    #[error("cannot read the node's token")]
    Token(#[source] StoreError),
    #[error("cannot reach the node at {}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot start the node")]
    Spawn(#[source] io::Error),
    #[error("the node did not start; its log ({}) ends with:\n{tail}", log.display())]
    NotReady { log: PathBuf, tail: String },
}

impl AttachError {
    const fn exit(&self) -> ProxyExit {
        match self {
            Self::DataDir(_) | Self::Token(_) => ProxyExit::DataDirUnusable,
            Self::Connect { .. } | Self::Spawn(_) | Self::NotReady { .. } => {
                ProxyExit::NodeUnavailable
            }
        }
    }
}

/// Run the proxy for the node of `dir`; returns how it ended.
pub async fn run(dir: DataDir) -> ProxyExit {
    match attach(&dir).await {
        Ok(exit) => exit,
        Err(error) => {
            tracing::error!(error = %chain(&error), "vesseld attach failed");
            error.exit()
        }
    }
}

async fn attach(dir: &DataDir) -> Result<ProxyExit, AttachError> {
    let stream = match connect(dir).await {
        Ok(stream) => stream,
        Err(Unreachable::NoNode) => {
            start_node(dir).await?;
            connect(dir)
                .await
                .map_err(|unreachable| unreachable.into_error(dir))?
        }
        Err(other) => return Err(other.into_error(dir)),
    };
    let token = read_token(dir).await?;
    Ok(bridge(stream, &token).await)
}

/// Why the local socket could not be reached.
enum Unreachable {
    /// No node is serving it (no socket, a stale one, or no node yet).
    NoNode,
    /// Something else went wrong.
    Failed(AttachError),
}

impl Unreachable {
    fn into_error(self, dir: &DataDir) -> AttachError {
        match self {
            Self::NoNode => AttachError::Connect {
                path: dir.socket_path(),
                source: io::ErrorKind::NotFound.into(),
            },
            Self::Failed(error) => error,
        }
    }
}

async fn connect(dir: &DataDir) -> Result<ipc::LocalClient, Unreachable> {
    let lookup = dir.clone();
    let endpoint = tokio::task::spawn_blocking(move || Endpoint::find(&lookup))
        .await
        .map_err(|error| Unreachable::Failed(AttachError::Spawn(io::Error::other(error))))?
        .map_err(|error| Unreachable::Failed(AttachError::DataDir(error)))?;
    let Some(endpoint) = endpoint else {
        return Err(Unreachable::NoNode);
    };
    match ipc::connect(&endpoint).await {
        Ok(stream) => Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(Unreachable::NoNode)
        }
        Err(source) => Err(Unreachable::Failed(AttachError::Connect {
            path: endpoint.path().to_path_buf(),
            source,
        })),
    }
}

async fn read_token(dir: &DataDir) -> Result<NodeToken, AttachError> {
    let dir = dir.clone();
    tokio::task::spawn_blocking(move || dir.read_token())
        .await
        .map_err(|error| AttachError::Spawn(io::Error::other(error)))?
        .map_err(AttachError::Token)
}

/// Start a node in the background and wait until it serves. Racing proxies
/// take turns on the start lock; whoever comes second finds the node the
/// first one started.
async fn start_node(dir: &DataDir) -> Result<(), AttachError> {
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

/// Present the token, then copy bytes both ways until the node closes.
async fn bridge(stream: ipc::LocalClient, token: &NodeToken) -> ProxyExit {
    let (mut from_node, mut to_node) = tokio::io::split(stream);
    if let Err(error) = write_frame(&mut to_node, token.expose().as_bytes()).await {
        tracing::warn!(%error, "the node closed before the token was sent");
        return ProxyExit::NodeUnavailable;
    }
    let mut stdin = match stdin_chunks() {
        Ok(stdin) => stdin,
        Err(error) => {
            tracing::error!(%error, "cannot read stdin");
            return ProxyExit::Closed;
        }
    };
    let upstream = async {
        while let Some(chunk) = stdin.recv().await {
            to_node.write_all(&chunk).await?;
            to_node.flush().await?;
        }
        // The client is done; the node sees end of stream and closes.
        to_node.shutdown().await
    };
    let downstream = forward_to_stdout(&mut from_node);
    tokio::pin!(upstream, downstream);
    let mut upstream_done = false;
    loop {
        tokio::select! {
            exit = &mut downstream => return exit,
            sent = &mut upstream, if !upstream_done => {
                upstream_done = true;
                if let Err(error) = sent {
                    tracing::debug!(%error, "node stopped reading");
                }
            }
        }
    }
}

/// Copy the node's output to stdout. The first frame is decoded to see
/// whether the node refused the token.
async fn forward_to_stdout(from_node: &mut (impl AsyncRead + Unpin)) -> ProxyExit {
    let mut stdout = tokio::io::stdout();
    let first = match read_frame(from_node).await {
        Ok(Some(frame)) => frame,
        Ok(None) => return ProxyExit::Closed,
        Err(error) => {
            tracing::debug!(%error, "node connection failed");
            return ProxyExit::Closed;
        }
    };
    if stdout.write_all(&first.raw).await.is_err() || stdout.flush().await.is_err() {
        return ProxyExit::Closed;
    }
    if first.is_refusal() {
        return ProxyExit::Refused;
    }
    let mut buffer = vec![0_u8; STDIN_CHUNK];
    loop {
        let read = match from_node.read(&mut buffer).await {
            Ok(0) | Err(_) => return ProxyExit::Closed,
            Ok(read) => read,
        };
        let Some(chunk) = buffer.get(..read) else {
            return ProxyExit::Closed;
        };
        if stdout.write_all(chunk).await.is_err() || stdout.flush().await.is_err() {
            return ProxyExit::Closed;
        }
    }
}

/// One frame as read: its bytes (length included) and its body.
struct RawFrame {
    raw: Vec<u8>,
}

impl RawFrame {
    fn body(&self) -> &[u8] {
        self.raw.get(4..).unwrap_or_default()
    }

    /// Whether this is the node's `rejected` (`unauthorized`) answer.
    fn is_refusal(&self) -> bool {
        matches!(
            serde_json::from_slice::<ServerFrame>(self.body()),
            Ok(ServerFrame::Rejected { error }) if error.code == ErrorCode::Unauthorized
        )
    }
}

/// Read one frame; `None` at a clean end of stream before it.
async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<RawFrame>> {
    let mut header = [0_u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = usize::try_from(u32::from_be_bytes(header))
        .ok()
        .filter(|length| *length <= MAX_FRAME.get())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    let mut raw = Vec::with_capacity(length.saturating_add(4));
    raw.extend_from_slice(&header);
    raw.resize(length.saturating_add(4), 0);
    if let Some(body) = raw.get_mut(4..) {
        let _read = reader.read_exact(body).await?;
    }
    Ok(Some(RawFrame { raw }))
}

/// Write one frame (PROTOCOL.md version 3 framing).
async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    body: &[u8],
) -> io::Result<()> {
    let length = u32::try_from(body.len())
        .map_err(|_too_long| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

/// Stdin as chunks, read on a plain thread (see the module docs). Ends at
/// end of input.
fn stdin_chunks() -> io::Result<mpsc::Receiver<Bytes>> {
    use std::io::Read as _;

    let (sender, chunks) = mpsc::channel(STDIN_QUEUE);
    let _detached = std::thread::Builder::new()
        .name("vesseld-attach-stdin".to_owned())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buffer = vec![0_u8; STDIN_CHUNK];
            loop {
                let read = match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let Some(chunk) = buffer.get(..read) else {
                    return;
                };
                if sender.blocking_send(Bytes::copy_from_slice(chunk)).is_err() {
                    return;
                }
            }
        })?;
    Ok(chunks)
}

/// An error and its causes on one line.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
