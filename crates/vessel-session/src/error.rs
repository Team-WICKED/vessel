//! [`SessionError`] and its mapping to the wire.

use std::fmt;

use vessel_harness::HarnessError;
use vessel_proto::{ErrorCode, SessionInput};

/// Why a session operation failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The spec cannot be launched (harness or adapter missing, bad working
    /// directory).
    #[error(transparent)]
    Launch(#[from] HarnessError),
    /// The program could not be started.
    #[error("cannot start the session")]
    Spawn(#[source] vessel_pty::PtyError),
    /// Structured (ACP) sessions are not available in this build yet.
    #[error("structured sessions are not available yet")]
    StructuredPending,
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

impl SessionError {
    /// The wire error code for this failure.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Launch(HarnessError::NotInstalled { .. }) => ErrorCode::HarnessNotInstalled,
            Self::Launch(HarnessError::StructuredUnavailable { .. })
            | Self::StructuredPending
            | Self::Unsupported { .. } => ErrorCode::Unsupported,
            Self::Launch(HarnessError::NoSuchDirectory { .. }) => ErrorCode::InvalidRequest,
            Self::Spawn(_) => ErrorCode::SpawnFailed,
            Self::Ended => ErrorCode::Conflict,
            Self::ShuttingDown => ErrorCode::Unavailable,
            Self::Internal(_) => ErrorCode::Internal,
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
