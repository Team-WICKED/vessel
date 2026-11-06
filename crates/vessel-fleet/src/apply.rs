//! Carrying out a [`Plan`]: [`apply`].
//!
//! [Decision Log]
//! - Each step looks at its place again right before it acts and decides
//!   by the same rules as the plan ([`decide`]), since the plan may be
//!   stale: a file edited after planning is kept, not overwritten; a file
//!   that became a symlink is not followed; a file that left the bundle is
//!   removed only if it is still a regular file with the recorded content,
//!   not adopted, and not the same file as a placed one.
//! - Places are reached through open directories from the canonical root
//!   ([`reach`]), so a directory swapped for a symlink after the check
//!   cannot redirect the act. Directories are created owner-only.
//! - Files are written to a temporary name beside the place and renamed
//!   over it, owner-only (executable if the bundle says so), with the blob
//!   streamed and its hash checked before the rename. A damaged blob is
//!   removed from the store, so it is fetched again.
//! - A failure at one place is reported and the rest carry on: one
//!   unwritable profile must not stop the others.

use std::io::{self, Read as _, Write as _};
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};
use vessel_proto::{BlobHash, BundleFile, BundlePath};

use crate::blobs::BlobStore;
use crate::dir::Dir;
use crate::files::Mode;
use crate::managed::{Content, Managed, Record};
use crate::place::{Found, Walk, look, reach};
use crate::plan::{Decision, Place, Plan, Step, decide, incoming_place};

/// What applying a plan did.
#[derive(Debug, Default)]
pub struct Applied {
    /// Files written.
    pub written: usize,
    /// Files found already holding the bundle's content, now recorded.
    pub adopted: usize,
    /// Files removed (gone from the bundle, unchanged since written).
    pub removed: usize,
    /// Places where this node's file and the bundle disagree.
    pub conflicts: Vec<Conflict>,
    /// Places where something failed, and why.
    pub failures: Vec<(PathBuf, ApplyError)>,
}

/// A place where this node's file and the bundle disagree, left as it is
/// here for a person to settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The bundle path.
    pub bundle: BundlePath,
    /// The file (or where it was).
    pub path: PathBuf,
    /// What kind of disagreement.
    pub kind: ConflictKind,
    /// Where the bundle's version was written beside it, if it was (never
    /// for a deleted file; not when a file of that name that Vessel did
    /// not write is in the way).
    pub incoming: Option<PathBuf>,
}

/// How a node's file disagrees with the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Vessel wrote it, and it was changed here since.
    Edited,
    /// A file Vessel never wrote is where the bundle's file goes.
    NotWritten,
    /// Vessel wrote it, and it was deleted here: it stays deleted until
    /// restored (`Managed::restore`).
    Deleted,
}

/// Why a step failed.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The place became a symlink, a source or something other than a
    /// file since the plan was made.
    #[error("the place changed since the plan was made; left alone")]
    Changed,
    /// A blob in the store does not hash to its name. It was removed from
    /// the store, so the next fetch gets it again.
    #[error("blob {expected} in the store is damaged (it hashes to {found}); removed")]
    DamagedBlob {
        /// The blob.
        expected: BlobHash,
        /// What its content hashes to.
        found: BlobHash,
    },
    /// The incoming version's name would not be a plain name.
    #[error("no name for the incoming version")]
    NoIncomingName,
    /// Reading or writing failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Applied {
    /// The bundle paths in conflict, each once, in order: what a member
    /// reports to the home.
    #[must_use]
    pub fn conflicted(&self) -> Vec<BundlePath> {
        let mut paths: Vec<BundlePath> = self
            .conflicts
            .iter()
            .map(|conflict| conflict.bundle.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// Carry out `plan` with content from `blobs`, recording what was written
/// in `managed` (the caller saves it).
pub fn apply(plan: &Plan, blobs: &BlobStore, managed: &mut Managed) -> Applied {
    let mut applied = Applied::default();
    for step in plan.steps() {
        let result = match step {
            Step::Write {
                bundle,
                place,
                file,
                ..
            }
            | Step::Adopt {
                bundle,
                place,
                file,
            }
            | Step::Deleted {
                bundle,
                place,
                file,
            }
            | Step::Keep {
                bundle,
                place,
                file,
                ..
            } => applied.placed(plan, bundle, place, file, blobs, managed),
            Step::Remove {
                place, incoming, ..
            }
            | Step::Release { place, incoming } => {
                applied.leftover(plan, place, *incoming, managed)
            }
        };
        if let Err(error) = result {
            let path = match step {
                Step::Write { place, .. }
                | Step::Adopt { place, .. }
                | Step::Deleted { place, .. }
                | Step::Keep { place, .. }
                | Step::Remove { place, .. }
                | Step::Release { place, .. } => place.path(),
            };
            tracing::warn!(path = %path.display(), %error, "could not apply a fleet file");
            applied.failures.push((path, error));
        }
    }
    applied
}

impl Applied {
    /// A placed file: decide again from what is there now, and do it.
    fn placed(
        &mut self,
        plan: &Plan,
        bundle: &BundlePath,
        place: &Place,
        file: &BundleFile,
        blobs: &BlobStore,
        managed: &mut Managed,
    ) -> Result<(), ApplyError> {
        let wanted = Content::of(file);
        let record = managed.file_at(&place.root, &place.rel).copied();
        let found = crate::place::inspect(&place.root, &place.rel, &plan.protected)?;
        match decide(found, &wanted, record.as_ref()) {
            Decision::Unchanged => Ok(()),
            Decision::Adopt => {
                let adopted = record.is_none_or(|record| record.adopted);
                managed.record(&place.root, place.rel.clone(), Record::new(wanted, adopted));
                self.adopted = self.adopted.saturating_add(1);
                drop_incoming(plan, place, managed)
            }
            Decision::Write(replaces) => {
                write(plan, place, file, blobs, |now| match (&now, replaces) {
                    (Found::Missing, None) => true,
                    (Found::File { content, .. }, Some(replaces)) => content.same(&replaces),
                    _ => false,
                })?;
                let adopted = record.is_some_and(|record| record.adopted);
                managed.record(&place.root, place.rel.clone(), Record::new(wanted, adopted));
                self.written = self.written.saturating_add(1);
                drop_incoming(plan, place, managed)
            }
            Decision::Deleted => {
                self.conflicts.push(Conflict {
                    bundle: bundle.clone(),
                    path: place.path(),
                    kind: ConflictKind::Deleted,
                    incoming: None,
                });
                Ok(())
            }
            Decision::Keep(_local) => {
                let kind = if record.is_some() {
                    ConflictKind::Edited
                } else {
                    ConflictKind::NotWritten
                };
                let incoming = keep(plan, place, file, blobs, managed)?;
                self.conflicts.push(Conflict {
                    bundle: bundle.clone(),
                    path: place.path(),
                    kind,
                    incoming,
                });
                Ok(())
            }
            Decision::Skip(_reason) => Err(ApplyError::Changed),
        }
    }

    /// A recorded file the bundle no longer places (or an incoming version
    /// no longer needed): removed if it is still exactly Vessel's,
    /// otherwise forgotten.
    fn leftover(
        &mut self,
        plan: &Plan,
        place: &Place,
        incoming: bool,
        managed: &mut Managed,
    ) -> Result<(), ApplyError> {
        let record = if incoming {
            managed.incoming_at(&place.root, &place.rel)
        } else {
            managed.file_at(&place.root, &place.rel)
        };
        let Some(record) = record.copied() else {
            return Ok(());
        };
        if plan.leftover_decision(place, &record)? && remove(plan, place, &record)? {
            self.removed = self.removed.saturating_add(1);
        }
        managed.forget(&place.root, &place.rel, incoming);
        Ok(())
    }
}

/// Write `file` at `place`, if what is there right before writing passes
/// `expected` (it may have changed since the decision).
fn write(
    plan: &Plan,
    place: &Place,
    file: &BundleFile,
    blobs: &BlobStore,
    expected: impl Fn(&Found) -> bool,
) -> Result<(), ApplyError> {
    let Walk::Reached(reached) = reach(&place.root, &place.rel, &plan.protected, true)? else {
        return Err(ApplyError::Changed);
    };
    let parent = reached.parent()?;
    let name = place.rel.name();
    if !expected(&look(parent, name, &plan.protected)?) {
        return Err(ApplyError::Changed);
    }
    write_blob(parent, name, file, blobs)
}

/// Keep the file at `place`; write the bundle's version beside it, unless
/// a file there that Vessel did not write is in the way. Where it went.
fn keep(
    plan: &Plan,
    place: &Place,
    file: &BundleFile,
    blobs: &BlobStore,
    managed: &mut Managed,
) -> Result<Option<PathBuf>, ApplyError> {
    let beside = incoming_place(place).ok_or(ApplyError::NoIncomingName)?;
    let wanted = Content::of(file);
    let recorded = managed.incoming_at(&beside.root, &beside.rel).copied();
    let found = crate::place::inspect(&beside.root, &beside.rel, &plan.protected)?;
    let write_it = match &found {
        Found::Missing => Some(None),
        Found::File { content, .. } if content.same(&wanted) => None,
        Found::File { content, .. }
            if recorded.is_some_and(|recorded| recorded.content().same(content)) =>
        {
            Some(Some(*content))
        }
        Found::File { .. } | Found::Source | Found::Symlink(_) | Found::Blocked => {
            return Ok(None);
        }
    };
    if let Some(replaces) = write_it {
        write(plan, &beside, file, blobs, |now| match (now, replaces) {
            (Found::Missing, None) => true,
            (Found::File { content, .. }, Some(replaces)) => content.same(&replaces),
            _ => false,
        })?;
    }
    managed.record_incoming(&beside.root, beside.rel.clone(), Record::new(wanted, false));
    Ok(Some(beside.path()))
}

/// The conflict at `place` is settled: remove the incoming version beside
/// it if it is still what Vessel wrote, and forget it.
fn drop_incoming(plan: &Plan, place: &Place, managed: &mut Managed) -> Result<(), ApplyError> {
    let Some(beside) = incoming_place(place) else {
        return Ok(());
    };
    let Some(record) = managed.incoming_at(&beside.root, &beside.rel).copied() else {
        return Ok(());
    };
    if plan.leftover_decision(&beside, &record)? {
        let _removed = remove(plan, &beside, &record)?;
    }
    managed.forget(&beside.root, &beside.rel, true);
    Ok(())
}

/// Remove the regular file at `place` if it still has `record`'s content,
/// then the directories that leaves empty, down to (not including) the
/// skills directory. Whether it was removed.
fn remove(plan: &Plan, place: &Place, record: &Record) -> Result<bool, ApplyError> {
    let Walk::Reached(reached) = reach(&place.root, &place.rel, &plan.protected, false)? else {
        return Ok(false);
    };
    let parent = reached.parent()?;
    let name = place.rel.name();
    // Checked once more through the open directory, right before removing.
    let Found::File { content, identity } = look(parent, name, &plan.protected)? else {
        return Ok(false);
    };
    if !content.same(&record.content()) || plan.aliased(place, identity) {
        return Ok(false);
    }
    parent.remove_file(name)?;
    remove_empty_parents(plan, place, &reached.dirs);
    Ok(true)
}

/// Remove the now empty directories above `place`, deepest first, never
/// the skills directory or anything above it; a directory that is not
/// empty ends it.
fn remove_empty_parents(plan: &Plan, place: &Place, dirs: &[Dir]) {
    let names = place.rel.names();
    let keep = plan
        .roots
        .get(&place.root)
        .and_then(Option::as_ref)
        .filter(|skills| place.rel.starts_with(skills))
        .map_or(names.len(), |skills| skills.names().len());
    // The directory at depth `depth` is `names[..depth]`, removed from the
    // one above it.
    let mut depth = names.len().saturating_sub(1);
    while depth > keep {
        let above = depth.saturating_sub(1);
        let (Some(dir), Some(name)) = (dirs.get(above), names.get(above)) else {
            break;
        };
        if dir.remove_dir(name).is_err() {
            break;
        }
        depth = above;
    }
}

/// Write blob `file.hash` as `name` in `dir`: streamed to a temporary file
/// while hashing, checked, then renamed into place.
fn write_blob(
    dir: &Dir,
    name: &str,
    file: &BundleFile,
    blobs: &BlobStore,
) -> Result<(), ApplyError> {
    let mode = if file.executable {
        Mode::Executable
    } else {
        Mode::Private
    };
    let source = blobs.open_blob(&file.hash)?;
    dir.replace(name, mode, |destination| {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        // One byte more than the manifest says, so a longer blob shows.
        let mut source = source.take(file.size.saturating_add(1));
        loop {
            let read = source.read(&mut buffer)?;
            let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
                break;
            };
            hasher.update(chunk);
            destination.write_all(chunk)?;
        }
        let found = BlobHash::from_bytes(hasher.finalize().into());
        if found == file.hash {
            return Ok(());
        }
        if let Err(error) = blobs.remove(&file.hash) {
            tracing::warn!(blob = %file.hash, %error, "could not remove a damaged blob");
        }
        Err(ApplyError::DamagedBlob {
            expected: file.hash,
            found,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_blob_is_never_written_and_leaves_the_store() {
        let temp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(temp.path().join("blobs")).unwrap();
        let hash = blobs.insert(b"right").unwrap();
        std::fs::write(blobs.path(&hash), b"wrong").unwrap();
        let file = BundleFile {
            hash,
            size: 5,
            executable: false,
        };
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let dir = Dir::open_root(&out.canonicalize().unwrap()).unwrap();
        let error = write_blob(&dir, "file", &file, &blobs).unwrap_err();
        assert!(
            matches!(error, ApplyError::DamagedBlob { expected, .. } if expected == hash),
            "{error}"
        );
        assert_eq!(
            std::fs::read_dir(&out).unwrap().count(),
            0,
            "nothing written, no temporary left"
        );
        assert!(!blobs.contains(&hash), "fetched again next time");
    }
}
