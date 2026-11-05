//! Pure conversions between ACP messages and Vessel's agent timeline.
//!
//! No I/O and no state: every function takes an ACP value and returns a
//! Vessel value (or the reverse for prompts and permission answers), so the
//! whole mapping is unit-tested against fixtures in `tests/fixtures/acp/`.
//! This is the only module that reads ACP payload fields.
//!
//! ACP enums are `#[non_exhaustive]`. Every match lists all known variants
//! and handles future ones explicitly: unknown updates are skipped (and
//! logged by the caller), unknown content is forwarded as
//! [`ContentBlock::Opaque`], and unknown kinds fall back to the most
//! cautious known value.

use agent_client_protocol::v1::schema as acp;
use serde::Serialize;
use vessel_proto::{
    AgentEvent, ContentBlock, PermissionOption, PermissionOptionId, PermissionOptionKind,
    PermissionOutcome, PlanEntry, PlanPriority, PlanStatus, RawJson, StopReason, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolKind, ToolLocation,
};

/// What a `session/update` becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Update {
    /// A timeline event.
    Event(AgentEvent),
    /// Not part of Vessel's timeline (yet); logged and dropped.
    Skipped(Skipped),
}

/// Kinds of `session/update` Vessel does not put on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Skipped {
    /// The user's own message, replayed by `session/load`. The timeline
    /// has no user-message chunk event; Vessel records prompts as
    /// `TurnStarted` when it sends them.
    UserMessage,
    /// Slash commands the agent offers.
    AvailableCommands,
    /// The agent's mode changed.
    CurrentMode,
    /// A session config option changed.
    ConfigOption,
    /// Session metadata (title) changed.
    SessionInfo,
    /// Token or cost usage.
    Usage,
    /// Context compaction progress.
    Compaction,
    /// An update kind newer than the ACP version Vessel pins.
    Unknown,
}

/// Map one `session/update`.
pub(crate) fn session_update(update: acp::SessionUpdate) -> Update {
    use acp::SessionUpdate as U;
    let event = match update {
        U::AgentMessageChunk(chunk) => AgentEvent::MessageChunk {
            content: content(chunk.content),
        },
        U::AgentThoughtChunk(chunk) => AgentEvent::ThoughtChunk {
            content: content(chunk.content),
        },
        U::ToolCall(call) => AgentEvent::ToolCall(tool_call(call)),
        U::ToolCallUpdate(update) => AgentEvent::ToolCallUpdate(tool_call_update(update)),
        U::Plan(plan) => AgentEvent::Plan {
            entries: plan.entries.into_iter().map(plan_entry).collect(),
        },
        // Notices are for people (rate limits, warnings); the harness log is
        // where clients already show such lines.
        U::Notice(notice) => AgentEvent::HarnessLog {
            line: notice_line(&notice),
        },
        U::UserMessageChunk(_) => return Update::Skipped(Skipped::UserMessage),
        U::AvailableCommandsUpdate(_) => return Update::Skipped(Skipped::AvailableCommands),
        U::CurrentModeUpdate(_) => return Update::Skipped(Skipped::CurrentMode),
        U::ConfigOptionUpdate(_) => return Update::Skipped(Skipped::ConfigOption),
        U::SessionInfoUpdate(_) => return Update::Skipped(Skipped::SessionInfo),
        U::UsageUpdate(_) => return Update::Skipped(Skipped::Usage),
        U::CompactionUpdate(_) | U::CompactionSummaryChunk(_) => {
            return Update::Skipped(Skipped::Compaction);
        }
        _ => return Update::Skipped(Skipped::Unknown),
    };
    Update::Event(event)
}

/// Map a content block. Text and resource links have Vessel equivalents;
/// everything else (images, audio, embedded resources, future kinds) is
/// forwarded verbatim for clients that can render it.
pub(crate) fn content(block: acp::ContentBlock) -> ContentBlock {
    match block {
        acp::ContentBlock::Text(text) => ContentBlock::Text { text: text.text },
        acp::ContentBlock::ResourceLink(link) => ContentBlock::ResourceLink {
            uri: link.uri,
            name: link.name,
        },
        other @ (acp::ContentBlock::Image(_)
        | acp::ContentBlock::Audio(_)
        | acp::ContentBlock::Resource(_)
        | _) => opaque(&other),
    }
}

/// A prompt block in ACP form. Opaque blocks must be ACP content already
/// (they are what [`content`] forwarded), so they are parsed back.
pub(crate) fn prompt_block(block: ContentBlock) -> Result<acp::ContentBlock, serde_json::Error> {
    match block {
        ContentBlock::Text { text } => Ok(acp::ContentBlock::Text(acp::TextContent::new(text))),
        ContentBlock::ResourceLink { uri, name } => Ok(acp::ContentBlock::ResourceLink(
            acp::ResourceLink::new(name, uri),
        )),
        ContentBlock::Opaque { raw } => serde_json::from_str(raw.as_text()),
    }
}

/// Map an announced tool call.
pub(crate) fn tool_call(call: acp::ToolCall) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(call.tool_call_id.0),
        title: call.title,
        kind: tool_kind(call.kind),
        status: tool_status(call.status),
        content: call.content.into_iter().map(tool_content).collect(),
        locations: call.locations.into_iter().map(location).collect(),
        raw_input: call.raw_input.as_ref().and_then(raw_json),
    }
}

/// Map a tool call update.
///
/// ACP updates may also carry `raw_input`; Vessel's `ToolCallUpdate` has no
/// such field, so it is dropped (see the crate docs).
pub(crate) fn tool_call_update(update: acp::ToolCallUpdate) -> ToolCallUpdate {
    let fields = update.fields;
    ToolCallUpdate {
        id: ToolCallId::new(update.tool_call_id.0),
        title: fields.title,
        kind: fields.kind.map(tool_kind),
        status: fields.status.map(tool_status),
        content: fields
            .content
            .map(|content| content.into_iter().map(tool_content).collect()),
        locations: fields
            .locations
            .map(|locations| locations.into_iter().map(location).collect()),
        raw_output: fields.raw_output.as_ref().and_then(raw_json),
    }
}

/// The tool call a permission request is about, as a timeline event.
///
/// ACP lets a permission request describe a tool call the agent never
/// announced, so clients would have nothing to show. An unknown call is
/// announced here; for a known one, any fields the request carries become
/// an update. `None` when there is nothing new to say.
pub(crate) fn permission_tool_call(update: acp::ToolCallUpdate, known: bool) -> Option<AgentEvent> {
    if known {
        let update = tool_call_update(update);
        let changed = update.title.is_some()
            || update.kind.is_some()
            || update.status.is_some()
            || update.content.is_some()
            || update.locations.is_some()
            || update.raw_output.is_some();
        return changed.then_some(AgentEvent::ToolCallUpdate(update));
    }
    let fields = update.fields;
    Some(AgentEvent::ToolCall(ToolCall {
        id: ToolCallId::new(update.tool_call_id.0),
        title: fields.title.unwrap_or_default(),
        kind: fields.kind.map_or(ToolKind::Other, tool_kind),
        status: fields.status.map_or(ToolCallStatus::Pending, tool_status),
        content: fields
            .content
            .unwrap_or_default()
            .into_iter()
            .map(tool_content)
            .collect(),
        locations: fields
            .locations
            .unwrap_or_default()
            .into_iter()
            .map(location)
            .collect(),
        raw_input: fields.raw_input.as_ref().and_then(raw_json),
    }))
}

/// Map the options of a permission request.
pub(crate) fn permission_options(options: Vec<acp::PermissionOption>) -> Vec<PermissionOption> {
    options
        .into_iter()
        .map(|option| PermissionOption {
            id: PermissionOptionId::new(option.option_id.0),
            name: option.name,
            kind: permission_kind(option.kind),
        })
        .collect()
}

/// A Vessel permission answer in ACP form.
pub(crate) fn permission_outcome(outcome: &PermissionOutcome) -> acp::RequestPermissionOutcome {
    match outcome {
        PermissionOutcome::Selected { option } => acp::RequestPermissionOutcome::Selected(
            acp::SelectedPermissionOutcome::new(option.as_str().to_owned()),
        ),
        PermissionOutcome::Cancelled => acp::RequestPermissionOutcome::Cancelled,
    }
}

/// Map why a turn stopped. A future reason is reported as a normal end:
/// the turn did end, and nothing more specific is known.
pub(crate) fn stop_reason(reason: acp::StopReason) -> StopReason {
    match reason {
        acp::StopReason::MaxTokens => StopReason::MaxTokens,
        acp::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        acp::StopReason::Refusal => StopReason::Refusal,
        acp::StopReason::Cancelled => StopReason::Cancelled,
        acp::StopReason::EndTurn | _ => StopReason::EndTurn,
    }
}

fn tool_kind(kind: acp::ToolKind) -> ToolKind {
    match kind {
        acp::ToolKind::Read => ToolKind::Read,
        acp::ToolKind::Edit => ToolKind::Edit,
        acp::ToolKind::Delete => ToolKind::Delete,
        acp::ToolKind::Move => ToolKind::Move,
        acp::ToolKind::Search => ToolKind::Search,
        acp::ToolKind::Execute => ToolKind::Execute,
        acp::ToolKind::Think => ToolKind::Think,
        acp::ToolKind::Fetch => ToolKind::Fetch,
        acp::ToolKind::SwitchMode => ToolKind::SwitchMode,
        acp::ToolKind::Other | _ => ToolKind::Other,
    }
}

/// A future status is reported as still running: it is neither known to
/// have finished nor to have failed.
fn tool_status(status: acp::ToolCallStatus) -> ToolCallStatus {
    match status {
        acp::ToolCallStatus::Pending => ToolCallStatus::Pending,
        acp::ToolCallStatus::Completed => ToolCallStatus::Completed,
        acp::ToolCallStatus::Failed => ToolCallStatus::Failed,
        acp::ToolCallStatus::InProgress | _ => ToolCallStatus::InProgress,
    }
}

/// Terminal content is forwarded verbatim: Vessel declares no terminal
/// capability, so agents should not send it, but nothing is lost if one does.
fn tool_content(content: acp::ToolCallContent) -> ToolCallContent {
    match content {
        acp::ToolCallContent::Content(inner) => ToolCallContent::Content {
            content: self::content(inner.content),
        },
        acp::ToolCallContent::Diff(diff) => ToolCallContent::Diff {
            path: diff.path,
            old_text: diff.old_text,
            new_text: diff.new_text,
        },
        other @ (acp::ToolCallContent::Terminal(_) | _) => ToolCallContent::Content {
            content: opaque(&other),
        },
    }
}

fn location(location: acp::ToolCallLocation) -> ToolLocation {
    ToolLocation {
        path: location.path,
        line: location.line,
    }
}

fn plan_entry(entry: acp::PlanEntry) -> PlanEntry {
    PlanEntry {
        content: entry.content,
        priority: match entry.priority {
            acp::PlanEntryPriority::High => PlanPriority::High,
            acp::PlanEntryPriority::Low => PlanPriority::Low,
            acp::PlanEntryPriority::Medium | _ => PlanPriority::Medium,
        },
        status: match entry.status {
            acp::PlanEntryStatus::InProgress => PlanStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanStatus::Completed,
            acp::PlanEntryStatus::Pending | _ => PlanStatus::Pending,
        },
    }
}

/// A future option kind is treated as a rejection: a client must never
/// show an unknown choice as "allow".
fn permission_kind(kind: acp::PermissionOptionKind) -> PermissionOptionKind {
    match kind {
        acp::PermissionOptionKind::AllowOnce => PermissionOptionKind::AllowOnce,
        acp::PermissionOptionKind::AllowAlways => PermissionOptionKind::AllowAlways,
        acp::PermissionOptionKind::RejectAlways => PermissionOptionKind::RejectAlways,
        acp::PermissionOptionKind::RejectOnce | _ => PermissionOptionKind::RejectOnce,
    }
}

fn notice_line(notice: &acp::Notice) -> String {
    let severity = match &notice.severity {
        acp::NoticeSeverity::Info => "info",
        acp::NoticeSeverity::Warning => "warning",
        acp::NoticeSeverity::Error => "error",
        acp::NoticeSeverity::Other(other) => other.as_str(),
        _ => "notice",
    };
    match &notice.description {
        Some(description) => format!("{severity}: {}: {description}", notice.title),
        None => format!("{severity}: {}", notice.title),
    }
}

/// Forward an ACP value Vessel does not model, as ACP JSON.
fn opaque<T: Serialize>(value: &T) -> ContentBlock {
    match RawJson::from_serialize(value) {
        Ok(raw) => ContentBlock::Opaque { raw },
        // Derived `Serialize` on ACP types cannot fail for JSON; keep the
        // timeline intact if it ever does.
        Err(error) => {
            tracing::warn!(%error, "cannot forward ACP content");
            ContentBlock::Text {
                text: String::from("[content Vessel could not forward]"),
            }
        }
    }
}

/// An opaque ACP JSON value as [`RawJson`]. The `serde_json::Value` exists
/// only here, at the edge, and is never inspected.
fn raw_json(value: &serde_json::Value) -> Option<RawJson> {
    RawJson::from_serialize(value)
        .inspect_err(|error| tracing::warn!(%error, "cannot forward raw tool data"))
        .ok()
}

#[cfg(test)]
mod tests;
