//! [`SessionError`] and its mapping to the wire.

use std::fmt;

use vessel_agent::InputError;
use vessel_harness::HarnessError;
use vessel_proto::{ErrorCode, SessionId, SessionInput};

/// Why a session operation failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The spec cannot be launched (harness or adapter missing, bad working
    /// directory).
    #[error(transparent)]
    Launch(#[from] HarnessError),
    /// The program could not be started in a PTY.
    #[error("cannot start the session")]
    Spawn(#[source] vessel_pty::PtyError),
    /// The ACP agent could not be started.
    #[error("cannot start the agent")]
    StartAgent(#[source] vessel_agent::AgentError),
    /// Neither the spec nor the node user's home gives a working directory,
    /// and ACP requires one.
    #[error("no working directory: the spec has none and the node user has no home directory")]
    NoWorkingDirectory,
    /// The agent refused structured input (a turn already running, a
    /// permission already answered, an unknown option or id, content ACP
    /// cannot carry).
    #[error(transparent)]
    Input(InputError),
    /// The input does not apply to this session's mode.
    #[error("{input} cannot be sent to a {mode} session")]
    Unsupported {
        /// What was sent.
        input: InputKind,
        /// The session's mode.
        mode: ModeKind,
    },
    /// The session has ended and accepts no more input.
    #[error("the session has ended")]
    Ended,
    /// The runtime is shutting down and starts no new sessions.
    #[error("the node is shutting down")]
    ShuttingDown,
    /// A runtime task failed unexpectedly.
    #[error("internal session runtime failure")]
    Internal(#[source] tokio::task::JoinError),
}

/// Why [`SessionRuntime::remove`](crate::SessionRuntime::remove) refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RemoveError {
    /// The runtime has no session with this id (never created, or already
    /// removed).
    #[error("no session {0} in the runtime")]
    NotFound(SessionId),
    /// The session can still publish records; it stays. Wait for
    /// [`SessionHandle::ended`](crate::SessionHandle::ended) (after `kill`,
    /// if it should stop).
    #[error("session {0} has not ended")]
    Running(SessionId),
}

impl SessionError {
    /// The wire error code for this failure.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Launch(HarnessError::NotInstalled { .. }) => ErrorCode::HarnessNotInstalled,
            Self::Launch(HarnessError::StructuredUnavailable { .. }) | Self::Unsupported { .. } => {
                ErrorCode::Unsupported
            }
            Self::Launch(HarnessError::NoSuchDirectory { .. })
            | Self::NoWorkingDirectory
            | Self::Input(InputError::InvalidContent(_) | InputError::UnknownOption { .. }) => {
                ErrorCode::InvalidRequest
            }
            Self::Spawn(_) | Self::StartAgent(_) => ErrorCode::SpawnFailed,
            Self::Input(InputError::PermissionNotFound(_)) => ErrorCode::NotFound,
            Self::Ended
            | Self::Input(
                InputError::Closed | InputError::TurnInProgress | InputError::PermissionSettled(_),
            ) => ErrorCode::Conflict,
            Self::ShuttingDown => ErrorCode::Unavailable,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }
}

impl From<InputError> for SessionError {
    /// A closed agent connection means the session has ended, whatever the
    /// process is still doing; everything else is the agent's verdict.
    fn from(error: InputError) -> Self {
        match error {
            InputError::Closed => Self::Ended,
            refused @ (InputError::TurnInProgress
            | InputError::InvalidContent(_)
            | InputError::PermissionNotFound(_)
            | InputError::PermissionSettled(_)
            | InputError::UnknownOption { .. }) => Self::Input(refused),
        }
    }
}

/// A session's mode, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeKind {
    /// Terminal mode.
    Terminal,
    /// Structured mode.
    Structured,
}

impl fmt::Display for ModeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Terminal => "terminal",
            Self::Structured => "structured",
        })
    }
}

/// A kind of [`SessionInput`], for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// Terminal bytes or a resize.
    Terminal,
    /// A prompt.
    Prompt,
    /// A turn cancellation.
    CancelTurn,
    /// A permission answer.
    AnswerPermission,
}

impl InputKind {
    pub(crate) const fn of(input: &SessionInput) -> Self {
        match input {
            SessionInput::Terminal { .. } => Self::Terminal,
            SessionInput::Prompt { .. } => Self::Prompt,
            SessionInput::CancelTurn => Self::CancelTurn,
            SessionInput::AnswerPermission { .. } => Self::AnswerPermission,
        }
    }
}

impl fmt::Display for InputKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Terminal => "terminal input",
            Self::Prompt => "a prompt",
            Self::CancelTurn => "a turn cancellation",
            Self::AnswerPermission => "a permission answer",
        })
    }
}
