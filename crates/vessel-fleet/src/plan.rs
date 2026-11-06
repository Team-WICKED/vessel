//! What applying a manifest to some targets would do: [`plan`].
//!
//! [Decision Log]
//! - Purpose: decide, from what is on disk and what Vessel recorded,
//!   which files to write, which to leave as somebody's edit, and which to
//!   leave alone entirely, before anything is written; `vessel fleet
//!   status` can show a plan without applying it.
//! - Rules, for each bundle file at its place in each target (content is
//!   the hash and the executable bit together):
//!   - nothing there and nothing recorded: write it;
//!   - nothing there, but Vessel wrote it before: deleted locally, which
//!     is a local edit like any other (often a deliberate opt-out). It
//!     stays deleted and is a conflict of its own kind; the record is
//!     kept, so status still shows it, and nothing is written beside it.
//!     `Managed::restore` settles it: the next apply writes it again;
//!   - the incoming content already there: nothing to write; recorded as
//!     Vessel's (adopted if Vessel had no record), so later updates reach
//!     it;
//!   - Vessel's content (the record): replace it, the mode included;
//!   - other content: a local edit, or a file Vessel never wrote. It is
//!     kept, the incoming version goes beside it, and it is a conflict;
//!   - the place is in a source, or reached through a symlink into one:
//!     skipped (the home's own homes, linked to the sources);
//!   - a symlink below the root to anywhere else, or a directory where a
//!     file goes: skipped and reported, never followed or replaced.
//! - A file Vessel wrote that left the bundle is removed if it is
//!   unchanged, and forgotten (left as it is) if it was edited or adopted.
//!   It is never removed when it is the same file as one the bundle places
//!   (by device and inode; by name without case where that is unknown):
//!   an alias, a hard link, or a name that changed only in case on a
//!   filesystem that ignores case.
//! - Targets are told apart by their canonical root, so two profiles
//!   sharing a home (`~/.claude-alt` linked to `~/.claude`) are written
//!   once, and records are kept by canonical root. A root that does not
//!   exist is not created (the harness is not set up there).
//! - A place that cannot be read is skipped with the error, and the rest
//!   of its target and every other target carry on: one unreadable
//!   profile must not stop the others. Only a source that cannot be found
//!   fails the plan, since what to protect would be unknown.
//! - Bundle paths a target has no place for (another harness's files, top
//!   levels reserved for later versions) are ignored, not reported.
//! - A [`Plan`] can only be made by [`plan`], and it carries what was
//!   protected and which files were placed, so [`apply`](crate::apply())
//!   checks every step against them again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::{BundleFile, BundlePath, Manifest};

use crate::dir::Identity;
use crate::managed::{Content, Managed, Record};
use crate::place::{Found, MissingSource, Protected, inspect};
use crate::rel::Rel;
use crate::target::{Place as Placement, Target};

/// A place in a target: its canonical root and the path below it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place {
    pub(crate) root: PathBuf,
    pub(crate) rel: Rel,
}

impl Place {
    /// The place's path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.rel.under(&self.root)
    }

    /// The target's canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// What applying a manifest would do. Made only by [`plan`].
#[derive(Debug, Clone)]
pub struct Plan {
    steps: Vec<Step>,
    skips: Vec<Skip>,
    unchanged: usize,
    pub(crate) protected: Protected,
    /// Each planned root, with its skills directory (never removed).
    pub(crate) roots: BTreeMap<PathBuf, Option<Rel>>,
    /// The files at placed places, by identity.
    placed: BTreeSet<Identity>,
    /// The placed places, by root and name without case.
    placed_names: BTreeSet<(PathBuf, String)>,
}

/// One change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write the bundle's file: new, or replacing what Vessel wrote
    /// (`replaces`).
    Write {
        /// Its bundle path.
        bundle: BundlePath,
        /// Where.
        place: Place,
        /// What.
        file: BundleFile,
        /// The content it replaces, if any.
        replaces: Option<Content>,
    },
    /// The file already holds the bundle's content: record it as Vessel's.
    Adopt {
        /// Its bundle path.
        bundle: BundlePath,
        /// Where.
        place: Place,
        /// What it holds.
        file: BundleFile,
    },
    /// A file Vessel wrote that was deleted here: it stays deleted, its
    /// record is kept, and it is a conflict until restored.
    Deleted {
        /// Its bundle path.
        bundle: BundlePath,
        /// Where it was.
        place: Place,
        /// The bundle's version.
        file: BundleFile,
    },
    /// A local edit, or a file Vessel did not write: kept; the incoming
    /// version goes beside it, and it is a conflict.
    Keep {
        /// Its bundle path.
        bundle: BundlePath,
        /// Where.
        place: Place,
        /// The incoming version.
        file: BundleFile,
        /// The content there now.
        local: Content,
    },
    /// Vessel's file, unchanged, gone from the bundle (or an incoming
    /// version no longer needed): removed, with the directories it leaves
    /// empty below the skills directory.
    Remove {
        /// Where.
        place: Place,
        /// The content it must still have.
        recorded: Content,
        /// Whether it is an incoming version.
        incoming: bool,
    },
    /// A file Vessel recorded that is no longer its to remove (edited,
    /// adopted, gone, or the same file as a placed one): forgotten, left
    /// as it is.
    Release {
        /// Where.
        place: Place,
        /// Whether it is an incoming version.
        incoming: bool,
    },
}

/// A place (or a whole target) left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    /// The bundle path, or `None` for a whole target.
    pub bundle: Option<BundlePath>,
    /// The place, or the target's root.
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
    /// The target's root does not exist: the harness is not set up there,
    /// and Vessel does not create its home.
    NoHome,
    /// It could not be read.
    Unreadable {
        /// The error's kind.
        kind: io::ErrorKind,
        /// The error.
        message: String,
    },
}

impl SkipReason {
    fn unreadable(error: &io::Error) -> Self {
        Self::Unreadable {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

/// No plan could be made.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// A source cannot be found, so what to protect is unknown.
    #[error("a fleet source cannot be found: {0}")]
    Source(#[from] MissingSource),
}

/// What applying `manifest` to `targets` would do, given what Vessel
/// recorded (`managed`) and the sources the bundle is made from on this
/// node (`sources`: the instructions file and the skills directory on the
/// home; nothing on a member).
///
/// # Errors
/// A source cannot be found ([`PlanError`]). Problems with targets are
/// [`Plan::skips`].
pub fn plan(
    manifest: &Manifest,
    targets: &[Target],
    managed: &Managed,
    sources: &[PathBuf],
) -> Result<Plan, PlanError> {
    let mut plan = Plan {
        steps: Vec::new(),
        skips: Vec::new(),
        unchanged: 0,
        protected: Protected::new(sources)?,
        roots: BTreeMap::new(),
        placed: BTreeSet::new(),
        placed_names: BTreeSet::new(),
    };
    let mut placed_rels = BTreeSet::new();
    for target in targets {
        let root = match fs::canonicalize(target.root()) {
            Ok(root) => root,
            Err(error) => {
                let reason = if error.kind() == io::ErrorKind::NotFound {
                    SkipReason::NoHome
                } else {
                    SkipReason::unreadable(&error)
                };
                plan.skip_target(target.root(), reason);
                continue;
            }
        };
        if plan.roots.contains_key(&root) {
            continue;
        }
        if let Err(error) = crate::dir::Dir::open_root(&root) {
            plan.skip_target(&root, SkipReason::unreadable(&error));
            continue;
        }
        let _new = plan.roots.insert(root.clone(), target.skills_rel());
        for (bundle, file) in &manifest.files {
            let rel = match target.placement(bundle) {
                Placement::At(rel) => rel,
                Placement::Unused => continue,
                Placement::Invalid => {
                    plan.skips.push(Skip {
                        bundle: Some(bundle.clone()),
                        path: target.root().to_path_buf(),
                        reason: SkipReason::Unplaced,
                    });
                    continue;
                }
            };
            let place = Place {
                root: root.clone(),
                rel,
            };
            let _new = plan.placed_names.insert((root.clone(), place.rel.folded()));
            let _new = placed_rels.insert(place.clone());
            plan.add(bundle, place, file, managed);
        }
    }
    plan.leftovers(managed, &placed_rels);
    Ok(plan)
}

/// What to do at a placed place, given what is there now and what Vessel
/// recorded; decided once when planning and again right before acting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// It holds the bundle's content and is recorded as such.
    Unchanged,
    /// Write the bundle's file (replacing this content of Vessel's).
    Write(Option<Content>),
    /// It holds the bundle's content; record it.
    Adopt,
    /// Vessel wrote it and it was deleted.
    Deleted,
    /// Somebody's content: keep it.
    Keep(Content),
    /// Leave it alone.
    Skip(SkipReason),
}

pub(crate) fn decide(found: Found, wanted: &Content, record: Option<&Record>) -> Decision {
    match found {
        Found::Source => Decision::Skip(SkipReason::Source),
        Found::Symlink(link) => Decision::Skip(SkipReason::Symlink(link)),
        Found::Blocked => Decision::Skip(SkipReason::Blocked),
        Found::Missing if record.is_some() => Decision::Deleted,
        Found::Missing => Decision::Write(None),
        Found::File { content, .. } if content.same(wanted) => {
            if record.is_some_and(|record| record.content().same(&content)) {
                Decision::Unchanged
            } else {
                Decision::Adopt
            }
        }
        Found::File { content, .. }
            if record.is_some_and(|record| record.content().same(&content)) =>
        {
            Decision::Write(Some(content))
        }
        Found::File { content, .. } => Decision::Keep(content),
    }
}

impl Plan {
    /// The changes, in order.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Places (and targets) left alone, and why.
    #[must_use]
    pub fn skips(&self) -> &[Skip] {
        &self.skips
    }

    /// Places that already hold what the bundle has and are recorded.
    #[must_use]
    pub const fn unchanged(&self) -> usize {
        self.unchanged
    }

    /// The bundle paths that are in conflict somewhere.
    #[must_use]
    pub fn conflicts(&self) -> BTreeSet<&BundlePath> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                Step::Keep { bundle, .. } | Step::Deleted { bundle, .. } => Some(bundle),
                Step::Write { .. }
                | Step::Adopt { .. }
                | Step::Remove { .. }
                | Step::Release { .. } => None,
            })
            .collect()
    }

    /// Whether the file at `place` (with `identity`) is one the bundle
    /// places under another name, and so must not be removed.
    pub(crate) fn aliased(&self, place: &Place, identity: Option<Identity>) -> bool {
        match identity {
            Some(identity) => self.placed.contains(&identity),
            None => self
                .placed_names
                .contains(&(place.root.clone(), place.rel.folded())),
        }
    }

    fn skip_target(&mut self, root: &Path, reason: SkipReason) {
        self.skips.push(Skip {
            bundle: None,
            path: root.to_path_buf(),
            reason,
        });
    }

    fn add(&mut self, bundle: &BundlePath, place: Place, file: &BundleFile, managed: &Managed) {
        let found = match inspect(&place.root, &place.rel, &self.protected) {
            Ok(found) => found,
            Err(error) => {
                self.skips.push(Skip {
                    bundle: Some(bundle.clone()),
                    path: place.path(),
                    reason: SkipReason::unreadable(&error),
                });
                return;
            }
        };
        if let Found::File {
            identity: Some(identity),
            ..
        } = &found
        {
            let _new = self.placed.insert(*identity);
        }
        let record = managed.file_at(&place.root, &place.rel);
        let bundle = bundle.clone();
        let file = file.clone();
        match decide(found, &Content::of(&file), record) {
            Decision::Unchanged => self.unchanged = self.unchanged.saturating_add(1),
            Decision::Write(replaces) => self.steps.push(Step::Write {
                bundle,
                place,
                file,
                replaces,
            }),
            Decision::Adopt => self.steps.push(Step::Adopt {
                bundle,
                place,
                file,
            }),
            Decision::Deleted => self.steps.push(Step::Deleted {
                bundle,
                place,
                file,
            }),
            Decision::Keep(local) => self.steps.push(Step::Keep {
                bundle,
                place,
                file,
                local,
            }),
            Decision::Skip(reason) => self.skips.push(Skip {
                bundle: Some(bundle),
                path: place.path(),
                reason,
            }),
        }
    }

    /// Files Vessel recorded under the planned roots that the bundle no
    /// longer places, and incoming versions no longer needed.
    fn leftovers(&mut self, managed: &Managed, placed: &BTreeSet<Place>) {
        let kept: BTreeSet<Place> = self
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Keep { place, .. } => incoming_place(place),
                Step::Write { .. }
                | Step::Deleted { .. }
                | Step::Adopt { .. }
                | Step::Remove { .. }
                | Step::Release { .. } => None,
            })
            .collect();
        let roots: Vec<PathBuf> = self.roots.keys().cloned().collect();
        for root in roots {
            for (rel, record) in managed.files_under(&root) {
                let place = Place {
                    root: root.clone(),
                    rel,
                };
                if !placed.contains(&place) {
                    self.leftover(place, &record, false);
                }
            }
            for (rel, record) in managed.incoming_under(&root) {
                let place = Place {
                    root: root.clone(),
                    rel,
                };
                if !kept.contains(&place) {
                    self.leftover(place, &record, true);
                }
            }
        }
    }

    fn leftover(&mut self, place: Place, record: &Record, incoming: bool) {
        let step = match self.leftover_decision(&place, record) {
            Ok(true) => Step::Remove {
                place,
                recorded: record.content(),
                incoming,
            },
            Ok(false) => Step::Release { place, incoming },
            Err(error) => {
                // Left as it is, record and all, until it can be read.
                self.skips.push(Skip {
                    bundle: None,
                    path: place.path(),
                    reason: SkipReason::unreadable(&error),
                });
                return;
            }
        };
        self.steps.push(step);
    }

    /// Whether the recorded file at `place` is Vessel's to remove now.
    pub(crate) fn leftover_decision(&self, place: &Place, record: &Record) -> io::Result<bool> {
        Ok(match inspect(&place.root, &place.rel, &self.protected)? {
            Found::File { content, identity } => {
                content.same(&record.content()) && !record.adopted && !self.aliased(place, identity)
            }
            // Gone, or no longer a plain file: no longer Vessel's.
            Found::Missing | Found::Source | Found::Symlink(_) | Found::Blocked => false,
        })
    }
}

/// Where the incoming version of the file at `place` goes.
pub(crate) fn incoming_place(place: &Place) -> Option<Place> {
    Some(Place {
        root: place.root.clone(),
        rel: place.rel.with_suffix(crate::INCOMING_SUFFIX)?,
    })
}
