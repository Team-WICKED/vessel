//! Harness sessions: spawning, output fan-out, stdin, and teardown.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;

use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, RwLock, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vessel_core::{OutputChunk, SessionId, SessionInfo, SessionSpec, SessionStatus, StreamKind};

use crate::error::NodeError;
use crate::harness;

/// Lines of output kept per session so late subscribers can catch up.
const BACKLOG_LINES: usize = 2000;
/// Live fan-out buffer per subscriber; slow readers skip ahead rather than block.
const BROADCAST_CAPACITY: usize = 1024;

/// One running (or finished) harness process.
#[derive(Debug)]
pub struct Session {
    id: SessionId,
    spec: SessionSpec,
    created_at: chrono::DateTime<Utc>,
    status: watch::Sender<SessionStatus>,
    backlog: Mutex<VecDeque<OutputChunk>>,
    output: broadcast::Sender<OutputChunk>,
    stdin: Mutex<Option<ChildStdin>>,
    cancel: CancellationToken,
}

impl Session {
    /// Snapshot of the session for the API.
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id,
            spec: self.spec.clone(),
            status: self.status.borrow().clone(),
            created_at: self.created_at,
        }
    }

    /// Watch for status changes.
    pub fn status(&self) -> watch::Receiver<SessionStatus> {
        self.status.subscribe()
    }

    /// Everything emitted so far, plus a receiver for what comes next.
    ///
    /// The receiver is created while the backlog lock is held, so no line is
    /// lost between the two.
    pub async fn subscribe(&self) -> (Vec<OutputChunk>, broadcast::Receiver<OutputChunk>) {
        let backlog = self.backlog.lock().await;
        let rx = self.output.subscribe();
        (backlog.iter().cloned().collect(), rx)
    }

    /// Write text to the harness's stdin.
    ///
    /// # Errors
    /// Fails if the session has ended or the pipe is broken.
    pub async fn write_input(&self, text: &str) -> Result<(), NodeError> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(NodeError::SessionNotLive(self.id));
        };
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(NodeError::Stdin)?;
        stdin.flush().await.map_err(NodeError::Stdin)
    }

    /// Ask the process to die. Returns immediately; status flips once it has.
    pub fn kill(&self) {
        self.cancel.cancel();
    }

    async fn push(&self, chunk: OutputChunk) {
        {
            let mut backlog = self.backlog.lock().await;
            while backlog.len() >= BACKLOG_LINES {
                drop(backlog.pop_front());
            }
            backlog.push_back(chunk.clone());
        }
        // Err only means nobody is listening right now; the backlog has it.
        if self.output.send(chunk).is_err() {
            tracing::trace!(session = %self.id, "no live subscribers");
        }
    }

    fn set_status(&self, status: SessionStatus) {
        tracing::info!(session = %self.id, ?status, "session status");
        // `send` would drop the value when nobody is watching; `send_replace`
        // always stores it, so `info()` stays correct with no subscribers.
        drop(self.status.send_replace(status));
    }
}

/// Owns every session on this node.
#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
}

impl SessionManager {
    /// Empty manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshots of every session, newest first.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .sessions
            .read()
            .await
            .values()
            .map(|s| s.info())
            .collect();
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        infos
    }

    /// Look a session up by id.
    pub async fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.read().await.get(&id).map(Arc::clone)
    }

    /// Start a harness process and begin pumping its output.
    ///
    /// # Errors
    /// Fails if the harness is not installed or the process cannot be spawned.
    pub async fn spawn(&self, spec: SessionSpec) -> Result<Arc<Session>, NodeError> {
        let path =
            harness::locate(spec.harness).ok_or(NodeError::HarnessNotInstalled(spec.harness))?;

        let mut builder = Command::new(&path);
        let command = builder
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let command = match &spec.cwd {
            Some(cwd) => command.current_dir(cwd),
            None => command,
        };

        let id = SessionId::random();
        tracing::info!(session = %id, harness = %spec.harness, path = %path.display(), args = ?spec.args, "spawning");
        let mut child = command.spawn().map_err(NodeError::Spawn)?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        let (status_tx, _) = watch::channel(SessionStatus::Running);
        let (output_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let session = Arc::new(Session {
            id,
            spec,
            created_at: Utc::now(),
            status: status_tx,
            backlog: Mutex::new(VecDeque::with_capacity(BACKLOG_LINES)),
            output: output_tx,
            stdin: Mutex::new(stdin),
            cancel: CancellationToken::new(),
        });

        let mut pumps = Vec::new();
        if let Some(stdout) = stdout {
            pumps.push(spawn_pump(Arc::clone(&session), stdout, StreamKind::Stdout));
        }
        if let Some(stderr) = stderr {
            pumps.push(spawn_pump(Arc::clone(&session), stderr, StreamKind::Stderr));
        }
        drop(tokio::spawn(supervise(Arc::clone(&session), child, pumps)));

        drop(self.sessions.write().await.insert(id, Arc::clone(&session)));
        Ok(session)
    }
}

fn spawn_pump<R>(session: Arc<Session>, reader: R, stream: StreamKind) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    session
                        .push(OutputChunk {
                            session: session.id,
                            stream,
                            line,
                        })
                        .await;
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(session = %session.id, ?stream, %err, "output pump failed");
                    break;
                }
            }
        }
    })
}

/// Wait for the child to exit (or be cancelled), let the pumps drain, then
/// publish the final status.
async fn supervise(session: Arc<Session>, mut child: Child, pumps: Vec<JoinHandle<()>>) {
    let code = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => status.code(),
            Err(err) => {
                tracing::warn!(session = %session.id, %err, "wait failed");
                None
            }
        },
        () = session.cancel.cancelled() => {
            if let Err(err) = child.kill().await {
                tracing::warn!(session = %session.id, %err, "kill failed");
            }
            None
        }
    };

    // Drop our stdin handle so nothing can write to a dead process.
    drop(session.stdin.lock().await.take());

    for pump in pumps {
        if let Err(err) = pump.await {
            tracing::warn!(session = %session.id, %err, "pump task panicked");
        }
    }

    session.set_status(SessionStatus::Exited { code });
}
