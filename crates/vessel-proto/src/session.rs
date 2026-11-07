//! Sessions: what to run, how it is going, and what can be sent to it.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::{ContentBlock, PermissionOutcome};
use crate::harness::HarnessKind;
use crate::id::{DeviceId, EventSeq, HarnessSessionId, ModelId, NodeId, PermissionId, SessionId};
use crate::model::SessionModel;
use crate::name::ProfileName;
use crate::profile::Creator;
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
///
/// `#[non_exhaustive]` with a constructor (protocol 5): every field added
/// since protocol 2 broke each struct literal in the workspace. Build one
/// with [`SessionSpec::new`] and the `with_*` methods; fields stay public
/// for reading and for changing after construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[non_exhaustive]
pub struct SessionSpec {
    /// Harness to launch.
    pub harness: HarnessKind,
    /// Working directory. Defaults to the node's home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The profile to launch with (protocol 5, ADR 0011); `None` is the
    /// harness's default, which is how every earlier session ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// How to drive it.
    #[serde(flatten)]
    pub mode: SessionMode,
}

impl SessionSpec {
    /// Run `harness` in `mode`, in the node's home directory, with the
    /// default profile.
    #[must_use]
    pub const fn new(harness: HarnessKind, mode: SessionMode) -> Self {
        Self {
            harness,
            cwd: None,
            profile: None,
            mode,
        }
    }

    /// In this working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// With this profile.
    #[must_use]
    pub fn with_profile(mut self, profile: ProfileName) -> Self {
        self.profile = Some(profile);
        self
    }
}

/// Lifecycle of the harness process behind a session.
///
/// This is process state only. Whether a structured session is mid-turn is
/// derived from `TurnStarted`/`TurnEnded` events, so there is one source of
/// truth for each fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
///
/// `#[non_exhaustive]` for the reason given on [`SessionSpec`]; build one
/// with [`SessionInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[non_exhaustive]
pub struct SessionInfo {
    /// Session id.
    pub id: SessionId,
    /// What was asked for.
    pub spec: SessionSpec,
    /// The profile the harness runs with now: `spec.profile` until
    /// `session.move_profile` changes it (protocol 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// Who created it (protocol 5, ADR 0013). Absent from older nodes, which
    /// only ever had the owner.
    #[serde(default)]
    pub created_by: Creator,
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
    /// The home a call creating it came through, when another node asked
    /// for it (protocol 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<NodeId>,
    /// Where it came from, when not from this node (protocol 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<SessionOrigin>,
    /// The model route it was launched with, and the provider's model its
    /// harness is pinned to (protocol 7). Absent for a session on its
    /// harness's own provider, and from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<SessionModel>,
}

/// Where a session came from, when another node of the fleet had a hand in
/// it (protocol 6). `created_by` says who created it on this node; an
/// agent's own node is in `created_by` already, so `remote` names only the
/// owner or a device of the other node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Created by a call forwarded from another node.
    Remote {
        /// The node the call started on.
        node: NodeId,
        /// Who started it there.
        by: RemoteCreator,
    },
    /// Imported from a session on another node (`session.import`).
    Imported {
        /// The node it was copied from.
        node: NodeId,
        /// The session there.
        session: SessionId,
    },
}

/// The owner or a device of another node, for [`SessionOrigin::Remote`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteCreator {
    /// That node's owner.
    Owner,
    /// A device paired with that node.
    Device {
        /// Its id on that node.
        id: DeviceId,
    },
}

impl SessionInfo {
    /// A session that is just starting: no events yet, running the profile
    /// it asked for, created by the owner.
    #[must_use]
    pub fn new(id: SessionId, spec: SessionSpec, created_at: DateTime<Utc>) -> Self {
        Self {
            id,
            profile: spec.profile.clone(),
            spec,
            created_by: Creator::Owner,
            status: SessionStatus::Starting,
            created_at,
            ended_at: None,
            next_seq: EventSeq::ZERO,
            harness_session: None,
            via: None,
            origin: None,
            model: None,
        }
    }

    /// Created by `creator`.
    #[must_use]
    pub fn with_creator(mut self, creator: Creator) -> Self {
        self.created_by = creator;
        self
    }
}

/// Something a client sends into a live session. Body of `session.send`.
///
/// Sending a variant that does not match the session's mode fails with
/// `ErrorCode::Unsupported`; the node never guesses a conversion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
