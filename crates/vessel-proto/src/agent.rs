//! The structured-session timeline: Vessel's harness-independent view of an
//! agent's turn.
//!
//! The shapes follow ACP's `session/update` model (the transport used by
//! structured mode, see ADR 0003) and Paseo's closed timeline set, trimmed to
//! what clients need. They are Vessel's own types, not re-exports of the ACP
//! crate, so an ACP version bump never changes Vessel's wire format; the
//! mapping lives in `vessel-agent`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::id::{HarnessSessionId, PermissionId, PermissionOptionId, ToolCallId};
use crate::raw::RawJson;

/// One entry in a structured session's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The harness accepted the session; its id is kept for resume.
    Ready {
        /// The harness's session id.
        harness_session: HarnessSessionId,
    },
    /// A user turn began (echoes the prompt as the harness received it).
    TurnStarted {
        /// The prompt.
        content: Vec<ContentBlock>,
    },
    /// A chunk of an earlier user message, replayed by the harness when a
    /// session is resumed. Prompts sent through Vessel appear as
    /// `TurnStarted` instead.
    UserMessageChunk {
        /// The chunk.
        content: ContentBlock,
    },
    /// A chunk of the agent's visible reply.
    MessageChunk {
        /// The chunk.
        content: ContentBlock,
    },
    /// A chunk of the agent's reasoning.
    ThoughtChunk {
        /// The chunk.
        content: ContentBlock,
    },
    /// A tool call was announced.
    ToolCall(ToolCall),
    /// A tool call progressed or finished.
    ToolCallUpdate(ToolCallUpdate),
    /// The agent's current plan (replaces any earlier plan).
    Plan {
        /// Plan entries in order.
        entries: Vec<PlanEntry>,
    },
    /// The harness asks a human to approve something.
    PermissionRequested(PermissionRequest),
    /// A pending permission request was settled (by a client or by the turn
    /// ending).
    PermissionResolved {
        /// Which request.
        id: PermissionId,
        /// How it was settled.
        outcome: PermissionOutcome,
    },
    /// The turn finished normally.
    TurnEnded {
        /// Why it stopped.
        reason: StopReason,
    },
    /// The turn failed (the harness returned an error for the prompt).
    TurnFailed {
        /// Human-readable reason.
        message: String,
    },
    /// A line the harness wrote to stderr. Kept because login and
    /// configuration problems surface only there.
    HarnessLog {
        /// The line, without its newline.
        line: String,
    },
}

/// A piece of message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain or markdown text.
    Text {
        /// The text.
        text: String,
    },
    /// A reference to a file or URL.
    ResourceLink {
        /// URI of the resource.
        uri: String,
        /// Display name.
        name: String,
    },
    /// Any other block (image, audio, embedded resource), forwarded verbatim
    /// in the transport's own format for clients that can render it.
    Opaque {
        /// The block as the transport sent it.
        raw: RawJson,
    },
}

/// What kind of work a tool call does. Lets clients pick an icon and a
/// default rendering without knowing harness tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads files or data.
    Read,
    /// Modifies files.
    Edit,
    /// Deletes files.
    Delete,
    /// Moves or renames files.
    Move,
    /// Searches.
    Search,
    /// Runs a command.
    Execute,
    /// Internal reasoning step.
    Think,
    /// Fetches from the network.
    Fetch,
    /// Changes the session mode.
    SwitchMode,
    /// Anything else.
    Other,
}

/// Progress of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Announced, not started (possibly awaiting permission).
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error, or was rejected.
    Failed,
}

/// A file location a tool call touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ToolLocation {
    /// Path on the node.
    pub path: PathBuf,
    /// 1-based line, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Output attached to a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    /// Regular content.
    Content {
        /// The content.
        content: ContentBlock,
    },
    /// A file edit.
    Diff {
        /// File being changed.
        path: PathBuf,
        /// Previous text (`None` for a new file).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        /// New text.
        new_text: String,
    },
}

/// A tool call as first announced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ToolCall {
    /// Harness-assigned id.
    pub id: ToolCallId,
    /// Short human-readable description.
    pub title: String,
    /// Category.
    pub kind: ToolKind,
    /// Initial status.
    pub status: ToolCallStatus,
    /// Content produced so far.
    #[serde(default)]
    pub content: Vec<ToolCallContent>,
    /// Files involved.
    #[serde(default)]
    pub locations: Vec<ToolLocation>,
    /// The tool's raw input arguments, forwarded and never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<RawJson>,
}

/// A change to an announced tool call. `None` fields are unchanged; `Some`
/// collections replace the previous value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ToolCallUpdate {
    /// Which call.
    pub id: ToolCallId,
    /// New title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// New status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Replacement content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolCallContent>>,
    /// Replacement locations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolLocation>>,
    /// The tool's raw input arguments, when the harness sends them only with
    /// an update (Claude's adapter often does). Forwarded, never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<RawJson>,
    /// The tool's raw output, forwarded and never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<RawJson>,
}

/// Priority of a plan entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PlanPriority {
    /// High.
    High,
    /// Medium.
    Medium,
    /// Low.
    Low,
}

/// Progress of a plan entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// One step of the agent's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PlanEntry {
    /// What the step is.
    pub content: String,
    /// Importance.
    pub priority: PlanPriority,
    /// Progress.
    pub status: PlanStatus,
}

/// What choosing an option means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this once.
    AllowOnce,
    /// Allow this and remember.
    AllowAlways,
    /// Reject this once.
    RejectOnce,
    /// Reject this and remember.
    RejectAlways,
}

/// One choice offered by a permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PermissionOption {
    /// Harness-assigned option id, echoed back when chosen.
    pub id: PermissionOptionId,
    /// Label to show.
    pub name: String,
    /// Meaning.
    pub kind: PermissionOptionKind,
}

/// A request for human approval, pending until a client answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PermissionRequest {
    /// Node-assigned id clients answer with.
    pub id: PermissionId,
    /// The tool call needing approval.
    pub tool_call: ToolCallId,
    /// Choices, in the harness's order.
    pub options: Vec<PermissionOption>,
}

/// How a permission request was settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// A client chose an option.
    Selected {
        /// The chosen option.
        option: PermissionOptionId,
    },
    /// Withdrawn: the turn was cancelled or the session ended first.
    Cancelled,
}

/// Why a turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The agent finished.
    EndTurn,
    /// Token limit reached.
    MaxTokens,
    /// Per-turn request limit reached.
    MaxTurnRequests,
    /// The model refused.
    Refusal,
    /// A client cancelled the turn.
    Cancelled,
}
