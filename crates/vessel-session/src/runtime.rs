//! [`SessionRuntime`]: starts sessions and keeps their handles.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use chrono::Utc;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;
use vessel_agent::{AgentProcess, AgentSetup};
use vessel_proto::{
    EventRecord, EventSeq, HarnessSessionId, ModelId, SessionId, SessionInfo, SessionMode,
    SessionSpec, SessionStatus, TerminalSize,
};
use vessel_pty::{PtyProcess, WindowSize};

use crate::config::RuntimeConfig;
use crate::error::SessionError;
use crate::handle::{Input, SessionHandle, Shared};
use crate::lifecycle::Lifecycle;
use crate::publish::Publisher;
use crate::ring::Ring;
use crate::stop::Stopper;
use crate::structured::StructuredActor;
use crate::terminal::TerminalActor;

/// Runs every session of one node.
///
/// [Decision Log]
/// - Purpose: `get` and `list` are synchronous (the plan's R6 API), so the
///   set of sessions must be readable without a round trip to a task.
/// - Alternatives: a registry actor (every `get` becomes async, for a map
///   that changes only on create); a concurrent map crate (a dependency for
///   one map).
/// - Choice: a `std` `RwLock` over a map of handles. It is not a shared
///   mutable "god object": it only maps ids to handles, entries are added
///   once and never changed, all session state stays inside each session's
///   actor, and the lock is never held across `.await`.
/// - Cost: ended sessions stay in the map (and keep their ring) until the
///   node restarts; `list` reports them, as the plan requires.
#[derive(Debug)]
pub struct SessionRuntime {
    config: RuntimeConfig,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    sessions: RwLock<HashMap<SessionId, SessionHandle>>,
}

/// The process-independent parts of a new session.
struct Parts {
    shared: Arc<Shared>,
    life: Lifecycle,
    durable: mpsc::Receiver<Arc<EventRecord>>,
}

/// A session that was just started.
#[derive(Debug)]
pub struct Created {
    /// The session.
    pub handle: SessionHandle,
    /// Every record, in order, exactly once. The node must drain it into the
    /// store. Bounded: if the node stops draining, the session applies
    /// backpressure (its output pump pauses) rather than dropping records.
    /// Closes after the final status record. The records are shared with
    /// the ring and live followers, not copies.
    pub durable: mpsc::Receiver<Arc<EventRecord>>,
}

impl SessionRuntime {
    /// A runtime whose sessions all stop when `shutdown` is cancelled.
    #[must_use]
    pub fn new(config: RuntimeConfig, shutdown: CancellationToken) -> Self {
        Self {
            config,
            shutdown,
            tasks: TaskTracker::new(),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Start a session. Status events are part of the stream: the first
    /// record is `Status{Starting}`.
    ///
    /// A structured session's handshake failing is not an error here: the
    /// session exists and ends with `Status{Failed}`, with the agent's log.
    ///
    /// # Errors
    /// [`SessionError::Launch`] when the harness, its adapter or the working
    /// directory is missing, [`SessionError::Spawn`] or
    /// [`SessionError::StartAgent`] when the program cannot be started, and
    /// [`SessionError::ShuttingDown`] after shutdown began.
    pub async fn create(&self, spec: SessionSpec) -> Result<Created, SessionError> {
        // Counts as a running task, so a concurrent `shutdown` waits until
        // this session is either refused or tracked.
        let _in_flight = self.tasks.token();
        if self.shutdown.is_cancelled() {
            return Err(SessionError::ShuttingDown);
        }
        match spec.mode.clone() {
            SessionMode::Terminal { size, .. } => self.create_terminal(spec, size).await,
            SessionMode::Structured { model, resume } => {
                self.create_structured(spec, model, resume).await
            }
        }
    }

    async fn create_terminal(
        &self,
        spec: SessionSpec,
        size: TerminalSize,
    ) -> Result<Created, SessionError> {
        let lookup = self.config.harness_lookup.clone();
        let launch_spec = spec.clone();
        let window = WindowSize {
            cols: size.cols,
            rows: size.rows,
        };
        // Lookup, `fork`/`exec` and thread creation all block.
        let (pty, output) = tokio::task::spawn_blocking(move || {
            let command = vessel_harness::launch(&lookup, &launch_spec)?;
            PtyProcess::spawn(&command, window).map_err(SessionError::Spawn)
        })
        .await
        .map_err(SessionError::Internal)??;
        if self.shutdown.is_cancelled() {
            // Dropping the PTY kills its program.
            drop(pty);
            return Err(SessionError::ShuttingDown);
        }

        let parts = self.parts(spec, Input::Terminal(pty.input()));
        let actor = TerminalActor {
            pty,
            output,
            life: parts.life,
        };
        Ok(self.start(parts.shared, actor.run(), parts.durable))
    }

    async fn create_structured(
        &self,
        spec: SessionSpec,
        model: Option<ModelId>,
        resume: Option<HarnessSessionId>,
    ) -> Result<Created, SessionError> {
        let lookup = self.config.harness_lookup.clone();
        let launch_spec = spec.clone();
        // Lookup and directory checks block.
        let (command, cwd) = tokio::task::spawn_blocking(move || {
            let command = vessel_harness::launch(&lookup, &launch_spec)?;
            // ACP needs an explicit directory; the launch defaults it to
            // the home directory, and the node's own is the last resort.
            let cwd = command
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .ok_or(SessionError::NoWorkingDirectory)?;
            Ok::<_, SessionError>((command, cwd))
        })
        .await
        .map_err(SessionError::Internal)??;
        // `fork`/`exec` on this thread, as `tokio::process` does.
        let (agent, input, outputs) =
            AgentProcess::spawn(&command, AgentSetup { cwd, model, resume })
                .map_err(SessionError::StartAgent)?;
        if self.shutdown.is_cancelled() {
            // Dropping the agent kills its process group.
            drop(agent);
            return Err(SessionError::ShuttingDown);
        }

        let parts = self.parts(spec, Input::Structured(input));
        let actor = StructuredActor {
            agent,
            outputs,
            life: parts.life,
            ready: false,
            disconnect: None,
        };
        Ok(self.start(parts.shared, actor.run(), parts.durable))
    }

    /// Everything a new session needs besides its process.
    fn parts(&self, spec: SessionSpec, input: Input) -> Parts {
        let (info, kill, shared) = self.shared(spec, input);
        let (durable_sender, durable) = mpsc::channel(self.config.durable_capacity.get());
        Parts {
            life: Lifecycle {
                out: Publisher::new(Arc::clone(&shared), durable_sender, info),
                stop: Stopper::new(kill, self.shutdown.clone()),
            },
            shared,
            durable,
        }
    }

    /// Initial info and the state shared by the actor and its handles.
    fn shared(
        &self,
        spec: SessionSpec,
        input: Input,
    ) -> (watch::Sender<SessionInfo>, CancellationToken, Arc<Shared>) {
        let id = SessionId::random();
        let (info, info_receiver) = watch::channel(SessionInfo {
            id,
            spec,
            status: SessionStatus::Starting,
            created_at: Utc::now(),
            ended_at: None,
            next_seq: EventSeq::ZERO,
            harness_session: None,
        });
        let kill = self.shutdown.child_token();
        let shared = Arc::new(Shared {
            id,
            info: info_receiver,
            ring: Mutex::new(Ring::new(self.config.ring_capacity)),
            input,
            kill: kill.clone(),
        });
        (info, kill, shared)
    }

    /// Spawn the actor, register the handle, and hand both ends out.
    fn start(
        &self,
        shared: Arc<Shared>,
        actor: impl Future<Output = ()> + Send + 'static,
        durable: mpsc::Receiver<Arc<EventRecord>>,
    ) -> Created {
        let handle = SessionHandle(shared);
        let id = handle.id();
        let span = tracing::info_span!("session", session = %id);
        drop(self.tasks.spawn(actor.instrument(span)));
        drop(
            self.sessions
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id, handle.clone()),
        );
        tracing::info!(session = %id, harness = %handle.info().spec.harness, "session started");
        Created { handle, durable }
    }

    /// A session of this run, live or ended.
    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<SessionHandle> {
        self.sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Every session of this run, live and ended, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(SessionHandle::info)
            .collect();
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        infos
    }

    /// Kill everything and wait until every session published its final
    /// status (`Interrupted` for sessions that were still running) and
    /// closed its durable channel. The node must keep draining durable
    /// channels while this runs.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let _ = self.tasks.close();
        self.tasks.wait().await;
    }
}
