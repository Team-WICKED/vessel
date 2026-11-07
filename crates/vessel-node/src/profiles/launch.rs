//! What a new session is launched with: its resolved profile, its MCP
//! servers and where their file goes, and who created it. The seam between
//! `session.create` (and `session.move_profile`) and the session runtime
//! (`vessel_session::LaunchOptions`).
//!
//! [Decision Log: where MCP servers come from] (H3, node side)
//! - Purpose: every session starts with the fleet's MCP servers, their
//!   secrets filled in from this node (ADR 0012), and agent sessions also
//!   with the `vessel` server (ADR 0013). Everything after this point
//!   (ACP for structured sessions, the owner-only file or the environment
//!   for terminal ones) already carries resolved servers.
//! - Constraint: fleet sync (Track F) owns the definitions and the secrets
//!   and builds them after this module; the node must not depend on how
//!   they are stored.
//! - Alternatives: a closure (cannot be `Debug`, which the service and
//!   its parts derive); passing resolved servers in `Config` (fixed at
//!   start, while the fleet's definitions change with each pull).
//! - Choice: [`McpSource`], a small trait the launcher asks once per launch
//!   and per move. Fleet sync implements it over its definitions and
//!   secrets (`vessel_fleet::resolve`, `fleet::FleetMcp`), which is what
//!   the node is put together with (`node.rs`). Secrets it lacks are reported
//!   back; the session starts without the servers that need them, and says
//!   so in its log.
//! - Cost: the source is called on the async runtime, so it must not
//!   block: it answers from memory.
//!
//! [Decision Log: the `vessel` server] (H4, ADR 0013)
//! - Every session's agent gets one more stdio server, `vessel`: the
//!   node's `vessel mcp` command (`Config::vessel_mcp`), with the node's
//!   local endpoint, the session's id and its credential in that server's
//!   own environment. So the secret reaches only `vessel mcp`: through
//!   ACP for a structured session, the owner-only MCP file for Claude
//!   Code's terminal, and Codex's environment for a Codex terminal, where
//!   its name (`..._SECRET`) keeps Codex from passing it to the commands
//!   the agent runs (ADR 0012, as built). It replaces a fleet server of the
//!   same name.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use vessel_proto::{
    AgentSecret, Creator, McpServerName, McpTransport, NodeId, OriginBy, RemoteCreator,
    ResolvedMcpServers, ResolvedValue, SecretName, SessionId, SessionOrigin, SessionSpec,
};
use vessel_session::LaunchOptions;

use super::NodeProfiles;
use crate::error::ServiceError;
use crate::principal::Principal;

/// The MCP servers a session gets from one source, resolved.
#[derive(Debug, Default)]
pub(crate) struct McpServers {
    /// The servers, every value filled in.
    pub(crate) servers: ResolvedMcpServers,
    /// Secrets this node lacks, each once; the servers that need them are
    /// left out.
    pub(crate) missing: Vec<SecretName>,
}

/// Where sessions' MCP servers come from (see the module's Decision Log).
pub(crate) trait McpSource: fmt::Debug + Send + Sync {
    /// The servers a session of `spec` starts with. Called on the async
    /// runtime: must answer from memory, without blocking.
    fn servers(&self, spec: &SessionSpec) -> McpServers;
}

/// No MCP servers, for tests of the launcher alone (the node launches with
/// fleet sync's source).
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoMcpServers;

#[cfg(test)]
impl McpSource for NoMcpServers {
    fn servers(&self, _spec: &SessionSpec) -> McpServers {
        McpServers::default()
    }
}

/// The `vessel` server's variables. `vessel-mcp` reads the same names;
/// its end-to-end test fails if they drift apart.
const SOCKET_VARIABLE: &str = "VESSEL_NODE_SOCKET";
const SESSION_VARIABLE: &str = "VESSEL_AGENT_SESSION";
const SECRET_VARIABLE: &str = "VESSEL_AGENT_SECRET";

/// The `vessel` server's name in every session.
const VESSEL_SERVER: &str = "vessel";

/// How sessions' agents reach this node: `vessel mcp` (ADR 0013).
#[derive(Debug, Clone)]
pub(crate) struct AgentTools {
    /// `vessel` (or what runs `vessel mcp`).
    pub(crate) program: PathBuf,
    /// `["mcp"]`.
    pub(crate) args: Vec<String>,
    /// The node's local socket or pipe name.
    pub(crate) socket: PathBuf,
}

/// Everything `session.create` adds to the spec.
#[derive(Debug)]
pub(crate) struct Launcher {
    /// Owner-only, under the data directory (`DataDir::prepare_mcp_dir`).
    mcp_dir: PathBuf,
    /// The fleet's MCP servers.
    mcp: Arc<dyn McpSource>,
    /// The `vessel` server, when the node knows how to run it.
    tools: Option<AgentTools>,
}

impl Launcher {
    /// A launcher writing MCP files in `mcp_dir`, with `mcp`'s servers,
    /// and `tools` as every session's `vessel` server.
    pub(crate) fn new(
        mcp_dir: PathBuf,
        mcp: Arc<dyn McpSource>,
        tools: Option<AgentTools>,
    ) -> Self {
        Self {
            mcp_dir,
            mcp,
            tools,
        }
    }

    /// Whether sessions get the `vessel` server (and so a credential).
    pub(crate) const fn has_tools(&self) -> bool {
        self.tools.is_some()
    }

    /// Add the `vessel` server for session `session`, holding `secret`.
    pub(crate) fn attach_tools(
        &self,
        servers: &mut ResolvedMcpServers,
        session: SessionId,
        secret: &AgentSecret,
    ) -> Result<(), ServiceError> {
        let Some(tools) = &self.tools else {
            return Ok(());
        };
        let name = |text: &str| text.parse().map_err(ServiceError::AgentTools);
        let socket = tools
            .socket
            .to_str()
            .ok_or(ServiceError::AgentToolsPath)?
            .to_owned();
        let env = BTreeMap::from([
            (name(SOCKET_VARIABLE)?, ResolvedValue::new(socket)),
            (
                name(SESSION_VARIABLE)?,
                ResolvedValue::new(session.to_string()),
            ),
            (
                name(SECRET_VARIABLE)?,
                ResolvedValue::new(secret.to_string()),
            ),
        ]);
        let server: McpServerName = VESSEL_SERVER.parse().map_err(ServiceError::AgentTools)?;
        let replaced = servers.insert(
            server,
            McpTransport::Stdio {
                command: tools.program.clone(),
                args: tools.args.clone(),
                env,
            },
        );
        if replaced.is_some() {
            tracing::warn!("a fleet MCP server named `vessel` is replaced by the node's own");
        }
        Ok(())
    }

    /// The options for `spec`, created by `principal`.
    ///
    /// # Errors
    /// The spec names a profile this node does not have, or one of another
    /// harness.
    pub(crate) fn options(
        &self,
        profiles: &NodeProfiles,
        spec: &SessionSpec,
        principal: &Principal,
    ) -> Result<LaunchOptions, ServiceError> {
        let profile = match &spec.profile {
            None => None,
            Some(name) => {
                let profile = profiles
                    .get(name)
                    .ok_or_else(|| ServiceError::UnknownProfile(name.clone()))?;
                if profile.harness() != spec.harness {
                    return Err(ServiceError::ProfileHarness {
                        profile: name.clone(),
                        profile_harness: profile.harness(),
                        harness: spec.harness,
                    });
                }
                Some(profile)
            }
        };
        let (mcp_servers, notices) = self.mcp_servers(spec);
        Ok(LaunchOptions {
            profile,
            mcp_servers,
            mcp_dir: Some(self.mcp_dir.clone()),
            created_by: creator(principal),
            notices,
            id: None,
            // An import (F4) sets its own origin.
            origin: origin(principal),
            via: match principal {
                Principal::Delegated { via, .. } => Some(*via),
                Principal::Owner | Principal::Device { .. } | Principal::Agent { .. } => None,
            },
            // A routed profile's are the node's to fill in, once the
            // session's id is known (`Models::launch`).
            model: None,
            model_proxy: None,
        })
    }

    /// The servers a session of `spec` starts with, and the lines its log
    /// gets about servers left out.
    pub(crate) fn mcp_servers(&self, spec: &SessionSpec) -> (ResolvedMcpServers, Vec<String>) {
        let McpServers { servers, missing } = self.mcp.servers(spec);
        if !missing.is_empty() {
            tracing::warn!(
                ?missing,
                "secrets missing on this node; the MCP servers that need them are left out"
            );
        }
        (servers, missing.iter().map(missing_secret).collect())
    }
}

/// The log line for a missing secret: its name, never a value.
fn missing_secret(name: &SecretName) -> String {
    format!(
        "vessel: secret {name} is not set on this node; MCP servers that need it were left out \
         (vessel fleet secret set {name})"
    )
}

/// Who a caller is, as a session records it (ADR 0013). A delivered call
/// (protocol 6) is recorded as the origin's agent, a variant protocol 5
/// readers know, or else as the delivering device, which is true here;
/// [`origin`] keeps the rest.
fn creator(principal: &Principal) -> Creator {
    match principal {
        Principal::Owner => Creator::Owner,
        Principal::Device { id, .. } => Creator::Device { id: *id },
        Principal::Agent { node, session, .. } => Creator::Agent {
            node: *node,
            session: *session,
        },
        Principal::Delegated { device, origin, .. } => match origin.by {
            OriginBy::Agent { session, .. } => Creator::Agent {
                node: origin.node,
                session,
            },
            OriginBy::Owner | OriginBy::Device { .. } => Creator::Device { id: *device },
        },
    }
}

/// Where a session a caller creates comes from: another node, for a
/// delivered call of its owner or one of its devices (an agent's is in
/// `created_by` already).
fn origin(principal: &Principal) -> Option<SessionOrigin> {
    let Principal::Delegated { origin, .. } = principal else {
        return None;
    };
    let by = match origin.by {
        OriginBy::Owner => RemoteCreator::Owner,
        OriginBy::Device { id } => RemoteCreator::Device { id },
        OriginBy::Agent { .. } => return None,
    };
    Some(SessionOrigin::Remote {
        node: origin.node,
        by,
    })
}

/// Whether `creator` is an agent session of `node`: the session, if so.
pub(crate) fn agent_session(creator: &Creator, node: NodeId) -> Option<SessionId> {
    match creator {
        Creator::Agent {
            node: agent_node,
            session,
        } if *agent_node == node => Some(*session),
        Creator::Agent { .. } | Creator::Owner | Creator::Device { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use vessel_proto::{HarnessKind, McpTransport, ResolvedValue, SessionMode, TerminalSize};

    use super::*;

    /// One server, and one secret it could not find.
    #[derive(Debug)]
    struct OneServer;

    impl McpSource for OneServer {
        fn servers(&self, spec: &SessionSpec) -> McpServers {
            let mut servers = ResolvedMcpServers::new();
            let _none = servers.insert(
                "docs".parse().unwrap(),
                McpTransport::Stdio {
                    command: PathBuf::from("docs-mcp"),
                    args: vec![spec.harness.to_string()],
                    env: BTreeMap::from([(
                        "DOCS_TOKEN".parse().unwrap(),
                        ResolvedValue::new("value-never-logged".to_owned()),
                    )]),
                },
            );
            McpServers {
                servers,
                missing: vec!["WEB_TOKEN".parse().unwrap()],
            }
        }
    }

    #[test]
    fn a_launch_gets_the_sources_servers_and_a_line_per_missing_secret() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = NodeProfiles::load(
            vessel_store::DataDir::new(dir.path()),
            vessel_harness::HarnessEnv::new(dir.path().to_owned()),
        )
        .unwrap();
        let launcher = Launcher::new(dir.path().join("mcp"), Arc::new(OneServer), None);
        let spec = SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        );
        let options = launcher
            .options(&profiles, &spec, &Principal::Owner)
            .unwrap();
        let names: Vec<String> = options
            .mcp_servers
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(names, ["docs"]);
        assert_eq!(options.notices.len(), 1);
        let notice = options.notices.first().unwrap();
        assert!(notice.starts_with("vessel: secret WEB_TOKEN "), "{notice}");
        assert!(!notice.contains("value-never-logged"), "{notice}");

        let none = Launcher::new(dir.path().join("mcp"), Arc::new(NoMcpServers), None)
            .options(&profiles, &spec, &Principal::Owner)
            .unwrap();
        assert!(none.mcp_servers.is_empty() && none.notices.is_empty());
    }
}
