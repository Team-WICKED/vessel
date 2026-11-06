//! What a new session is launched with: its resolved profile, its MCP
//! servers and where their file goes, and who created it. The seam between
//! `session.create` and the session runtime (`vessel_session::LaunchOptions`).

use std::path::PathBuf;

use vessel_proto::{Creator, ResolvedMcpServers, SessionSpec};
use vessel_session::LaunchOptions;

use super::NodeProfiles;
use crate::error::ServiceError;
use crate::principal::Principal;

/// Everything `session.create` adds to the spec.
#[derive(Debug)]
pub(crate) struct Launcher {
    /// Owner-only, under the data directory (`DataDir::prepare_mcp_dir`).
    pub(crate) mcp_dir: PathBuf,
}

impl Launcher {
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
        Ok(LaunchOptions {
            profile,
            mcp_servers: mcp_servers(spec),
            mcp_dir: Some(self.mcp_dir.clone()),
            created_by: creator(principal),
        })
    }
}

/// The MCP servers a new session gets: none yet. Fleet sync (Track F, ADR
/// 0012) resolves its definitions and the node's secrets here; everything
/// after this point (ACP for structured sessions, the owner-only file or
/// environment for terminal ones) already carries them.
pub(crate) const fn mcp_servers(_spec: &SessionSpec) -> ResolvedMcpServers {
    ResolvedMcpServers::new()
}

/// Who a caller is, as a session records it (ADR 0013). Agents (H4) get
/// their own principal later.
fn creator(principal: &Principal) -> Creator {
    match principal {
        Principal::Owner => Creator::Owner,
        Principal::Device { id, .. } => Creator::Device { id: *id },
    }
}
