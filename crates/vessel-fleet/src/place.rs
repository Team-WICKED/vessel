//! Looking at a place in a target: [`inspect`], and [`reach`] for acting
//! on it, both from the target's canonical root through open directories
//! ([`Dir`]) without following a symlink; and the sources a node must
//! never write into ([`Protected`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::dir::{Dir, Entry, Identity};
use crate::files;
use crate::managed::Content;
use crate::rel::Rel;

/// The sources the bundle is made from on this node, canonical: no place
/// in them, or reached through a symlink into them, is ever written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Protected(Vec<PathBuf>);

/// A source that cannot be protected because neither it nor its directory
/// can be found.
#[derive(Debug, thiserror::Error)]
#[error("{}: {source}", path.display())]
pub struct MissingSource {
    /// The source as given.
    pub path: PathBuf,
    /// Why it could not be resolved.
    #[source]
    pub source: io::Error,
}

impl Protected {
    /// Each source made canonical. A source that is missing for a moment
    /// (an editor saving `AGENTS.md` by replacing it) is protected by its
    /// canonical directory and its name, so it is never taken for a free
    /// place in between; a source whose directory is missing too is an
    /// error, so no plan is made without knowing what to protect.
    pub(crate) fn new(sources: &[PathBuf]) -> Result<Self, MissingSource> {
        let mut protected = Vec::with_capacity(sources.len());
        for source in sources {
            let canonical = fs::canonicalize(source).or_else(|error| {
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
                let (Some(parent), Some(name)) = (source.parent(), source.file_name()) else {
                    return Err(error);
                };
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                fs::canonicalize(parent).map(|parent| parent.join(name))
            });
            match canonical {
                Ok(canonical) => protected.push(canonical),
                Err(source_error) => {
                    return Err(MissingSource {
                        path: source.clone(),
                        source: source_error,
                    });
                }
            }
        }
        Ok(Self(protected))
    }

    /// Whether the canonical `path` is a source or inside one.
    pub(crate) fn covers(&self, path: &Path) -> bool {
        self.0.iter().any(|source| path.starts_with(source))
    }
}

/// What is at a place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// Nothing (and maybe no directory above it either).
    Missing,
    /// A regular file, reached without any symlink below the root.
    File {
        /// Its content.
        content: Content,
        /// Which file it is.
        identity: Option<Identity>,
    },
    /// The place is, or is reached through, a source.
    Source,
    /// A symlink below the root that does not lead into a source: never
    /// followed.
    Symlink(PathBuf),
    /// Something other than a directory where one is needed, or other than
    /// a regular file at the place itself.
    Blocked,
}

/// The directories from the root down to a place's own directory, open.
#[derive(Debug)]
pub(crate) struct Reached {
    /// `dirs[i]` is the directory at the first `i` names of the place
    /// (`dirs[0]` the root); the last is the place's own directory.
    pub(crate) dirs: Vec<Dir>,
}

impl Reached {
    /// The place's own directory.
    pub(crate) fn parent(&self) -> io::Result<&Dir> {
        self.dirs
            .last()
            .ok_or_else(|| io::Error::other("no directory was opened"))
    }
}

/// How far a walk got.
#[derive(Debug)]
pub(crate) enum Walk {
    /// Every directory above the place is open.
    Reached(Reached),
    /// The walk stopped at a directory that is missing, a source, a
    /// symlink or not a directory.
    Stopped(Found),
}

/// Open the directories from `root` (canonical) to the place `rel`,
/// creating missing ones owner-only when `create`; never through a
/// symlink, never into a source.
pub(crate) fn reach(
    root: &Path,
    rel: &Rel,
    protected: &Protected,
    create: bool,
) -> io::Result<Walk> {
    let mut dirs = vec![Dir::open_root(root)?];
    let parents = rel
        .names()
        .split_last()
        .map_or(&[][..], |(_last, parents)| parents);
    for name in parents {
        let current = dirs
            .last()
            .ok_or_else(|| io::Error::other("no directory was opened"))?;
        let canonical = current.path().join(name);
        if protected.covers(&canonical) {
            return Ok(Walk::Stopped(Found::Source));
        }
        let next = match current.entry(name)? {
            Entry::Dir => current.open_dir(name)?,
            Entry::Missing if create => current.create_dir(name)?,
            Entry::Missing => return Ok(Walk::Stopped(Found::Missing)),
            Entry::Symlink => return Ok(Walk::Stopped(symlink(canonical, protected))),
            Entry::File | Entry::Other => return Ok(Walk::Stopped(Found::Blocked)),
        };
        dirs.push(next);
    }
    Ok(Walk::Reached(Reached { dirs }))
}

/// What is at `rel` under `root` (canonical), without following any
/// symlink below the root.
pub(crate) fn inspect(root: &Path, rel: &Rel, protected: &Protected) -> io::Result<Found> {
    match reach(root, rel, protected, false)? {
        Walk::Stopped(found) => Ok(found),
        Walk::Reached(reached) => look(reached.parent()?, rel.name(), protected),
    }
}

/// What `name` is in `dir`.
pub(crate) fn look(dir: &Dir, name: &str, protected: &Protected) -> io::Result<Found> {
    let canonical = dir.path().join(name);
    if protected.covers(&canonical) {
        return Ok(Found::Source);
    }
    Ok(match dir.entry(name)? {
        Entry::Missing => Found::Missing,
        Entry::Symlink => symlink(canonical, protected),
        Entry::Dir | Entry::Other => Found::Blocked,
        Entry::File => {
            let opened = dir.open_file(name)?;
            let (hash, _size) = files::hash_reader(&opened.file)?;
            Found::File {
                content: Content {
                    hash,
                    executable: opened.executable,
                },
                identity: opened.identity,
            }
        }
    })
}

/// A symlink at `path`: a source if it leads into one (the home's own
/// harness homes, linked to the sources), else left alone.
fn symlink(path: PathBuf, protected: &Protected) -> Found {
    match fs::canonicalize(&path) {
        Ok(target) if protected.covers(&target) => Found::Source,
        Ok(_) | Err(_) => Found::Symlink(path),
    }
}
