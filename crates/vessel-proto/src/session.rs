//! Sessions: what to run, how it is going, and what can be sent to it.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::{ContentBlock, PermissionOutcome};
use crate::harness::HarnessKind;
use crate::id::{EventSeq, HarnessSessionId, ModelId, PermissionId, SessionId};
use crate::terminal::{TerminalInput, TerminalSize};

/// How a harness is driven.
///
/// [Decision Log]
/// - Purpose: Vessel must keep every harness feature as-is (ORIGIN item 2)
///   and also give clients a structured, harness-independent view.
/// - Alternatives: only a PTY (no structure; clients scrape a TUI); only
///   structured protocols (loses TUI-only features and the familiar UI);
///   one hand-written parser per harness's own JSON stream (Paseo's path:
///   highest fidelity, but N adapters of 5-8k lines each).
/// - Choice: two modes. `Terminal` runs the real TUI in a PTY. `Structured`
///   speaks ACP (Agent Client Protocol) to the harness or its adapter, so one
///   client implementation covers all four harnesses. See ADR 0003.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionMode {
    /// The harness's own TUI in a pseudo-terminal; clients attach and see
    /// exactly what a local terminal would.
    Terminal {
        /// Extra arguments passed verbatim to the harness binary.
        #[serde(default)]
        args: Vec<String>,
        /// Initial window size.
        #[serde(default)]
        size: TerminalSize,
    },
    /// The harness driven over ACP; output is a typed event timeline.
    Structured {
        /// Model to request, if not the harness default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
        /// Resume this harness session instead of starting a new one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<HarnessSessionId>,
    },
}

/// What to run. Body of `session.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Harness to launch.
    pub harness: HarnessKind,
    /// Working directory. Defaults to the node's home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// How to drive it.
    #[serde(flatten)]
    pub mode: SessionMode,
}

/// Lifecycle of the harness process behind a session.
///
/// This is process state only. Whether a structured session is mid-turn is
/// derived from `TurnStarted`/`TurnEnded` events, so there is one source of
/// truth for each fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionStatus {
    /// Spawning, or (structured) waiting for the ACP handshake.
    Starting,
    /// The process is alive and accepting input.
    Running,
    /// The process exited on its own or was killed.
    Exited {
        /// Exit code, if the OS reported one (none when killed by a signal).
        code: Option<i32>,
    },
    /// The process could not be started or the handshake failed.
    Failed {
        /// Human-readable reason.
        message: String,
    },
    /// The node stopped while the session was live. The log is intact; a
    /// structured session can be resumed through its `harness_session`.
    Interrupted,
}

impl SessionStatus {
    /// Whether the session can still accept input.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// A session as reported by the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Session id.
    pub id: SessionId,
    /// What was asked for.
    pub spec: SessionSpec,
    /// Current process state.
    pub status: SessionStatus,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the process ended, if it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Sequence number the next event will get (= number of events so far).
    pub next_seq: EventSeq,
    /// The harness's own session id, once a structured session has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session: Option<HarnessSessionId>,
}

/// Something a client sends into a live session. Body of `session.send`.
///
/// Sending a variant that does not match the session's mode fails with
/// `ErrorCode::Unsupported`; the node never guesses a conversion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionInput {
    /// Terminal mode: keystrokes or a resize.
    Terminal {
        /// The input.
        input: TerminalInput,
    },
    /// Structured mode: start a user turn.
    Prompt {
        /// The user's message.
        content: Vec<ContentBlock>,
    },
    /// Structured mode: cancel the turn in progress.
    CancelTurn,
    /// Structured mode: answer a pending permission request. The first answer
    /// wins; later answers fail with `ErrorCode::Conflict`.
    AnswerPermission {
        /// Which request.
        id: PermissionId,
        /// The choice.
        outcome: PermissionOutcome,
    },
}
