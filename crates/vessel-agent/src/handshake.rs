//! The ACP handshake: `initialize`, then `session/new` or `session/load`,
//! then the model, all within [`AgentSetup::handshake_timeout`].
//!
//! [Decision Log]
//! - Purpose: an adapter that never answers `initialize`, `session/new` or
//!   `session/load` (hung, waiting for a login prompt nobody sees) left the
//!   session `Starting` forever, and every input sent to it waited, holding
//!   one of its connection's in-flight call slots on the node.
//! - Choice: one deadline for the whole handshake. When it passes, the
//!   handshake fails like any other (`Disconnect::Handshake`), the session
//!   stops the process and ends `Failed`, and waiting input is refused.
//! - Why the whole handshake rather than each request: the steps are
//!   sequential and none of them is useful alone; one budget is what a user
//!   waiting for "ready" experiences.
//! - Why 60 s by default: `session/load` replays the whole conversation
//!   before it answers, and a long one through a slow store can take tens of
//!   seconds; a healthy adapter starts in a few.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{Agent, Error};
use vessel_proto::{HarnessSessionId, ModelId, ResolvedMcpServers};

use crate::mcp;
use crate::model::{self, ModelError};

/// The default for [`AgentSetup::handshake_timeout`].
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// What the session should start with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSetup {
    /// Working directory of the ACP session (absolute).
    pub cwd: PathBuf,
    /// Model to select, if not the agent's default.
    pub model: Option<ModelId>,
    /// Resume this harness session (`session/load`) instead of starting a
    /// new one.
    pub resume: Option<HarnessSessionId>,
    /// MCP servers the session gets, in `session/new` or `session/load`.
    /// Values reach the agent over its stdin, never its command line; an
    /// HTTP server is left out (and logged) if the agent cannot take HTTP.
    pub mcp_servers: ResolvedMcpServers,
    /// Longest the whole handshake may take (normally
    /// [`DEFAULT_HANDSHAKE_TIMEOUT`]); past it the connection ends with
    /// `Disconnect::Handshake`.
    pub handshake_timeout: Duration,
}

/// Why the handshake failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HandshakeError {
    #[error("`initialize` failed")]
    Initialize(#[source] Error),
    #[error("the agent speaks ACP version {0:?}; Vessel speaks version 1")]
    Version(ProtocolVersion),
    #[error("the agent cannot resume sessions (it does not support `session/load`)")]
    CannotResume,
    #[error("`session/new` failed")]
    NewSession(#[source] Error),
    #[error("`session/load` failed")]
    LoadSession(#[source] Error),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("selecting the model failed")]
    SetModel(#[source] Error),
    #[error("the agent did not complete the ACP handshake within {} s", .0.as_secs())]
    TimedOut(Duration),
}

/// [`run`], bounded by `setup.handshake_timeout`.
pub(crate) async fn run_within(
    cx: &ConnectionTo<Agent>,
    setup: &AgentSetup,
) -> Result<acp::SessionId, HandshakeError> {
    let limit = setup.handshake_timeout;
    tokio::time::timeout(limit, run(cx, setup))
        .await
        .unwrap_or(Err(HandshakeError::TimedOut(limit)))
}

/// Run the handshake; returns the ACP session id.
///
/// Client capabilities are deliberately empty: no `fs` and no `terminal`,
/// so agents use their own tools (Vessel wraps harnesses; it does not
/// re-implement their file and shell access, ADR 0003).
pub(crate) async fn run(
    cx: &ConnectionTo<Agent>,
    setup: &AgentSetup,
) -> Result<acp::SessionId, HandshakeError> {
    let initialized = cx
        .send_request(
            acp::InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(acp::ClientCapabilities::new())
                .client_info(acp::Implementation::new(
                    "vessel",
                    env!("CARGO_PKG_VERSION"),
                )),
        )
        .block_task()
        .await
        .map_err(HandshakeError::Initialize)?;
    if initialized.protocol_version != ProtocolVersion::V1 {
        return Err(HandshakeError::Version(initialized.protocol_version));
    }

    let (servers, left_out) = mcp::to_acp(
        &setup.mcp_servers,
        &initialized.agent_capabilities.mcp_capabilities,
    );
    if !left_out.is_empty() {
        tracing::warn!(
            servers = ?left_out,
            "the agent does not take HTTP MCP servers; the session starts without them"
        );
    }
    let (session, options) = if let Some(resume) = &setup.resume {
        if !initialized.agent_capabilities.load_session {
            return Err(HandshakeError::CannotResume);
        }
        let session = acp::SessionId::new(resume.as_str());
        let loaded = cx
            .send_request(
                acp::LoadSessionRequest::new(session.clone(), &setup.cwd).mcp_servers(servers),
            )
            .block_task()
            .await
            .map_err(HandshakeError::LoadSession)?;
        (session, loaded.config_options)
    } else {
        let created = cx
            .send_request(acp::NewSessionRequest::new(&setup.cwd).mcp_servers(servers))
            .block_task()
            .await
            .map_err(HandshakeError::NewSession)?;
        (created.session_id, created.config_options)
    };

    if let Some(wanted) = &setup.model
        && let Some(choice) = model::choose(options.as_deref().unwrap_or_default(), wanted)?
    {
        drop(
            cx.send_request(acp::SetSessionConfigOptionRequest::new(
                session.clone(),
                choice.config,
                choice.value,
            ))
            .block_task()
            .await
            .map_err(HandshakeError::SetModel)?,
        );
    }
    Ok(session)
}
