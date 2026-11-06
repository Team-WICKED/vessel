//! What applying a manifest to some targets would do: [`plan`].
//!
//! [Decision Log]
//! - Purpose: decide, from what is on disk and what Vessel recorded,
//!   which files to write, which to leave as somebody's edit, and which to
//!   leave alone entirely, before anything is written; `vessel fleet
//!   status` can show a plan without applying it.
//! - Rules, for each bundle file at its place in each target:
//!   - nothing there: write it (a deleted file comes back: the home is
//!     where the fleet's files are changed, and its `skip` globs are where
//!     one is left out);
//!   - the incoming content already there: nothing to write; it is
//!     recorded as Vessel's (adopted), so later updates reach it;
//!   - Vessel's content (the recorded hash): replace it;
//!   - other content: a local edit, or a file Vessel never wrote. It is
//!     kept, the incoming version goes beside it, and it is a conflict;
//!   - the place is in a source, or reached through a symlink into one:
//!     skipped (the home's own homes, linked to the sources);
//!   - a symlink below the root to anywhere else, or a directory where a
//!     file goes: skipped and reported, never followed or replaced.
//! - A file Vessel wrote that left the bundle is removed if it is
//!   unchanged, and forgotten (left as it is) if it was edited.
//! - Targets are told apart by their canonical root, so two profiles
//!   sharing a home are written once.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::{BlobHash, BundleFile, BundlePath, Manifest};

use crate::managed::Managed;
use crate::target::{Found, Target, inspect};

/// What applying a manifest would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Changes, in order.
    pub steps: Vec<Step>,
    /// Places left alone, and why.
    pub skips: Vec<Skip>,
    /// Places that already hold what the bundle has and are recorded.
    pub unchanged: usize,
}

/// One change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write the bundle's file: new, or replacing what Vessel wrote
    /// (`replaces`, checked again right before writing).
    Write {
        /// Its bundle path.
        bundle: BundlePath,
        /// Where.
        path: PathBuf,
        /// What.
        file: BundleFile,
        /// The target's root; directories are created below it only.
        root: PathBuf,
        /// The content it replaces, if any.
        replaces: Option<BlobHash>,
    },
    /// The file already holds the bundle's content: record it as Vessel's.
    Adopt {
        /// Where.
        path: PathBuf,
        /// Its content's hash.
        hash: BlobHash,
    },
    /// A local edit, or a file Vessel did not write: kept; the incoming
    /// version goes beside it, and it is a conflict.
    Keep {
        /// Its bundle path.
        bundle: BundlePath,
        /// Where.
        path: PathBuf,
        /// The incoming version.
        file: BundleFile,
        /// The target's root.
        root: PathBuf,
        /// The content there now.
        local: BlobHash,
    },
    /// Vessel's file, unchanged, gone from the bundle (or an incoming
    /// version no longer needed): removed, and empty directories above it
    /// up to `stop` with it.
    Remove {
        /// Where.
        path: PathBuf,
        /// The content it must still have.
        hash: BlobHash,
        /// A directory never removed (the skills directory).
        stop: PathBuf,
        /// Whether it is an incoming version (recorded apart from files).
        incoming: bool,
    },
    /// A file Vessel wrote that was edited and left the bundle (or an
    /// incoming version that was edited or removed): forgotten, left as it
    /// is.
    Release {
        /// Where.
        path: PathBuf,
        /// Whether it is an incoming version.
        incoming: bool,
    },
}

/// A place left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    /// The bundle path.
    pub bundle: BundlePath,
    /// The place.
    pub path: PathBuf,
    /// Why.
    pub reason: SkipReason,
}

/// Why a place is left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// It is, or links to, the source the bundle is made from.
    Source,
    /// A symlink below the target's root, which is never followed.
    Symlink(PathBuf),
    /// Something other than a file (or a directory above it) is there.
    Blocked,
    /// The target's names are not plain relative paths, or the path does
    /// not belong in a target.
    Unplaced,
}

/// What applying `manifest` to `targets` would do, given what Vessel
/// recorded (`managed`) and the sources the bundle is made from on this
/// node (`sources`: the instructions file and the skills directory on the
/// home; nothing on a member).
///
/// # Errors
/// A place cannot be inspected (a permission error, a failed read).
pub fn plan(
    manifest: &Manifest,
    targets: &[Target],
    managed: &Managed,
    sources: &[PathBuf],
) -> io::Result<Plan> {
    let sources: Vec<PathBuf> = sources
        .iter()
        .filter_map(|source| fs::canonicalize(source).ok())
        .collect();
    let mut plan = Plan::default();
    let mut seen_roots = BTreeSet::new();
    let mut placed = BTreeMap::new();
    for target in targets {
        let root = fs::canonicalize(&target.root).unwrap_or_else(|_missing| target.root.clone());
        if !seen_roots.insert(root) {
            continue;
        }
        for (bundle, file) in &manifest.files {
            let Some(path) = target.place(bundle) else {
                plan.skips.push(Skip {
                    bundle: bundle.clone(),
                    path: target.root.clone(),
                    reason: SkipReason::Unplaced,
                });
                continue;
            };
            let _placed = placed.insert(path.clone(), target);
            plan.add(target, bundle, &path, file, managed, &sources)?;
        }
    }
    plan.leftovers(targets, managed, &placed, &sources)?;
    Ok(plan)
}

impl Plan {
    /// The bundle paths that are in conflict somewhere.
    #[must_use]
    pub fn conflicts(&self) -> BTreeSet<&BundlePath> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                Step::Keep { bundle, .. } => Some(bundle),
                Step::Write { .. }
                | Step::Adopt { .. }
                | Step::Remove { .. }
                | Step::Release { .. } => None,
            })
            .collect()
    }

    fn add(
        &mut self,
        target: &Target,
        bundle: &BundlePath,
        path: &Path,
        file: &BundleFile,
        managed: &Managed,
        sources: &[PathBuf],
    ) -> io::Result<()> {
        let skip = |reason| Skip {
            bundle: bundle.clone(),
            path: path.to_path_buf(),
            reason,
        };
        match inspect(&target.root, path, sources)? {
            Found::Source => self.skips.push(skip(SkipReason::Source)),
            Found::Symlink(link) => self.skips.push(skip(SkipReason::Symlink(link))),
            Found::Blocked => self.skips.push(skip(SkipReason::Blocked)),
            Found::Missing => self.steps.push(Step::Write {
                bundle: bundle.clone(),
                path: path.to_path_buf(),
                file: file.clone(),
                root: target.root.clone(),
                replaces: None,
            }),
            Found::File { hash } if hash == file.hash => {
                if managed.file(path) == Some(&hash) {
                    self.unchanged = self.unchanged.saturating_add(1);
                } else {
                    self.steps.push(Step::Adopt {
                        path: path.to_path_buf(),
                        hash,
                    });
                }
            }
            Found::File { hash } if managed.file(path) == Some(&hash) => {
                self.steps.push(Step::Write {
                    bundle: bundle.clone(),
                    path: path.to_path_buf(),
                    file: file.clone(),
                    root: target.root.clone(),
                    replaces: Some(hash),
                });
            }
            Found::File { hash } => self.steps.push(Step::Keep {
                bundle: bundle.clone(),
                path: path.to_path_buf(),
                file: file.clone(),
                root: target.root.clone(),
                local: hash,
            }),
        }
        Ok(())
    }

    /// Files Vessel wrote under these targets that the bundle no longer
    /// places, and incoming versions no longer needed.
    fn leftovers(
        &mut self,
        targets: &[Target],
        managed: &Managed,
        placed: &BTreeMap<PathBuf, &Target>,
        sources: &[PathBuf],
    ) -> io::Result<()> {
        let kept: BTreeSet<PathBuf> = self
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Keep { path, .. } => Some(incoming_path(path)),
                Step::Write { .. }
                | Step::Adopt { .. }
                | Step::Remove { .. }
                | Step::Release { .. } => None,
            })
            .collect();
        for (path, recorded) in managed.files() {
            if placed.contains_key(path) {
                continue;
            }
            let Some(target) = targets.iter().find(|target| path.starts_with(&target.root)) else {
                // A target this node no longer writes: its files stay.
                continue;
            };
            self.leftover(target, path, recorded, false, sources)?;
        }
        for (path, recorded) in managed.incoming_paths() {
            if kept.contains(path) {
                continue;
            }
            if let Some(target) = targets.iter().find(|target| path.starts_with(&target.root)) {
                self.leftover(target, path, recorded, true, sources)?;
            }
        }
        Ok(())
    }

    fn leftover(
        &mut self,
        target: &Target,
        path: &Path,
        recorded: &BlobHash,
        incoming: bool,
        sources: &[PathBuf],
    ) -> io::Result<()> {
        let found = inspect(&target.root, path, sources)?;
        let step = match found {
            Found::File { hash } if hash == *recorded => Step::Remove {
                path: path.to_path_buf(),
                hash,
                stop: target.skills_dir(),
                incoming,
            },
            // Edited, gone, or no longer a plain file: no longer ours.
            Found::File { .. }
            | Found::Missing
            | Found::Source
            | Found::Symlink(_)
            | Found::Blocked => Step::Release {
                path: path.to_path_buf(),
                incoming,
            },
        };
        self.steps.push(step);
        Ok(())
    }
}

/// Where the incoming version of the file at `path` goes.
pub(crate) fn incoming_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(crate::INCOMING_SUFFIX);
    path.with_file_name(name)
}
