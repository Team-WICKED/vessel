//! The ACP handshake: `initialize`, then `session/new` or `session/load`,
//! then the model.

use std::path::PathBuf;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{Agent, Error};
use vessel_proto::{HarnessSessionId, ModelId};

use crate::model::{self, ModelError};

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

    let (session, options) = if let Some(resume) = &setup.resume {
        if !initialized.agent_capabilities.load_session {
            return Err(HandshakeError::CannotResume);
        }
        let session = acp::SessionId::new(resume.as_str());
        let loaded = cx
            .send_request(acp::LoadSessionRequest::new(session.clone(), &setup.cwd))
            .block_task()
            .await
            .map_err(HandshakeError::LoadSession)?;
        (session, loaded.config_options)
    } else {
        let created = cx
            .send_request(acp::NewSessionRequest::new(&setup.cwd))
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
