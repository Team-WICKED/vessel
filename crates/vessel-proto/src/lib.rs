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
mod base64_bytes;
mod device;
mod error;
mod event;
mod fleet;
mod forward;
mod harness;
mod id;
mod key;
pub mod lsp;
mod model;
mod name;
mod node;
mod pairing;
mod profile;
mod proxy;
mod raw;
mod relay;
mod rpc;
mod secret;
mod session;
mod terminal;
mod transfer;

pub use agent::{
    AgentEvent, ContentBlock, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PermissionRequest, PlanEntry, PlanPriority, PlanStatus, StopReason, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind, ToolLocation,
};
pub use device::{
    DeviceInfo, DeviceSecret, Invitation, InvitationSecret, InviteDevice, ListDevices, Preamble,
    PreambleReply, RefusalReason, RevokeDevice, Scope,
};
pub use error::{ErrorCode, WireError};
pub use event::{EventRecord, SessionEvent};
pub use fleet::{
    BlobChunk, BlobHash, BundleFile, BundlePath, BundlePathRule, ConfigValue, FleetMember,
    Generation, GetManifest, HttpUrl, InvalidBundlePath, InvalidHttpUrl, ListMembers, Manifest,
    McpDefinitions, McpServers, McpTransport, ReadBlob, ReportApplied, ResolvedMcpServers,
    ResolvedValue, WatchFleet,
};
pub use forward::{
    Callback, Deliver, FleetNode, ForgetNode, Forward, Forwarded, GetCallback, GrantHome,
    ListNodes, ListPeers, NodeForgotten, Origin, OriginBy, RelayCall,
};
pub use harness::{Harness, HarnessKind, StructuredSupport, UnknownHarness};
pub use id::{
    DeviceId, EventSeq, HarnessSessionId, ModelId, NodeId, PermissionId, PermissionOptionId,
    RequestId, SessionId, SubscriptionId, ToolCallId, TransferId,
};
pub use key::{KeyTextError, NodePublicKey, RoutingId};
pub use model::{
    DurationMs, ListModels, ModelApi, ModelCall, ModelFailure, ModelRoute, ModelStop, SessionModel,
    TokenCount, TokenUsage,
};
pub use name::{
    EnvName, HeaderName, InvalidName, McpServerName, ModelRouteName, NameKind, ProfileName,
    ProviderName, SecretName,
};
pub use node::{
    MIN_PROTOCOL_VERSION, NodeInfo, NodeState, PROTOCOL_VERSION, ProtocolRange, ProtocolVersion,
};
pub use pairing::{InvalidRoute, LinkError, OfferVersion, PairingLink, PairingOffer, RouteUrl};
pub use profile::{
    AgentAuth, AgentSecret, Creator, ListProfiles, MoveProfile, ProfileInfo, ReloadProfiles,
};
pub use proxy::ProxyExit;
pub use raw::RawJson;
pub use relay::{
    ConnId, EmptyToken, HandshakeMessage, NodeClose, NodeToRelay, RegistrationRefusal, RelayClose,
    RelayTicket, RelayToNode, RelayToken, TicketMac,
};
pub use rpc::{
    Ack, Call, ClientFrame, ClientInfo, CreateSession, EventPage, GetNode, GetSession, Handler,
    Hello, KillSession, ListHarnesses, ListSessions, Method, Outcome, ReadEvents, Reply, Request,
    SendInput, ServerFrame, StopNode, StopWhen, Subscribe, Subscribed, SubscriptionEnd,
    UnexpectedReply, Unsubscribe, Welcome, dispatch,
};
pub use secret::{CopySecret, DeleteSecret, ListSecrets, SecretValue, SetSecret};
pub use session::{
    RemoteCreator, SessionInfo, SessionInput, SessionMode, SessionOrigin, SessionSpec,
    SessionStatus,
};
pub use terminal::{TerminalBytes, TerminalInput, TerminalOutput, TerminalSize};
pub use transfer::{
    CloseTransfer, ExportMemory, ExportSession, ImportMemory, ImportSession, MAX_TRANSFER_FILE,
    MAX_TRANSFER_TOTAL, MemoryImported, MemoryRef, ReadTransfer, SessionImported, SessionRef,
    TransferDetail, TransferManifest,
};

/// Default TCP port a node listens on.
pub const DEFAULT_PORT: u16 = 7077;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn session_spec_flattens_mode() {
        let spec = SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Structured {
                model: Some(ModelId::new("opus")),
                resume: None,
            },
        )
        .with_cwd(PathBuf::from("/work"));
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
