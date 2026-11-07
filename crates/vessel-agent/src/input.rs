//! [`AgentInput`]: the cloneable command side of an agent session.

use tokio::sync::{mpsc, oneshot};
use vessel_proto::{ContentBlock, PermissionId, PermissionOutcome};

use crate::error::InputError;

/// Where a command's result goes.
pub(crate) type Reply = oneshot::Sender<Result<(), InputError>>;

/// A command for the driver, answered once it has been applied.
#[derive(Debug)]
pub(crate) enum Command {
    Prompt {
        content: Vec<ContentBlock>,
        reply: Reply,
    },
    CancelTurn {
        reply: Reply,
    },
    Answer {
        id: PermissionId,
        outcome: PermissionOutcome,
        reply: Reply,
    },
    TurnRunning {
        reply: oneshot::Sender<bool>,
    },
}

/// Sends prompts, cancellations and permission answers to an agent.
///
/// Cheap to clone. Each call waits until the driver has applied it, so the
/// caller learns about a conflict (a turn already running, a permission
/// already answered) directly. Commands sent while the ACP handshake is
/// still running wait for it to finish.
#[derive(Debug, Clone)]
pub struct AgentInput {
    commands: mpsc::Sender<Command>,
}

impl AgentInput {
    pub(crate) const fn new(commands: mpsc::Sender<Command>) -> Self {
        Self { commands }
    }

    /// Start a turn with this prompt. Returns once the prompt was sent;
    /// the turn's progress and end arrive as events.
    ///
    /// # Errors
    /// [`InputError::TurnInProgress`] while a turn runs,
    /// [`InputError::InvalidContent`] for opaque blocks that are not ACP
    /// content, [`InputError::Closed`] after the connection ended.
    pub async fn prompt(&self, content: Vec<ContentBlock>) -> Result<(), InputError> {
        self.call(|reply| Command::Prompt { content, reply }).await
    }

    /// Cancel the turn in progress (a no-op when there is none). Pending
    /// permission requests are withdrawn as `Cancelled`.
    ///
    /// # Errors
    /// [`InputError::Closed`] after the connection ended.
    pub async fn cancel_turn(&self) -> Result<(), InputError> {
        self.call(|reply| Command::CancelTurn { reply }).await
    }

    /// Answer a pending permission request.
    ///
    /// # Errors
    /// [`InputError::PermissionSettled`] when another answer won,
    /// [`InputError::PermissionNotFound`] for an unknown id,
    /// [`InputError::UnknownOption`] for an option the request did not
    /// offer, [`InputError::Closed`] after the connection ended.
    pub async fn answer(
        &self,
        id: PermissionId,
        outcome: PermissionOutcome,
    ) -> Result<(), InputError> {
        self.call(|reply| Command::Answer { id, outcome, reply })
            .await
    }

    /// Whether a turn is running, as the driver sees it once every command
    /// sent before this one has been applied (a prompt that was already on
    /// its way counts). For moving a session to another profile, which must
    /// not stop the agent mid-turn.
    ///
    /// # Errors
    /// [`InputError::Closed`] after the connection ended.
    pub async fn turn_running(&self) -> Result<bool, InputError> {
        let (reply, result) = oneshot::channel();
        if self
            .commands
            .send(Command::TurnRunning { reply })
            .await
            .is_err()
        {
            return Err(InputError::Closed);
        }
        result.await.map_err(|_closed| InputError::Closed)
    }

    async fn call(&self, command: impl FnOnce(Reply) -> Command) -> Result<(), InputError> {
        let (reply, result) = oneshot::channel();
        if self.commands.send(command(reply)).await.is_err() {
            return Err(InputError::Closed);
        }
        result.await.unwrap_or(Err(InputError::Closed))
    }
}
