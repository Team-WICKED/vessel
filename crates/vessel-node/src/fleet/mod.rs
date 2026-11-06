//! Fleet sync on this node (ADR 0012, plan F2): the home role (publishing
//! the bundle made from `fleet.toml`, serving `fleet.*`), the member role
//! (pulling it from the home, applying it into every profile's home,
//! reporting), and the files both keep.
//!
//! ```text
//!  fleet.toml ──stamp──▶ bundle ──▶ manifest.json + blobs/ ──▶ Home ◀── fleet.manifest/blob/watch
//!                                        │                     ▲          fleet.report/members
//!                                        ▼ apply here          │ (secure channel, sync scope)
//!  member.json + member.credential ──▶ member::run ── pull ────┘
//!                                        │ fetch blobs, apply, report
//!                                        ▼
//!                  managed.json, state.json  ◀── vessel fleet status / restore
//! ```
//!
//! [Decision Log]
//! - Purpose: one person's instructions and skills, edited on one node,
//!   reach every profile of every node without losing local edits (the
//!   materialiser's rules are `vessel-fleet`'s).
//! - State in files under `<data_dir>/fleet/` (`files`), not in the event
//!   log: it is small, and the CLI must read and write some of it with the
//!   node stopped or running (`vessel fleet join`, `status`, `restore`).
//! - One loop (`supervisor`) looks at the files every few seconds
//!   (`FleetConfig::poll`) and acts on what changed: `fleet.toml` makes the
//!   node the home; `member.json` makes it a member (the loop starts or
//!   stops the pull loop); `fleet/apply` asks for an apply. Polling a few
//!   small files costs little and needs no protocol between the CLI and
//!   the node beyond the files themselves (protocol 5 has no call for it).
//! - A node is the home or a member, never both: `init` refuses a member,
//!   `join` refuses a home, and if both files appear anyway the home wins
//!   and the member loop stops.
//! - Sessions' MCP servers (F3): `mcp::FleetMcp` is the node's
//!   `McpSource`, answering from a snapshot of the applied manifest's
//!   definitions and `secrets.toml` (`secrets`), which this loop keeps
//!   current.
//! - Targets come from the node's profiles (`local`); tests replace them
//!   with fixed homes (`FleetTargets::Only`) so nothing is ever written
//!   into the real harness homes of the machine they run on.
//! - Alternatives: the home pushing bundles to members (members behind NAT
//!   are reachable only through relays they dial; pull fits ADR 0012's
//!   topology); a subscription frame for `fleet.watch` (a long poll needs
//!   no new frame type).

mod bindings;
mod chunks;
mod config;
mod files;
mod home;
mod links;
mod local;
mod mcp;
mod member;
mod secrets;
mod state;
mod supervisor;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_proto::HarnessKind;

pub(crate) use chunks::{fetch, serve};
pub use config::{FleetInit, InitError, SourcesError, init};
pub use files::{FileError, FleetFiles};
pub(crate) use home::Home;
pub(crate) use links::{HomeRelays, Links};
pub use local::{RestoreError, restore};
pub(crate) use mcp::FleetMcp;
pub use member::{DEFAULT_GRANT, JoinError, Joined, join, leave};
pub(crate) use secrets::secret_value;
pub use secrets::{SecretsError, delete_secret, secret_names, set_secret};
pub use state::{ConflictEntry, ConflictWhy, FailureEntry, FleetState, FleetStatus, Role, status};
pub(crate) use vessel_client::FetchError;
/// A `skip` pattern of `fleet.toml` (`vessel fleet init --skip`).
pub use vessel_fleet::SkipGlob;

use crate::profiles::NodeProfiles;

/// Where the node writes bundles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FleetTargets {
    /// Each profile's home (every harness's default profile included),
    /// where it exists.
    #[default]
    Profiles,
    /// These harness homes only (tests; never the profiles').
    Only(Vec<(HarnessKind, PathBuf)>),
}

/// How the node takes part in a fleet.
#[derive(Debug, Clone)]
pub struct FleetConfig {
    /// Where bundles are written.
    pub targets: FleetTargets,
    /// How often the node looks at `fleet.toml`, its sources, its
    /// membership and requests to apply: 5 s.
    pub poll: Duration,
    /// How often a member pulls even without news: 10 minutes.
    pub pull_every: Duration,
    /// How long the home holds a `fleet.watch`: 50 s, under the 60 s idle
    /// limits proxies commonly have.
    pub watch_limit: Duration,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            targets: FleetTargets::Profiles,
            poll: Duration::from_secs(5),
            pull_every: Duration::from_secs(10 * 60),
            watch_limit: Duration::from_secs(50),
        }
    }
}

/// Start the fleet loop of the node keeping its state in `files`; the
/// home's side and the links across nodes, for the service. Blocks briefly
/// (reads `members.json` and `state.json`, and brings a home's binding
/// record up to date).
pub(crate) fn start(
    files: FleetFiles,
    config: &FleetConfig,
    profiles: &Arc<NodeProfiles>,
    mcp: Arc<FleetMcp>,
    links: Arc<Links>,
    tasks: &TaskTracker,
    stop: &CancellationToken,
) -> Arc<Home> {
    let home = Arc::new(Home::new(files.clone(), config.watch_limit, stop.clone()));
    // A home's binding record from before names were kept gets them now,
    // from the grants it keeps (`bindings.rs`).
    match bindings::upgrade(&files) {
        Ok(true) => tracing::info!("filled in the names the fleet's binding record lacked"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "cannot bring the fleet's binding record up to date"),
    }
    let state = Arc::new(state::StateFile::open(&files));
    let local = Arc::new(local::Local::new(
        files,
        local::TargetSource::new(&config.targets, profiles),
        state,
        mcp,
    ));
    drop(tasks.spawn(supervisor::run(
        local,
        Arc::clone(&home),
        links,
        config.clone(),
        Some(profiles.user_home().to_owned()),
        tasks.clone(),
        stop.clone(),
    )));
    home
}
