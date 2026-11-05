//! Wire contract between Vessel nodes and their clients.
//!
//! Everything a node, the CLI, a web UI or an editor plugin needs to agree
//! on: identifiers, harnesses, sessions, the session event log, the
//! structured agent timeline, errors, and the typed RPC frames.
//!
//! Rules this crate keeps (see `AGENTS.md`):
//! - no I/O and no async runtime, so any client can depend on it;
//! - no untyped JSON: open-ended payloads are [`RawJson`], never a `Value`;
//! - it is the only place wire types are defined.

mod agent;
mod error;
mod event;
mod harness;
mod id;
mod node;
mod raw;
mod rpc;
mod session;
mod terminal;

pub use agent::{
    AgentEvent, ContentBlock, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PermissionRequest, PlanEntry, PlanPriority, PlanStatus, StopReason, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind, ToolLocation,
};
pub use error::{ErrorCode, WireError};
pub use event::{EventRecord, SessionEvent};
pub use harness::{Harness, HarnessKind, StructuredSupport, UnknownHarness};
pub use id::{
    EventSeq, HarnessSessionId, ModelId, NodeId, PermissionId, PermissionOptionId, RequestId,
    SessionId, SubscriptionId, ToolCallId,
};
pub use node::{MIN_PROTOCOL_VERSION, NodeInfo, PROTOCOL_VERSION, ProtocolRange, ProtocolVersion};
pub use raw::RawJson;
pub use rpc::{
    Ack, Call, ClientFrame, ClientInfo, CreateSession, EventPage, GetNode, GetSession, Handler,
    Hello, KillSession, ListHarnesses, ListSessions, Method, Outcome, ReadEvents, Reply, Request,
    SendInput, ServerFrame, Subscribe, Subscribed, SubscriptionEnd, UnexpectedReply, Unsubscribe,
    Welcome, dispatch,
};
pub use session::{SessionInfo, SessionInput, SessionMode, SessionSpec, SessionStatus};
pub use terminal::{TerminalBytes, TerminalInput, TerminalOutput, TerminalSize};

/// Default TCP port a node listens on.
pub const DEFAULT_PORT: u16 = 7077;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn session_spec_flattens_mode() {
        let spec = SessionSpec {
            harness: HarnessKind::ClaudeCode,
            cwd: Some(PathBuf::from("/work")),
            mode: SessionMode::Structured {
                model: Some(ModelId::new("opus")),
                resume: None,
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(
            json,
            r#"{"harness":"claude-code","cwd":"/work","mode":"structured","model":"opus"}"#
        );
        assert_eq!(serde_json::from_str::<SessionSpec>(&json).unwrap(), spec);
    }

    #[test]
    fn terminal_mode_defaults_apply() {
        let spec: SessionSpec =
            serde_json::from_str(r#"{"harness":"codex","mode":"terminal"}"#).unwrap();
        assert_eq!(
            spec.mode,
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            }
        );
    }

    #[test]
    fn event_records_round_trip() {
        let record = EventRecord {
            seq: EventSeq::new(3),
            at: chrono::DateTime::UNIX_EPOCH,
            event: SessionEvent::Agent {
                event: AgentEvent::ToolCall(ToolCall {
                    id: ToolCallId::new("call_1"),
                    title: "Run tests".to_owned(),
                    kind: ToolKind::Execute,
                    status: ToolCallStatus::Pending,
                    content: Vec::new(),
                    locations: Vec::new(),
                    raw_input: Some(RawJson::from_text(r#"{"cmd":"cargo test"}"#).unwrap()),
                }),
            },
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<EventRecord>(&json).unwrap(), record);
    }
}
