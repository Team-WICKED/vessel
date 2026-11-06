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
//!   secrets (`vessel_fleet::resolve`) and replaces [`NoMcpServers`] where
//!   the node is put together (`node.rs`). Secrets it lacks are reported
//!   back; the session starts without the servers that need them, and says
//!   so in its log.
//! - Cost: the source is called on the async runtime, so it must not
//!   block: it answers from memory.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use vessel_proto::{Creator, ResolvedMcpServers, SecretName, SessionSpec};
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

/// No MCP servers: the node until fleet sync plugs in its source.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoMcpServers;

impl McpSource for NoMcpServers {
    fn servers(&self, _spec: &SessionSpec) -> McpServers {
        McpServers::default()
    }
}

/// Everything `session.create` adds to the spec.
#[derive(Debug)]
pub(crate) struct Launcher {
    /// Owner-only, under the data directory (`DataDir::prepare_mcp_dir`).
    mcp_dir: PathBuf,
    /// The fleet's MCP servers.
    mcp: Arc<dyn McpSource>,
}

impl Launcher {
    /// A launcher writing MCP files in `mcp_dir`, with `mcp`'s servers.
    pub(crate) fn new(mcp_dir: PathBuf, mcp: Arc<dyn McpSource>) -> Self {
        Self { mcp_dir, mcp }
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

/// Who a caller is, as a session records it (ADR 0013). Agents (H4) get
/// their own principal later.
fn creator(principal: &Principal) -> Creator {
    match principal {
        Principal::Owner => Creator::Owner,
        Principal::Device { id, .. } => Creator::Device { id: *id },
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
            Some(dir.path().to_owned()),
        )
        .unwrap();
        let launcher = Launcher::new(dir.path().join("mcp"), Arc::new(OneServer));
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

        let none = Launcher::new(dir.path().join("mcp"), Arc::new(NoMcpServers))
            .options(&profiles, &spec, &Principal::Owner)
            .unwrap();
        assert!(none.mcp_servers.is_empty() && none.notices.is_empty());
    }
}
