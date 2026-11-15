//! Applying a manifest into this node's harness homes ([`Local`]), and
//! settling deleted files ([`restore`]).
//!
//! [Decision Log]
//! - Targets: every profile's home (ADR 0011), each harness's default
//!   profile included, through `NodeProfiles::home` with the account the
//!   node was given (a profile that does not set the home variable gets
//!   that account's, as the harness would) and
//!   `vessel_fleet::Target::for_harness`.
//!   Homes that do not exist are skipped by the plan, so a harness that is
//!   not set up here gets nothing. A harness that also reads another's
//!   default home (`HarnessLayout::also_reads`: `OpenCode` reads
//!   `~/.claude`) is left out when that home is a target (ADR 0012, "As
//!   built"); which harness that is stays in `vessel-harness`.
//! - `FleetTargets::Only` replaces the profiles with fixed homes: tests,
//!   which must never write into the real harness homes of the machine
//!   they run on.
//! - Applying is blocking file work under the fleet lock (`files`), run on
//!   a blocking thread; the result goes to `state.json`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vessel_fleet::{BlobStore, Managed, Plan, SkipReason, Target};
use vessel_harness::Profile;
use vessel_proto::{BundlePath, HarnessKind, Manifest};

use super::FleetTargets;
use super::files::{self, FileError, FleetFiles, FleetLock};
use super::mcp::FleetMcp;
use super::state::StateFile;
use crate::profiles::NodeProfiles;

/// Where bundles are written on this node.
#[derive(Debug, Clone)]
pub(crate) enum TargetSource {
    /// Each profile's home.
    Profiles(Arc<NodeProfiles>),
    /// These homes only.
    Only(Vec<(HarnessKind, PathBuf)>),
}

impl TargetSource {
    pub(crate) fn new(targets: &FleetTargets, profiles: &Arc<NodeProfiles>) -> Self {
        match targets {
            FleetTargets::Profiles => Self::Profiles(Arc::clone(profiles)),
            FleetTargets::Only(homes) => Self::Only(homes.clone()),
        }
    }

    /// The targets now.
    fn targets(&self) -> Vec<Target> {
        match self {
            Self::Only(homes) => homes
                .iter()
                .filter_map(|(harness, root)| Target::for_harness(*harness, root.clone()))
                .collect(),
            Self::Profiles(profiles) => profile_targets(profiles),
        }
    }
}

fn profile_targets(profiles: &NodeProfiles) -> Vec<Target> {
    let mut targets: Vec<(HarnessKind, Target)> = profiles
        .list(None)
        .into_iter()
        .filter_map(|info| profiles.get(&info.name))
        .filter_map(|profile| {
            let root = profiles.home(&profile)?;
            Some((
                profile.harness(),
                Target::for_harness(profile.harness(), root)?,
            ))
        })
        .collect();
    // A harness that also reads another's default home (the harness module
    // says which, `HarnessLayout::also_reads`) is left out when that home
    // is filled: it would see every fleet file twice.
    let filled: Vec<(HarnessKind, PathBuf)> = targets
        .iter()
        .map(|(harness, target)| (*harness, canonical(&target.root)))
        .collect();
    targets.retain(|(harness, _target)| {
        vessel_harness::layout(*harness)
            .also_reads
            .is_none_or(|other| {
                let Some(default) = profiles.home(&Profile::default_for(other)) else {
                    return true;
                };
                let default = canonical(&default);
                !filled
                    .iter()
                    .any(|(kind, root)| *kind == other && *root == default)
            })
    });
    targets
        .into_iter()
        .map(|(_harness, target)| target)
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_missing| path.to_path_buf())
}

/// What an apply did, for the report to the home.
#[derive(Debug, Clone, Default)]
pub(crate) struct Outcome {
    /// Bundle paths left as they are somewhere here.
    pub(crate) conflicts: Vec<BundlePath>,
}

/// Why an apply did not happen.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalError {
    /// The lock, `managed.json` or the blob store.
    #[error(transparent)]
    File(#[from] FileError),
    /// `managed.json` is damaged.
    #[error(transparent)]
    Managed(#[from] vessel_fleet::ManagedError),
    /// No plan: a source cannot be found.
    #[error(transparent)]
    Plan(#[from] vessel_fleet::PlanError),
}

/// Applies manifests into this node's harness homes.
#[derive(Debug)]
pub(crate) struct Local {
    pub(crate) files: FleetFiles,
    targets: TargetSource,
    pub(crate) state: Arc<StateFile>,
    /// Sessions' MCP servers: the definitions of the manifest applied.
    pub(crate) mcp: Arc<FleetMcp>,
}

impl Local {
    pub(crate) const fn new(
        files: FleetFiles,
        targets: TargetSource,
        state: Arc<StateFile>,
        mcp: Arc<FleetMcp>,
    ) -> Self {
        Self {
            files,
            targets,
            state,
            mcp,
        }
    }

    /// Apply `manifest` (its blobs in `blobs`), protecting `sources` (the
    /// home's own; none on a member), and record what happened. Blocks.
    pub(crate) fn apply(
        &self,
        manifest: &Manifest,
        blobs: &BlobStore,
        sources: &[PathBuf],
    ) -> Result<Outcome, LocalError> {
        // Sessions get this manifest's MCP servers whatever happens to its
        // files: the definitions do not depend on them.
        self.mcp.set_definitions(manifest.mcp.clone());
        let result = self.apply_locked(manifest, blobs, sources);
        match &result {
            Ok((applied, skipped)) => {
                self.state.update(|state| {
                    state.applied(manifest.generation, applied, *skipped);
                    state.error = None;
                });
            }
            Err(error) => {
                let message = error.to_string();
                self.state.update(|state| state.error = Some(message));
            }
        }
        result.map(|(applied, _skipped)| Outcome {
            conflicts: applied.conflicted(),
        })
    }

    fn apply_locked(
        &self,
        manifest: &Manifest,
        blobs: &BlobStore,
        sources: &[PathBuf],
    ) -> Result<(vessel_fleet::Applied, usize), LocalError> {
        let _lock = FleetLock::acquire(&self.files)?;
        let managed_path = self.files.managed();
        let mut managed = Managed::load(&managed_path)?;
        let targets = self.targets.targets();
        let plan = vessel_fleet::plan(manifest, &targets, &managed, sources)?;
        log_skips(&plan);
        let applied = vessel_fleet::apply(&plan, blobs, &mut managed);
        managed.save(&managed_path)?;
        tracing::info!(
            generation = manifest.generation.get(),
            written = applied.written,
            adopted = applied.adopted,
            removed = applied.removed,
            conflicts = applied.conflicts.len(),
            failures = applied.failures.len(),
            "applied the fleet bundle"
        );
        Ok((applied, plan.skips().len()))
    }
}

fn log_skips(plan: &Plan) {
    for skip in plan.skips() {
        match &skip.reason {
            // Expected on the home and for harnesses not set up here.
            SkipReason::Source | SkipReason::NoHome => {}
            SkipReason::Symlink(_)
            | SkipReason::Blocked
            | SkipReason::Unplaced
            | SkipReason::Collision
            | SkipReason::Unreadable { .. } => tracing::warn!(
                path = %skip.path.display(),
                reason = ?skip.reason,
                "a fleet file was left alone"
            ),
        }
    }
}

/// Why `vessel fleet restore` did not restore.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// The lock or a file.
    #[error(transparent)]
    File(#[from] FileError),
    /// `managed.json` cannot be used.
    #[error(transparent)]
    Managed(#[from] vessel_fleet::ManagedError),
}

/// Settle fleet files deleted here: forget the records of those at `path`
/// (a file, or every file below a directory) that are missing, and ask the
/// node to apply again, which writes them back. How many. Blocks.
///
/// # Errors
/// See [`RestoreError`].
pub fn restore(files: &FleetFiles, path: &Path) -> Result<usize, RestoreError> {
    let restored = {
        let _lock = FleetLock::acquire(files)?;
        let managed_path = files.managed();
        let mut managed = Managed::load(&managed_path)?;
        let restored = managed.restore(path);
        if restored > 0 {
            managed.save(&managed_path)?;
        }
        restored
    };
    if restored > 0 {
        files::write_json(&files.apply_request(), &chrono::Utc::now())?;
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the default Claude Code home among the targets, the harness
    /// that also reads it (`OpenCode`) gets no copy of its own.
    #[test]
    fn a_harness_reading_a_filled_home_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = NodeProfiles::load(
            vessel_store::DataDir::new(dir.path()),
            vessel_harness::HarnessEnv::new(dir.path().to_owned()),
        )
        .unwrap();
        let roots: Vec<PathBuf> = profile_targets(&profiles)
            .into_iter()
            .map(|target| target.root)
            .collect();
        let home_of = |harness| canonical(&profiles.home(&Profile::default_for(harness)).unwrap());
        let roots: Vec<PathBuf> = roots.iter().map(|root| canonical(root)).collect();
        assert!(roots.contains(&home_of(HarnessKind::ClaudeCode)));
        assert!(roots.contains(&home_of(HarnessKind::Codex)));
        assert!(!roots.contains(&home_of(HarnessKind::OpenCode)));
    }
}
