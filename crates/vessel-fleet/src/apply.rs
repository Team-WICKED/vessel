//! Carrying out a [`Plan`]: [`apply`].
//!
//! Each step checks the place again right before it acts, since the plan
//! may be stale (a file edited after planning is kept, not overwritten;
//! a symlink that appeared is not followed). Directories are created one
//! at a time below the target's root, never through a symlink. Files are
//! written to a temporary name beside the place and renamed over it, so a
//! crash leaves the old content or the new. A failure at one place is
//! reported and the rest carry on: one unwritable profile must not stop
//! the others.

use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use vessel_proto::{BundleFile, BundlePath};

use crate::blobs::BlobStore;
use crate::files::{self, Mode};
use crate::managed::Managed;
use crate::plan::{Plan, Step, incoming_path};
use crate::target::{Found, create_parents, inspect};

/// What applying a plan did.
#[derive(Debug, Default)]
pub struct Applied {
    /// Files written.
    pub written: usize,
    /// Files found already holding the bundle's content, now recorded.
    pub adopted: usize,
    /// Files removed (gone from the bundle, unchanged since written).
    pub removed: usize,
    /// Local edits kept, with the incoming version beside them.
    pub conflicts: Vec<Conflict>,
    /// Places where something failed, and why.
    pub failures: Vec<(PathBuf, io::Error)>,
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
/// in `managed` (the caller saves it). `sources` are the same as for
/// [`plan`](crate::plan()).
pub fn apply(
    plan: &Plan,
    blobs: &BlobStore,
    managed: &mut Managed,
    sources: &[PathBuf],
) -> Applied {
    let sources: Vec<PathBuf> = sources
        .iter()
        .filter_map(|source| fs::canonicalize(source).ok())
        .collect();
    let mut applied = Applied::default();
    for step in &plan.steps {
        let path = match step {
            Step::Write { path, .. }
            | Step::Adopt { path, .. }
            | Step::Keep { path, .. }
            | Step::Deleted { path, .. }
            | Step::Remove { path, .. }
            | Step::Release { path, .. } => path.clone(),
        };
        if let Err(error) = applied.step(step, blobs, managed, &sources) {
            tracing::warn!(path = %path.display(), %error, "could not apply a fleet file");
            applied.failures.push((path, error));
        }
    }
    applied
}

impl Applied {
    fn step(
        &mut self,
        step: &Step,
        blobs: &BlobStore,
        managed: &mut Managed,
        sources: &[PathBuf],
    ) -> io::Result<()> {
        match step {
            Step::Write {
                bundle,
                path,
                file,
                root,
                replaces,
            } => {
                match inspect(root, path, sources)? {
                    Found::Missing => {}
                    Found::File { hash } if Some(hash) == *replaces => {}
                    Found::File { hash } if hash == file.hash => {
                        managed.record(path.clone(), hash);
                        self.adopted = self.adopted.saturating_add(1);
                        return Ok(());
                    }
                    // Edited since the plan was made: keep it.
                    Found::File { .. } => {
                        return self.keep(bundle, root, path, file, blobs, managed, sources);
                    }
                    Found::Source | Found::Symlink(_) | Found::Blocked => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "the place changed since the plan was made; left alone",
                        ));
                    }
                }
                create_parents(root, path)?;
                write_blob(path, file, blobs)?;
                managed.record(path.clone(), file.hash);
                self.written = self.written.saturating_add(1);
                Self::drop_incoming(root, path, managed, sources)
            }
            Step::Adopt { path, hash } => {
                managed.record(path.clone(), *hash);
                self.adopted = self.adopted.saturating_add(1);
                Ok(())
            }
            Step::Keep {
                bundle,
                path,
                file,
                root,
                ..
            } => self.keep(bundle, root, path, file, blobs, managed, sources),
            Step::Deleted { bundle, path } => {
                self.conflicts.push(Conflict {
                    bundle: bundle.clone(),
                    path: path.clone(),
                    kind: ConflictKind::Deleted,
                    incoming: None,
                });
                Ok(())
            }
            Step::Remove {
                path,
                hash,
                stop,
                incoming,
            } => {
                // Still exactly what Vessel wrote?
                let (found, _size) = match files::hash_file(path) {
                    Ok(found) => found,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        forget(managed, path, *incoming);
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                let plain = fs::symlink_metadata(path)?.file_type().is_file();
                if found != *hash || !plain {
                    forget(managed, path, *incoming);
                    return Ok(());
                }
                fs::remove_file(path)?;
                forget(managed, path, *incoming);
                self.removed = self.removed.saturating_add(1);
                remove_empty_parents(path, stop);
                Ok(())
            }
            Step::Release { path, incoming } => {
                forget(managed, path, *incoming);
                Ok(())
            }
        }
    }

    /// Keep the file at `path`; write the bundle's version beside it,
    /// unless a file there that Vessel did not write is in the way.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site per step kind; a struct would only rename them"
    )]
    fn keep(
        &mut self,
        bundle: &BundlePath,
        root: &Path,
        path: &Path,
        file: &BundleFile,
        blobs: &BlobStore,
        managed: &mut Managed,
        sources: &[PathBuf],
    ) -> io::Result<()> {
        let beside = incoming_path(path);
        let incoming = match inspect(root, &beside, sources)? {
            Found::Missing => {
                write_blob(&beside, file, blobs)?;
                managed.record_incoming(beside.clone(), file.hash);
                Some(beside)
            }
            Found::File { hash } if hash == file.hash => {
                managed.record_incoming(beside.clone(), hash);
                Some(beside)
            }
            Found::File { hash } if managed.incoming(&beside) == Some(&hash) => {
                write_blob(&beside, file, blobs)?;
                managed.record_incoming(beside.clone(), file.hash);
                Some(beside)
            }
            Found::File { .. } | Found::Source | Found::Symlink(_) | Found::Blocked => None,
        };
        let kind = if managed.file(path).is_some() {
            ConflictKind::Edited
        } else {
            ConflictKind::NotWritten
        };
        self.conflicts.push(Conflict {
            bundle: bundle.clone(),
            path: path.to_path_buf(),
            kind,
            incoming,
        });
        Ok(())
    }

    /// The conflict at `path` is settled: remove the incoming version
    /// beside it if it is still what Vessel wrote.
    fn drop_incoming(
        root: &Path,
        path: &Path,
        managed: &mut Managed,
        sources: &[PathBuf],
    ) -> io::Result<()> {
        let beside = incoming_path(path);
        let Some(recorded) = managed.incoming(&beside).copied() else {
            return Ok(());
        };
        if let Found::File { hash } = inspect(root, &beside, sources)?
            && hash == recorded
        {
            fs::remove_file(&beside)?;
        }
        managed.forget_incoming(&beside);
        Ok(())
    }
}

fn forget(managed: &mut Managed, path: &Path, incoming: bool) {
    if incoming {
        managed.forget_incoming(path);
    } else {
        managed.forget(path);
    }
}

/// Write blob `file.hash` at `path` (atomically, executable if the bundle
/// says so), checking its content on the way.
fn write_blob(path: &Path, file: &BundleFile, blobs: &BlobStore) -> io::Result<()> {
    let mode = if file.executable {
        Mode::Executable
    } else {
        Mode::Ordinary
    };
    let mut source = blobs.open_blob(&file.hash)?;
    files::replace_with(path, mode, |destination| {
        let mut content = Vec::new();
        let _read = (&mut source)
            .take(file.size.saturating_add(1))
            .read_to_end(&mut content)?;
        let found = files::hash_bytes(&content);
        if found != file.hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("blob {} in the store hashes to {found}", file.hash),
            ));
        }
        io::Write::write_all(destination, &content)
    })
}

/// Remove the now empty directories above `path`, up to but not including
/// `stop`; a directory that is not empty (or anything else) ends it.
fn remove_empty_parents(path: &Path, stop: &Path) {
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == stop || !current.starts_with(stop) || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_directories_go_up_to_the_skills_directory_only() {
        let temp = tempfile::tempdir().unwrap();
        let skills = temp.path().join("skills");
        let deep = skills.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(skills.join("a").join("other")).unwrap();
        remove_empty_parents(&deep.join("file"), &skills);
        assert!(!deep.exists());
        assert!(skills.join("a").exists(), "not empty");
        remove_empty_parents(&temp.path().join("CLAUDE.md"), &skills);
        assert!(temp.path().exists());
    }

    #[test]
    fn a_blob_is_checked_as_it_is_written() {
        let temp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(temp.path().join("blobs")).unwrap();
        let hash = blobs.insert(b"right").unwrap();
        fs::write(blobs.path(&hash), b"wrong").unwrap();
        let file = BundleFile {
            hash,
            size: 5,
            executable: false,
        };
        let place = temp.path().join("out");
        assert!(write_blob(&place, &file, &blobs).is_err());
        assert!(!place.exists());
    }
}
