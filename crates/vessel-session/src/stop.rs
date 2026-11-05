//! [`Stopper`]: the escalation from "please exit" to "killed".

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long a program gets to exit after the polite signal before its
/// process group is killed. Long enough for a harness to save its session,
/// short enough that node shutdown stays snappy.
const GRACE: Duration = Duration::from_secs(2);

/// Tracks a session's stop request and tells the actor when to signal.
#[derive(Debug)]
pub(crate) struct Stopper {
    kill: CancellationToken,
    shutdown: CancellationToken,
    state: State,
}

#[derive(Debug, Clone, Copy)]
enum State {
    /// Nobody asked the session to stop.
    Idle,
    /// The polite signal was sent; force at the deadline.
    Asked { deadline: Instant },
    /// The group was killed; nothing more to do.
    Forced,
}

/// What the actor should do to its process now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Send the polite stop signal.
    Ask,
    /// Kill the process group.
    Force,
}

impl Stopper {
    /// `kill` stops this session; `shutdown` is the runtime's token (an
    /// ancestor of `kill`), consulted to tell a node shutdown from a kill.
    pub(crate) const fn new(kill: CancellationToken, shutdown: CancellationToken) -> Self {
        Self {
            kill,
            shutdown,
            state: State::Idle,
        }
    }

    /// Resolves when the process should be signalled next; never resolves
    /// once it has been killed. Cancel-safe.
    pub(crate) async fn next(&self) -> Step {
        match self.state {
            State::Idle => {
                self.kill.cancelled().await;
                Step::Ask
            }
            State::Asked { deadline } => {
                tokio::time::sleep_until(deadline).await;
                Step::Force
            }
            State::Forced => std::future::pending().await,
        }
    }

    /// Record that `step` was carried out.
    pub(crate) fn advance(&mut self, step: Step) {
        self.state = match step {
            // If the deadline is not representable, force right away.
            Step::Ask => State::Asked {
                deadline: Instant::now()
                    .checked_add(GRACE)
                    .unwrap_or_else(Instant::now),
            },
            Step::Force => State::Forced,
        };
    }

    /// Whether no stop has started yet.
    pub(crate) const fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    /// Whether the session is ending because the runtime shut down.
    pub(crate) fn interrupted(&self) -> bool {
        self.shutdown.is_cancelled()
    }
}
