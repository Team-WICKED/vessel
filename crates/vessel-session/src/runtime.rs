//! [`SessionRuntime`]: starts sessions and keeps their handles.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use chrono::Utc;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;
use vessel_agent::{AgentProcess, AgentSetup};
use vessel_harness::{McpFile, Profile};
use vessel_proto::{
    Creator, EventRecord, HarnessSessionId, ModelId, ResolvedMcpServers, SessionId, SessionInfo,
    SessionMode, SessionSpec, TerminalSize,
};
use vessel_pty::{PtyProcess, WindowSize};

use crate::config::RuntimeConfig;
use crate::error::{RemoveError, SessionError};
use crate::handle::{Input, SessionHandle, Shared};
use crate::lifecycle::Lifecycle;
use crate::moving::{Control, Launch, MoveError, MoveOrder, ProfileMove};
use crate::publish::Publisher;
use crate::ring::Ring;
use crate::stop::Stopper;
use crate::structured::{MoveState, StructuredActor};
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
///   once and removed once, all session state stays inside each session's
///   actor, and the lock is never held across `.await`.
/// - Lifetime: an ended session stays in the map (and keeps its ring, up to
///   `ring_capacity` records) until its owner calls [`remove`](Self::remove);
///   `list` reports it until then, as the plan requires. The node removes a
///   session once its log is persisted, after which the store serves it.
///   The runtime does not evict on its own: it cannot know when the store
///   has everything.
#[derive(Debug)]
pub struct SessionRuntime {
    config: RuntimeConfig,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    sessions: RwLock<HashMap<SessionId, SessionHandle>>,
}

/// How a session is launched beyond its spec: the node decides these
/// (ADR 0011, ADR 0012), clients only name them.
///
/// [Decision Log]
/// - Purpose: profiles and MCP servers reach `vessel_harness::launch` and
///   the ACP handshake from the node, which resolves the profile name and
///   the servers' secrets.
/// - Constraint: `create(spec)` is the node's call today and `SessionSpec`
///   gains its `profile` field only in contract step 1b, so this is an
///   additive `create_with`; `create` is `create_with` with the defaults.
/// - Debug shows names only: `Profile` and `ResolvedValue` redact values.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// The profile `spec.profile` names, resolved by the caller; `None` is
    /// the spec's harness's default profile (the launch from before
    /// profiles), which is right only when the spec names no profile.
    pub profile: Option<Profile>,
    /// MCP servers, values resolved. Structured sessions get them over ACP;
    /// terminal ones in the harness's launch-time form, when it has one.
    pub mcp_servers: ResolvedMcpServers,
    /// Where a terminal session's MCP configuration file goes: an
    /// owner-only directory of the node's (ADR 0012). There is no shared
    /// fallback: without one, a terminal session whose harness reads its
    /// servers from a file is refused.
    pub mcp_dir: Option<PathBuf>,
    /// Who asks for the session (ADR 0013), recorded in its
    /// `SessionInfo::created_by`; the owner by default.
    pub created_by: Creator,
    /// Lines Vessel adds to the session's log right after
    /// `Status{Starting}`, each a `HarnessLog` record (in either mode),
    /// starting with `vessel:` by convention: for example MCP servers left
    /// out because a secret is missing (ADR 0012).
    pub notices: Vec<String>,
    /// The session's id, when the caller needs it before the session starts
    /// (an agent credential is bound to it, ADR 0013); a fresh random one
    /// otherwise. An id the runtime already has is refused.
    pub id: Option<SessionId>,
}

/// What a session records about its start besides its spec.
struct Record {
    id: SessionId,
    created_by: Creator,
    notices: Vec<String>,
}

/// The process-independent parts of a new session.
struct Parts {
    shared: Arc<Shared>,
    life: Lifecycle,
    durable: mpsc::Receiver<Arc<EventRecord>>,
}

/// A terminal session's MCP servers and where their file goes.
struct TerminalMcp {
    servers: ResolvedMcpServers,
    dir: Option<PathBuf>,
}

/// What a structured session starts with besides its process.
struct AgentStart {
    model: Option<ModelId>,
    resume: Option<HarnessSessionId>,
    mcp_servers: ResolvedMcpServers,
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
        self.create_with(spec, LaunchOptions::default()).await
    }

    /// [`create`](Self::create) with a profile and MCP servers.
    ///
    /// A terminal session's MCP configuration file, if its harness takes
    /// one, is written right before the program starts and removed when the
    /// session ends. Servers a terminal harness cannot be given are logged
    /// by name; the session starts without them.
    ///
    /// # Errors
    /// As [`create`](Self::create), and [`SessionError::Launch`] for a
    /// profile of another harness or servers the harness cannot take,
    /// [`SessionError::McpFile`] when the MCP file cannot be written.
    pub async fn create_with(
        &self,
        spec: SessionSpec,
        options: LaunchOptions,
    ) -> Result<Created, SessionError> {
        // Counts as a running task, so a concurrent `shutdown` waits until
        // this session is either refused or tracked.
        let _in_flight = self.tasks.token();
        if self.shutdown.is_cancelled() {
            return Err(SessionError::ShuttingDown);
        }
        let LaunchOptions {
            profile,
            mcp_servers,
            mcp_dir,
            created_by,
            notices,
            id,
        } = options;
        let id = id.unwrap_or_else(SessionId::random);
        if self.get(id).is_some() {
            return Err(SessionError::DuplicateId(id));
        }
        let record = Record {
            id,
            created_by,
            notices,
        };
        let profile = profile.unwrap_or_else(|| Profile::default_for(spec.harness));
        match spec.mode.clone() {
            SessionMode::Terminal { size, .. } => {
                let mcp = TerminalMcp {
                    servers: mcp_servers,
                    dir: mcp_dir,
                };
                self.create_terminal(spec, size, profile, mcp, record).await
            }
            SessionMode::Structured { model, resume } => {
                let start = AgentStart {
                    model,
                    resume,
                    mcp_servers,
                };
                self.create_structured(spec, profile, start, record).await
            }
        }
    }

    async fn create_terminal(
        &self,
        spec: SessionSpec,
        size: TerminalSize,
        profile: Profile,
        mcp: TerminalMcp,
        record: Record,
    ) -> Result<Created, SessionError> {
        let lookup = self.config.harness_lookup.clone();
        let launch_spec = spec.clone();
        let window = WindowSize {
            cols: size.cols,
            rows: size.rows,
        };
        // Lookup, file writes, `fork`/`exec` and thread creation all block.
        let (pty, output, mcp_file) = tokio::task::spawn_blocking(move || {
            let launch = vessel_harness::launch(
                &lookup,
                &launch_spec,
                &profile,
                &mcp.servers,
                mcp.dir.as_deref(),
            )?;
            if !launch.unattached.is_empty() {
                tracing::warn!(
                    servers = ?launch.unattached,
                    harness = %launch_spec.harness,
                    "this harness takes no MCP servers at launch in terminal mode; the session starts without them"
                );
            }
            // Written last and dropped (removed) if the program fails to
            // start.
            let mcp_file = launch
                .mcp_file
                .map(McpFile::write)
                .transpose()
                .map_err(SessionError::McpFile)?;
            let (pty, output) =
                PtyProcess::spawn(&launch.command, window).map_err(SessionError::Spawn)?;
            Ok::<_, SessionError>((pty, output, mcp_file))
        })
        .await
        .map_err(SessionError::Internal)??;
        if self.shutdown.is_cancelled() {
            // Dropping the PTY kills its program.
            drop(pty);
            return Err(SessionError::ShuttingDown);
        }

        let parts = self.parts(spec, record, Input::Terminal(pty.input()), None);
        let actor = TerminalActor {
            pty,
            output,
            life: parts.life,
            _mcp_file: mcp_file,
        };
        Ok(self.start(parts.shared, actor.run(), parts.durable))
    }

    async fn create_structured(
        &self,
        spec: SessionSpec,
        profile: Profile,
        start: AgentStart,
        record: Record,
    ) -> Result<Created, SessionError> {
        let lookup = self.config.harness_lookup.clone();
        let launch_spec = spec.clone();
        // Lookup and directory checks block.
        let (command, cwd) = tokio::task::spawn_blocking(move || {
            // Structured sessions get their MCP servers over ACP, not here.
            let none = ResolvedMcpServers::new();
            let command =
                vessel_harness::launch(&lookup, &launch_spec, &profile, &none, None)?.command;
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
        let setup = AgentSetup {
            cwd,
            model: start.model,
            resume: start.resume,
            mcp_servers: start.mcp_servers,
            handshake_timeout: self.config.agent_handshake_timeout,
        };
        // `fork`/`exec` on this thread, as `tokio::process` does.
        let (agent, input, outputs) =
            AgentProcess::spawn(&command, setup.clone()).map_err(SessionError::StartAgent)?;
        if self.shutdown.is_cancelled() {
            // Dropping the agent kills its process group.
            drop(agent);
            return Err(SessionError::ShuttingDown);
        }

        let profile = spec.profile.clone();
        let (route, routed) = watch::channel(Some(input.clone()));
        // One move at a time; a second waits for the first's answer.
        let (control, orders) = mpsc::channel(1);
        let parts = self.parts(spec, record, Input::Structured(routed), Some(control));
        let actor = StructuredActor {
            agent,
            outputs,
            outputs_open: true,
            input,
            route,
            control: orders,
            launch: Launch { command, setup },
            profile,
            life: parts.life,
            ready: false,
            disconnect: None,
            failed_before_kill: false,
            move_state: MoveState::default(),
        };
        Ok(self.start(parts.shared, actor.run(), parts.durable))
    }

    /// Move a live structured session to another profile (ADR 0011): link
    /// its files into the target home, restart its agent there with
    /// `session/load`, and publish `ProfileChanged`. The session keeps its
    /// id, its log and its subscribers. See `moving` for the order and what
    /// a failure leaves.
    ///
    /// # Errors
    /// [`SessionError::NotFound`], [`SessionError::Ended`],
    /// [`SessionError::Launch`] (the profile is for another harness, or the
    /// adapter is missing), and [`SessionError::Move`] for the rest.
    pub async fn move_profile(
        &self,
        id: SessionId,
        request: ProfileMove,
    ) -> Result<SessionInfo, SessionError> {
        let handle = self.get(id).ok_or(SessionError::NotFound(id))?;
        let info = handle.info();
        if !info.status.is_live() {
            return Err(SessionError::Ended);
        }
        let SessionMode::Structured { model, .. } = &info.spec.mode else {
            return Err(MoveError::Terminal.into());
        };
        let model = model.clone();
        let harness_session = info.harness_session.clone().ok_or(MoveError::NotReady)?;
        let control = handle.0.control.clone().ok_or(MoveError::Terminal)?;
        let ProfileMove {
            profile,
            from_home,
            to_home,
            mcp_servers,
            notices,
            force,
        } = request;
        let to = (!profile.is_default()).then(|| profile.name().clone());
        let spec = info.spec.with_profile(profile.name().clone());
        let lookup = self.config.harness_lookup.clone();
        let session = harness_session.clone();
        let from = from_home.clone();
        let (command, cwd, files) = tokio::task::spawn_blocking(move || {
            let none = ResolvedMcpServers::new();
            let command = vessel_harness::launch(&lookup, &spec, &profile, &none, None)?.command;
            let cwd = command
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .ok_or(SessionError::NoWorkingDirectory)?;
            let files = vessel_harness::session_files(spec.harness, &from, &cwd, &session)
                .map_err(MoveError::Files)?;
            Ok::<_, SessionError>((command, cwd, files))
        })
        .await
        .map_err(SessionError::Internal)??;
        let setup = AgentSetup {
            cwd,
            model,
            resume: Some(harness_session),
            mcp_servers,
            handshake_timeout: self.config.agent_handshake_timeout,
        };
        let (reply, answer) = oneshot::channel();
        let order = MoveOrder {
            command,
            setup,
            to,
            files,
            from_home,
            to_home,
            notices,
            force,
            reply,
        };
        if control.send(Control::Move(order)).await.is_err() {
            return Err(SessionError::Ended);
        }
        answer.await.map_err(|_ended| SessionError::Ended)??;
        Ok(handle.info())
    }

    /// Everything a new session needs besides its process.
    fn parts(
        &self,
        spec: SessionSpec,
        record: Record,
        input: Input,
        control: Option<mpsc::Sender<Control>>,
    ) -> Parts {
        let Record {
            id,
            created_by,
            notices,
        } = record;
        let (info, kill, shared) = self.shared(id, spec, created_by, input, control);
        let (durable_sender, durable) = mpsc::channel(self.config.durable_capacity.get());
        Parts {
            life: Lifecycle {
                out: Publisher::new(Arc::clone(&shared), durable_sender, info),
                stop: Stopper::new(kill, self.shutdown.clone()),
                notices,
            },
            shared,
            durable,
        }
    }

    /// Initial info and the state shared by the actor and its handles.
    fn shared(
        &self,
        id: SessionId,
        spec: SessionSpec,
        created_by: Creator,
        input: Input,
        control: Option<mpsc::Sender<Control>>,
    ) -> (watch::Sender<SessionInfo>, CancellationToken, Arc<Shared>) {
        let (info, info_receiver) =
            watch::channel(SessionInfo::new(id, spec, Utc::now()).with_creator(created_by));
        let kill = self.shutdown.child_token();
        let shared = Arc::new(Shared {
            id,
            info: info_receiver,
            ring: Mutex::new(Ring::new(self.config.ring_capacity)),
            input,
            kill: kill.clone(),
            control,
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

    /// A session of this run, live or ended (until [`remove`](Self::remove)d).
    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<SessionHandle> {
        self.sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Every session of this run, live and ended (until removed), newest
    /// first.
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

    /// Forget an ended session: the runtime drops its handle, so the
    /// session's ring, input path and remaining state are freed once the
    /// last other holder lets go. Returns the session's final info.
    ///
    /// Legal only once the session has ended, meaning its actor published
    /// the final status and closed the ring and the durable channel (no
    /// record can follow). The node calls it when its persistence task has
    /// drained the durable channel to its end, which implies that, or after
    /// [`SessionHandle::ended`] otherwise; from then on the session exists
    /// only in the store.
    ///
    /// Afterwards [`get`](Self::get) returns `None` and [`list`](Self::list)
    /// leaves the session out. Nothing else changes for anyone already
    /// holding it:
    /// - other `SessionHandle` clones stay valid: `info` returns the final
    ///   info, `send` fails with [`SessionError::Ended`], `kill` does
    ///   nothing, and `follow` still serves the ring (with a receiver that
    ///   is already closed);
    /// - live followers already have every record up to the final status
    ///   and then see `Closed`, exactly as without the removal.
    ///
    /// # Errors
    /// [`RemoveError::NotFound`] for an unknown or already removed id, and
    /// [`RemoveError::Running`] while the session can still publish records
    /// (nothing is removed).
    pub fn remove(&self, id: SessionId) -> Result<SessionInfo, RemoveError> {
        // Checked and removed under one write lock, so a concurrent call
        // cannot remove it in between. Lock order: the map, then the
        // session's ring (nothing takes them the other way round).
        let removed = match self
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id)
        {
            Entry::Occupied(entry) if entry.get().is_ended() => entry.remove(),
            Entry::Occupied(_) => return Err(RemoveError::Running(id)),
            Entry::Vacant(_) => return Err(RemoveError::NotFound(id)),
        };
        tracing::debug!(session = %id, "session removed from the runtime");
        Ok(removed.info())
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
