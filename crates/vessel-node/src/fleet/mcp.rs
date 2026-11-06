//! The fleet's MCP servers for sessions on this node ([`FleetMcp`], the
//! node's `McpSource`, ADR 0012 "MCP servers at launch").
//!
//! [Decision Log]
//! - Purpose: every session gets the fleet's MCP servers, with this node's
//!   secrets filled in; a server whose secret is missing is left out and
//!   named in the session's log (the launcher does that with `missing`).
//! - From memory: the launcher asks on the async runtime, once per launch
//!   and per profile move, so the source answers from a snapshot (the
//!   definitions of the manifest this node last applied or published, and
//!   `secrets.toml`) behind a `std` lock that is never held across an
//!   `.await`. The fleet loop replaces the snapshot when either changes:
//!   a new manifest, the file's text changing, or the node leaving the
//!   fleet (no definitions then).
//! - Every session gets every server for now: definitions carry no profile
//!   or harness selector in protocol 5.

use std::sync::{Arc, PoisonError, RwLock};

use vessel_proto::{McpDefinitions, SessionSpec};

use super::secrets::Secrets;
use crate::profiles::launch::{McpServers, McpSource};

/// What sessions are launched with.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    definitions: McpDefinitions,
    secrets: Secrets,
}

/// The fleet's MCP servers, resolved with this node's secrets.
#[derive(Debug, Default)]
pub(crate) struct FleetMcp {
    snapshot: RwLock<Arc<Snapshot>>,
}

impl FleetMcp {
    fn replace(&self, change: impl FnOnce(&mut Snapshot)) {
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut next = Snapshot::clone(&current);
        change(&mut next);
        *current = Arc::new(next);
    }

    /// Use `definitions` (from the manifest applied or published).
    pub(crate) fn set_definitions(&self, definitions: McpDefinitions) {
        self.replace(|snapshot| snapshot.definitions = definitions);
    }

    /// Use `secrets` (from `secrets.toml`).
    pub(crate) fn set_secrets(&self, secrets: Secrets) {
        self.replace(|snapshot| snapshot.secrets = secrets);
    }

    /// The server names defined now.
    #[cfg(test)]
    pub(crate) fn defined(&self) -> Vec<String> {
        self.current()
            .definitions
            .keys()
            .map(ToString::to_string)
            .collect()
    }

    fn current(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(PoisonError::into_inner))
    }
}

impl McpSource for FleetMcp {
    fn servers(&self, _spec: &SessionSpec) -> McpServers {
        let snapshot = self.current();
        let (servers, missing) =
            vessel_fleet::resolve(&snapshot.definitions, |name| snapshot.secrets.get(name));
        McpServers { servers, missing }
    }
}
