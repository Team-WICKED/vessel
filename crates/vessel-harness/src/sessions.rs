//! Where a harness keeps one session's conversation, and linking those
//! files into another profile's home (ADR 0011, moving a session).
//!
//! [Decision Log]
//! - Purpose: a structured session moves to another profile (another
//!   account) by resuming the same conversation there, which the harness
//!   can only do if its transcript is in the new profile's home.
//! - Grounding (2026-11-06, file names in real homes, never contents):
//!   Claude Code keeps `projects/<cwd slug>/<id>.jsonl`, a `<id>/`
//!   directory beside it (`subagents`, `tool-results`, `workflows`) and
//!   `file-history/<id>/`; memory is `projects/<cwd slug>/memory/`. Codex
//!   keeps `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`. An existing
//!   account-switching tool links exactly Claude's three, never `memory/`.
//! - Choice: hard links, not copies. Both homes then share one file, so the
//!   conversation cannot fork when the old process writes its last line, and
//!   linking any transcript takes the same time.
//! - Safety rules, enforced here rather than trusted to callers: every
//!   linked path is relative to its home, has no `..`, and never names
//!   `memory`; no symbolic link is followed on either side, so nothing
//!   outside the two homes is read or written; and an existing different
//!   file in the target is a conflict found before anything is linked, so a
//!   failed move changes nothing.
//! - Windows: hard links work on NTFS; telling "already linked" from "a
//!   different file" needs a file id that `std` exposes only on Unix, so on
//!   Windows any existing target file is reported as a conflict.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use vessel_proto::{HarnessKind, HarnessSessionId};

use crate::harnesses::descriptor;

/// The files of one harness session, relative to its home. The first is
/// the transcript; the rest are directories or files that belong to the
/// same session and exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFiles {
    entries: Vec<PathBuf>,
}

impl SessionFiles {
    /// Files of a session, transcript first (relative paths).
    pub(crate) fn new(transcript: PathBuf, extras: Vec<PathBuf>) -> Self {
        let mut entries = Vec::with_capacity(extras.len().saturating_add(1));
        entries.push(transcript);
        entries.extend(extras);
        Self { entries }
    }

    /// The conversation's transcript, relative to the home.
    #[must_use]
    pub fn transcript(&self) -> &Path {
        self.entries.first().map_or(Path::new(""), PathBuf::as_path)
    }

    /// Every entry, transcript first, relative to the home.
    #[must_use]
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }
}

/// Why a session's files could not be found.
#[derive(Debug, thiserror::Error)]
pub enum SessionFilesError {
    /// The harness keeps its sessions where Vessel cannot move them (in a
    /// database, or a layout not known yet).
    #[error("{harness} sessions cannot be moved between profiles")]
    Unsupported {
        /// Which harness.
        harness: HarnessKind,
    },
    /// The harness session id cannot name a file (separators, `..`).
    #[error("`{id}` is not a session id that can name a file")]
    InvalidId {
        /// The id as given.
        id: String,
    },
    /// The working directory cannot be turned into the harness's project
    /// name (not absolute, or not Unicode).
    #[error("working directory {} cannot name a project", cwd.display())]
    InvalidDirectory {
        /// The directory.
        cwd: PathBuf,
    },
    /// No transcript for that session in this home.
    #[error("no {harness} transcript for session {id} under {}", home.display())]
    NotFound {
        /// Which harness.
        harness: HarnessKind,
        /// The session.
        id: String,
        /// Where it was looked for.
        home: PathBuf,
    },
    /// Reading the home failed.
    #[error("cannot read {}", path.display())]
    Io {
        /// What was being read.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
}

/// The files that hold session `id` of `harness`, started in `cwd`, under
/// the harness home `home` (see [`home`](crate::home)).
///
/// # Errors
/// See [`SessionFilesError`].
pub fn session_files(
    harness: HarnessKind,
    home: &Path,
    cwd: &Path,
    id: &HarnessSessionId,
) -> Result<SessionFiles, SessionFilesError> {
    let Some(find) = descriptor(harness).sessions else {
        return Err(SessionFilesError::Unsupported { harness });
    };
    let id = id.as_str();
    if !is_file_name(id) {
        return Err(SessionFilesError::InvalidId { id: id.to_owned() });
    }
    find(home, cwd, id)?.ok_or_else(|| SessionFilesError::NotFound {
        harness,
        id: id.to_owned(),
        home: home.to_owned(),
    })
}

/// Whether `text` is safe as one path component: no separators, no `..`,
/// no leading dot, only characters harnesses use in session ids.
fn is_file_name(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `path` as metadata, without following a final symbolic link; `None` if
/// it does not exist.
pub(crate) fn entry(path: &Path) -> Result<Option<fs::Metadata>, SessionFilesError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SessionFilesError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// What linking a session into another home did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Linked {
    /// Files linked now.
    pub linked: usize,
    /// Files that were already the same file in both homes.
    pub already: usize,
    /// Entries inside the session's directories that were not linked:
    /// symbolic links (never followed), special files, and directories
    /// named `memory` (memory stays per profile).
    pub skipped: Vec<PathBuf>,
}

/// Why a session could not be linked into another home.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// An entry is not a plain relative path inside the home, or names
    /// `memory`, which stays per profile.
    #[error("{} may not be linked between profiles", path.display())]
    Refused {
        /// The entry.
        path: PathBuf,
    },
    /// A symbolic link where a directory of the home was expected: linking
    /// through it could reach outside the home.
    #[error("{} is a symbolic link; it is not followed", path.display())]
    SymbolicLink {
        /// The link.
        path: PathBuf,
    },
    /// The target home already has a different file there. Nothing was
    /// linked.
    #[error("{} already exists in the target home and is a different file", path.display())]
    Conflict {
        /// The existing file.
        path: PathBuf,
    },
    /// A file system operation failed (including linking across file
    /// systems, which hard links cannot do).
    #[error("cannot link {}", path.display())]
    Io {
        /// The path concerned.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> LinkError + '_ {
    move |source| LinkError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Hard-link `files` from the home `from` into the home `to`, creating
/// directories as needed (owner-only on Unix). Directories are linked file
/// by file. Checks everything first: a conflict or a refused path links
/// nothing.
///
/// # Errors
/// See [`LinkError`].
pub fn link_session(files: &SessionFiles, from: &Path, to: &Path) -> Result<Linked, LinkError> {
    let mut report = Linked::default();
    let mut pairs = Vec::new();
    for relative in files.entries() {
        check_relative(relative)?;
        check_no_links(from, relative)?;
        collect(from, relative, &mut pairs, &mut report.skipped)?;
    }
    let mut fresh = Vec::new();
    for relative in &pairs {
        check_no_links(to, relative.parent().unwrap_or(Path::new("")))?;
        let target = to.join(relative);
        match fs::symlink_metadata(&target) {
            Ok(_) if same_file(&from.join(relative), &target).map_err(io_error(&target))? => {
                report.already = report.already.saturating_add(1);
            }
            Ok(_) => return Err(LinkError::Conflict { path: target }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => fresh.push(relative),
            Err(source) => {
                return Err(LinkError::Io {
                    path: target,
                    source,
                });
            }
        }
    }
    for relative in fresh {
        if let Some(parent) = relative.parent() {
            create_dirs(to, parent)?;
        }
        let target = to.join(relative);
        fs::hard_link(from.join(relative), &target).map_err(io_error(&target))?;
        report.linked = report.linked.saturating_add(1);
    }
    Ok(report)
}

/// A plain relative path: no root, no `..`, no `.`, and no `memory`.
fn check_relative(relative: &Path) -> Result<(), LinkError> {
    let refused = || LinkError::Refused {
        path: relative.to_owned(),
    };
    if relative.as_os_str().is_empty() {
        return Err(refused());
    }
    for component in relative.components() {
        match component {
            Component::Normal(name) if name != "memory" => {}
            Component::Normal(_)
            | Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return Err(refused()),
        }
    }
    Ok(())
}

/// No component of `home/relative` below `home` is a symbolic link.
fn check_no_links(home: &Path, relative: &Path) -> Result<(), LinkError> {
    let mut path = home.to_owned();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LinkError::SymbolicLink { path });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(LinkError::Io { path, source }),
        }
    }
    Ok(())
}

/// Every regular file at or under `home/relative`, as relative paths;
/// symbolic links, special files and `memory` directories inside are listed
/// as skipped, never followed.
fn collect(
    home: &Path,
    relative: &Path,
    files: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), LinkError> {
    let mut pending = vec![relative.to_owned()];
    while let Some(current) = pending.pop() {
        let path = home.join(&current);
        let metadata = fs::symlink_metadata(&path).map_err(io_error(&path))?;
        let kind = metadata.file_type();
        if kind.is_symlink() || (kind.is_dir() && current.file_name() == Some("memory".as_ref())) {
            skipped.push(current);
        } else if kind.is_dir() {
            for child in fs::read_dir(&path).map_err(io_error(&path))? {
                let child = child.map_err(io_error(&path))?;
                pending.push(current.join(child.file_name()));
            }
        } else if kind.is_file() {
            files.push(current);
        } else {
            skipped.push(current);
        }
    }
    Ok(())
}

/// Create `home/relative` and its missing parents, refusing to go through
/// a symbolic link.
fn create_dirs(home: &Path, relative: &Path) -> Result<(), LinkError> {
    let mut path = home.to_owned();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LinkError::SymbolicLink { path });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                private_dir(&path).map_err(io_error(&path))?;
            }
            Err(source) => return Err(LinkError::Io { path, source }),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// Whether two paths are the same file (one inode).
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let (a, b) = (fs::symlink_metadata(a)?, fs::symlink_metadata(b)?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

/// Without a stable file id on Windows, an existing file is never assumed
/// to be the same one.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Unix version"
)]
fn same_file(_a: &Path, _b: &Path) -> io::Result<bool> {
    Ok(false)
}
