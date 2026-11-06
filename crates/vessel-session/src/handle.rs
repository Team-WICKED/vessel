//! [`SessionHandle`]: the cheap, cloneable view of one session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use vessel_agent::AgentInput;
use vessel_proto::{EventSeq, SessionId, SessionInfo, SessionInput, TerminalInput};
use vessel_pty::{PtyInput, WindowSize};

use crate::error::{InputKind, ModeKind, SessionError};
use crate::moving::Control;
use crate::ring::{Follow, FollowError, Ring};

/// State shared between a session's actor and its handles.
///
/// Everything here is either immutable, a channel end, or the ring behind
/// its own short lock; all other session state is owned by the actor.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) id: SessionId,
    /// Latest info, published by the actor.
    pub(crate) info: watch::Receiver<SessionInfo>,
    /// Recent records and the live stream. A `std` mutex: it is held only
    /// for an append or a snapshot, never across `.await`.
    pub(crate) ring: Mutex<Ring>,
    /// Where input goes, by mode.
    pub(crate) input: Input,
    /// Cancelled to stop the session (a child of the runtime's shutdown
    /// token).
    pub(crate) kill: CancellationToken,
    /// Orders for the actor (structured sessions: moves).
    pub(crate) control: Option<mpsc::Sender<Control>>,
    /// Set just before the final status is published (`Publisher::commit`),
    /// so whoever sees that record finds it set ([`SessionHandle::has_ended`]).
    pub(crate) ended: AtomicBool,
}

impl Shared {
    /// The ring, recovering from a poisoned lock: a panic elsewhere cannot
    /// leave a half-appended record (append is push then send), so the data
    /// is still consistent.
    pub(crate) fn ring(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The input path of a session, chosen by its mode.
///
/// Input goes straight to the process side (the PTY's bounded input queue,
/// or the ACP driver), not through the actor, so a program that stops
/// reading input slows down only the caller sending it, never the actor's
/// output pump or `kill`.
#[derive(Debug)]
pub(crate) enum Input {
    Terminal(PtyInput),
    /// The current agent's input; `None` while the session moves to another
    /// profile and its agent is replaced.
    Structured(watch::Receiver<Option<AgentInput>>),
}

impl Input {
    const fn mode(&self) -> ModeKind {
        match self {
            Self::Terminal(_) => ModeKind::Terminal,
            Self::Structured(_) => ModeKind::Structured,
        }
    }
}

/// The agent input goes to now; refused while the session moves.
fn current(route: &watch::Receiver<Option<AgentInput>>) -> Result<AgentInput, SessionError> {
    route.borrow().clone().ok_or(SessionError::Moving)
}

/// A handle to one session. Cheap to clone; every clone refers to the same
/// session, which keeps running whether or not any handle exists.
#[derive(Debug, Clone)]
pub struct SessionHandle(pub(crate) Arc<Shared>);

impl SessionHandle {
    /// The session's id.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.0.id
    }

    /// Whether the session has ended: true from just before its final
    /// status (`Exited`, `Failed`, `Interrupted`) is visible to followers,
    /// readers and info watchers, so anyone who saw it end finds this true.
    /// What frees an ended session's places (the node's agent limits) checks
    /// this rather than waiting for the actor to finish.
    #[must_use]
    pub fn has_ended(&self) -> bool {
        self.0.ended.load(Ordering::Acquire)
    }

    /// A snapshot of the session's info (one clone of the current value).
    #[must_use]
    pub fn info(&self) -> SessionInfo {
        self.0.info.borrow().clone()
    }

    /// A receiver of info changes.
    ///
    /// It is notified when the status or the harness session id changes. The
    /// value's `next_seq` is kept current too, but advancing it alone does
    /// not wake watchers (it changes on every event). The sender closes once
    /// the session has published its final status.
    #[must_use]
    pub fn watch_info(&self) -> watch::Receiver<SessionInfo> {
        self.0.info.clone()
    }

    /// Send input to the session.
    ///
    /// Terminal input waits while the program's input queue is full.
    /// Structured input waits until the agent has applied it (and, while the
    /// ACP handshake runs, until it is ready).
    ///
    /// # Errors
    /// [`SessionError::Unsupported`] when the input does not match the
    /// session's mode, [`SessionError::Ended`] once the session has ended,
    /// [`SessionError::Input`] when the agent refuses it (for example a
    /// second answer to a permission request).
    pub async fn send(&self, input: SessionInput) -> Result<(), SessionError> {
        if !self.0.info.borrow().status.is_live() {
            return Err(SessionError::Ended);
        }
        match (&self.0.input, input) {
            (Input::Terminal(pty), SessionInput::Terminal { input }) => {
                let sent = match input {
                    TerminalInput::Data { data } => pty.write(data.into_bytes()).await,
                    TerminalInput::Resize { size } => {
                        pty.resize(WindowSize {
                            cols: size.cols,
                            rows: size.rows,
                        })
                        .await
                    }
                };
                sent.map_err(|vessel_pty::PtyClosed| SessionError::Ended)
            }
            (Input::Structured(route), SessionInput::Prompt { content }) => {
                Ok(current(route)?.prompt(content).await?)
            }
            (Input::Structured(route), SessionInput::CancelTurn) => {
                Ok(current(route)?.cancel_turn().await?)
            }
            (Input::Structured(route), SessionInput::AnswerPermission { id, outcome }) => {
                Ok(current(route)?.answer(id, outcome).await?)
            }
            (
                input_path @ Input::Terminal(_),
                input @ (SessionInput::Prompt { .. }
                | SessionInput::CancelTurn
                | SessionInput::AnswerPermission { .. }),
            )
            | (input_path @ Input::Structured(_), input @ SessionInput::Terminal { .. }) => {
                Err(SessionError::Unsupported {
                    input: InputKind::of(&input),
                    mode: input_path.mode(),
                })
            }
        }
    }

    /// Wait until the session has ended, then return its final info.
    ///
    /// Resolves once the actor has published the final status and let go
    /// of everything it publishes through (ring, durable channel, info), so
    /// afterwards [`SessionRuntime::remove`](crate::SessionRuntime::remove)
    /// accepts the session. Returns at once for a session that already
    /// ended. Never resolves for a session nobody stops; `kill` first if it
    /// should end.
    pub async fn ended(&self) -> SessionInfo {
        let mut info = self.0.info.clone();
        // `changed` fails only once the actor dropped the sender.
        while info.changed().await.is_ok() {}
        info.borrow().clone()
    }

    /// Whether the session can publish no more records.
    pub(crate) fn is_ended(&self) -> bool {
        self.0.ring().is_closed()
    }

    /// Stop the session: ask the process to exit, then kill its process
    /// group after a grace period. Returns immediately; the final status
    /// arrives as the session's last event.
    pub fn kill(&self) {
        self.0.kill.cancel();
    }

    /// Records from `from` onwards: the cached part now, the rest live.
    ///
    /// # Errors
    /// [`FollowError::Gap`] when `from` is older than the ring (the node
    /// then reads `from..oldest_cached` from the store and calls `follow`
    /// again), [`FollowError::Ahead`] when `from` is past the end of the
    /// log.
    pub fn follow(&self, from: EventSeq) -> Result<Follow, FollowError> {
        self.0.ring().follow(from)
    }
}
