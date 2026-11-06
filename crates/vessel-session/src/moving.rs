//! Moving a structured session to another profile (ADR 0011, H2): the
//! same conversation, resumed by the harness under another account.
//!
//! [Decision Log]
//! - Purpose: a session that reached a usage limit continues under another
//!   login without losing its id, its log or its subscribers.
//! - Order, chosen so a failure changes as little as possible:
//!   1. New input is held (`SessionError::Moving`), then the driver says
//!      whether a turn runs; it answers after every command sent before,
//!      so a prompt already on its way counts. Mid-turn is refused unless
//!      `force`, which cancels the turn and waits (bounded) for it to end.
//!   2. The transcript is hard-linked into the target home while the old
//!      agent still runs: a link failure leaves everything as it was
//!      (`link_session` takes back what it made) and the old agent keeps
//!      the session.
//!   3. The old agent is stopped (`SIGTERM`, then `SIGKILL` after a grace)
//!      and its last outputs published; then the new one starts with
//!      `session/load`. Its replay of the conversation is dropped (the log
//!      has it already) until it is ready.
//!   4. Only then is `ProfileChanged` published. If the new agent cannot
//!      start, the old command is started again on the same conversation,
//!      and the move fails without a `ProfileChanged`.
//! - The session's status stays `Running` throughout; the move is visible
//!   as `ProfileChanged`, and as a pause in output.
//! - A `kill` during a move wins: the move stops where it is and the
//!   session ends as any killed session does.
//! - Trade-off: links made for a move whose new agent did not start stay
//!   in the target home. They are the same files, so nothing forks; a
//!   retry on Unix finds them already linked (on Windows `link_session`
//!   cannot tell, and reports a conflict). Taking them back would need
//!   to know which files the failed agent created meanwhile.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;
use vessel_agent::{AgentError, AgentProcess, AgentSetup, CommandSpec, Disconnect, Signal};
use vessel_harness::{LinkError, Profile, SessionFiles, SessionFilesError};
use vessel_proto::{ProfileName, ResolvedMcpServers, SessionEvent};

use crate::lifecycle::drain_deadline;
use crate::structured::StructuredActor;

/// How long a forced move waits for the cancelled turn to end before it
/// stops the agent anyway (which ends the turn too).
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// How long the old agent may take to exit after `SIGTERM`.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// What the node asks for (`SessionRuntime::move_profile`).
#[derive(Debug)]
pub struct ProfileMove {
    /// The profile to run under, resolved by the caller.
    pub profile: Profile,
    /// The current profile's harness home (`vessel_harness::home`), where
    /// the session's files are.
    pub from_home: PathBuf,
    /// The target profile's harness home, where they are linked.
    pub to_home: PathBuf,
    /// MCP servers for the new agent.
    pub mcp_servers: ResolvedMcpServers,
    /// Move even mid-turn, cancelling the turn.
    pub force: bool,
}

/// Why a session did not move.
#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    /// Terminal sessions cannot move: the harness's own TUI owns its state.
    #[error("terminal sessions cannot move to another profile")]
    Terminal,
    /// The agent has not finished starting, so there is no conversation id
    /// to resume yet.
    #[error("the session's agent is still starting")]
    NotReady,
    /// A turn is running and the move was not forced.
    #[error("a turn is running; wait for it to end, or force the move to cancel it")]
    TurnInProgress,
    /// The session's files could not be found.
    #[error(transparent)]
    Files(#[from] SessionFilesError),
    /// The session's files could not be linked into the target home;
    /// nothing changed.
    #[error(transparent)]
    Link(#[from] LinkError),
    /// The new agent did not start; the session went back to its previous
    /// profile's agent.
    #[error("the agent did not start under the new profile: {0}")]
    Restart(Restart),
    /// The session was stopped during the move.
    #[error("the session was stopped during the move")]
    Stopped,
    /// A task the move ran failed unexpectedly.
    #[error("a task of the move failed")]
    Task(#[source] tokio::task::JoinError),
}

/// Why the new agent did not start.
#[derive(Debug, thiserror::Error)]
pub enum Restart {
    /// It could not be spawned.
    #[error("{0}")]
    Spawn(#[source] AgentError),
    /// Its ACP connection failed before it was ready.
    #[error("{0}")]
    Handshake(Disconnect),
    /// It exited before it was ready.
    #[error("it exited (code {0:?}) before it was ready")]
    Exited(Option<i32>),
}

/// A message to a structured session's actor.
#[derive(Debug)]
pub(crate) enum Control {
    Move(MoveOrder),
}

/// Everything the actor needs to move.
#[derive(Debug)]
pub(crate) struct MoveOrder {
    pub(crate) command: CommandSpec,
    pub(crate) setup: AgentSetup,
    /// The target profile's name; `None` is the harness's default.
    pub(crate) to: Option<ProfileName>,
    pub(crate) files: SessionFiles,
    pub(crate) from_home: PathBuf,
    pub(crate) to_home: PathBuf,
    pub(crate) force: bool,
    pub(crate) reply: oneshot::Sender<Result<(), MoveError>>,
}

/// How a session's agent is started: kept so a failed move can start the
/// previous one again.
#[derive(Debug, Clone)]
pub(crate) struct Launch {
    pub(crate) command: CommandSpec,
    pub(crate) setup: AgentSetup,
}

impl StructuredActor {
    /// Carry out a move and answer it.
    pub(crate) async fn move_profile(&mut self, order: MoveOrder) {
        let MoveOrder {
            command,
            setup,
            to,
            files,
            from_home,
            to_home,
            force,
            reply,
        } = order;
        let _held = self.route.send_replace(None);
        self.move_state.active = true;
        let outcome = self
            .try_move(
                Launch { command, setup },
                to,
                (files, from_home, to_home),
                force,
            )
            .await;
        self.move_state.active = false;
        let _none = self.route.send_replace(Some(self.input.clone()));
        if let Err(error) = &outcome {
            tracing::info!(%error, "the session did not move to another profile");
        }
        let _gone = reply.send(outcome);
    }

    async fn try_move(
        &mut self,
        launch: Launch,
        to: Option<ProfileName>,
        (files, from_home, to_home): (SessionFiles, PathBuf, PathBuf),
        force: bool,
    ) -> Result<(), MoveError> {
        let input = self.input.clone();
        let running = self.pump(async move { input.turn_running().await }).await?;
        if running.unwrap_or(false) {
            if !force {
                return Err(MoveError::TurnInProgress);
            }
            self.move_state.turn_over = false;
            let input = self.input.clone();
            let _cancelled = self.pump(async move { input.cancel_turn().await }).await?;
            self.until_turn_over().await?;
        }

        let linked = self
            .pump(tokio::task::spawn_blocking(move || {
                vessel_harness::link_session(&files, &from_home, &to_home)
            }))
            .await?;
        let linked = linked.map_err(MoveError::Task)??;
        tracing::debug!(?linked, "session files linked");

        self.stop_agent().await?;
        // The previous command resumes the same conversation if it has to
        // start again (it may have started a new one the first time).
        let mut previous = self.launch.clone();
        previous.setup.resume.clone_from(&launch.setup.resume);
        match self.start_agent(&launch).await? {
            Ok(()) => {
                let from = std::mem::replace(&mut self.profile, to.clone());
                self.launch = launch;
                self.life
                    .publish(&mut self.agent, SessionEvent::ProfileChanged { from, to })
                    .await;
                Ok(())
            }
            Err(failure) => {
                tracing::warn!(%failure, "the agent did not start under the new profile; restarting the previous one");
                if let Err(again) = self.start_agent(&previous).await? {
                    tracing::warn!(%again, "the previous agent did not start again either");
                }
                Err(MoveError::Restart(failure))
            }
        }
    }

    /// Run `future` while publishing the agent's outputs; `Stopped` if the
    /// session is stopped meanwhile.
    async fn pump<F: Future>(&mut self, future: F) -> Result<F::Output, MoveError> {
        let mut future = std::pin::pin!(future);
        loop {
            tokio::select! {
                biased;
                step = self.life.stop.next() => {
                    self.life.apply(&mut self.agent, step);
                    return Err(MoveError::Stopped);
                }
                done = &mut future => return Ok(done),
                output = self.outputs.next(), if self.outputs_open => match output {
                    Some(output) => self.output(output).await,
                    None => self.outputs_open = false,
                },
            }
        }
    }

    /// Publish outputs until the cancelled turn ends, at most
    /// [`CANCEL_GRACE`]; stopping the agent ends it otherwise.
    async fn until_turn_over(&mut self) -> Result<(), MoveError> {
        let deadline = Instant::now()
            .checked_add(CANCEL_GRACE)
            .unwrap_or_else(Instant::now);
        while !self.move_state.turn_over && self.outputs_open {
            tokio::select! {
                biased;
                step = self.life.stop.next() => {
                    self.life.apply(&mut self.agent, step);
                    return Err(MoveError::Stopped);
                }
                output = self.outputs.next() => match output {
                    Some(output) => self.output(output).await,
                    None => self.outputs_open = false,
                },
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!("the cancelled turn did not end in time; stopping the agent anyway");
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Stop the current agent (`SIGTERM`, then `SIGKILL` after
    /// [`STOP_GRACE`]) and publish what it still had to say.
    async fn stop_agent(&mut self) -> Result<(), MoveError> {
        signal(&mut self.agent, Signal::Terminate);
        let grace = tokio::time::sleep(STOP_GRACE);
        let mut grace = std::pin::pin!(grace);
        let mut forced = false;
        loop {
            tokio::select! {
                biased;
                step = self.life.stop.next() => {
                    self.life.apply(&mut self.agent, step);
                    return Err(MoveError::Stopped);
                }
                _code = self.agent.wait() => break,
                output = self.outputs.next(), if self.outputs_open => match output {
                    Some(output) => self.output(output).await,
                    None => self.outputs_open = false,
                },
                () = &mut grace, if !forced => {
                    forced = true;
                    signal(&mut self.agent, Signal::Kill);
                }
            }
        }
        let deadline = drain_deadline();
        while self.outputs_open {
            tokio::select! {
                output = self.outputs.next() => match output {
                    Some(output) => self.output(output).await,
                    None => self.outputs_open = false,
                },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        Ok(())
    }

    /// Start `launch` in place of the stopped agent and wait until it is
    /// ready. The outer error is a stop of the whole session; the inner one
    /// is this agent's failure to start.
    async fn start_agent(&mut self, launch: &Launch) -> Result<Result<(), Restart>, MoveError> {
        let (agent, input, outputs) =
            match AgentProcess::spawn(&launch.command, launch.setup.clone()) {
                Ok(started) => started,
                Err(error) => return Ok(Err(Restart::Spawn(error))),
            };
        self.agent = agent;
        self.input = input;
        self.outputs = outputs;
        self.outputs_open = true;
        self.move_state.replaying = true;
        self.move_state.disconnect = None;
        loop {
            if !self.move_state.replaying {
                return Ok(Ok(()));
            }
            if let Some(disconnect) = self.move_state.disconnect.take() {
                signal(&mut self.agent, Signal::Kill);
                let _code = self.agent.wait().await;
                return Ok(Err(Restart::Handshake(disconnect)));
            }
            tokio::select! {
                biased;
                step = self.life.stop.next() => {
                    self.life.apply(&mut self.agent, step);
                    return Err(MoveError::Stopped);
                }
                output = self.outputs.next(), if self.outputs_open => match output {
                    Some(output) => self.output(output).await,
                    None => self.outputs_open = false,
                },
                code = self.agent.wait(), if !self.outputs_open => {
                    return Ok(Err(Restart::Exited(code)));
                }
            }
        }
    }
}

/// Signal the agent, logging a failure (the move goes on: a stop that
/// cannot be delivered shows as an agent that does not exit).
fn signal(agent: &mut AgentProcess, signal: Signal) {
    if let Err(error) = agent.signal(signal) {
        tracing::warn!(pid = agent.pid(), %error, ?signal, "cannot signal the agent during a move");
    }
}
