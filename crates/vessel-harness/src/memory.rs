//! A profile's memory ([`memory_files`]), and renaming a harness's files
//! for another working directory ([`rebase`]): what a transfer between
//! nodes needs to know about a harness (plan F4, protocol 6).
//!
//! [Decision Log]
//! - Grounding (2026-11-06, file names in real homes, never contents):
//!   Claude Code keeps a project's memory in `projects/<cwd slug>/memory/`,
//!   so it is per project; Codex keeps `memories/` in its home, with no
//!   project in the path, so it is all or nothing. Gemini CLI and
//!   `OpenCode` have no confirmed memory layout: unsupported, not guessed.
//! - Claude Code names a project directory after its working directory
//!   (`projects/<slug>`), so a session or memory copied to another
//!   directory must be renamed for Claude to find it there; Codex files a
//!   rollout by date and records the directory inside it, so its paths
//!   stay as they are. The project's name is replaced whatever it is: a
//!   session's or one project's files are under one project, named after
//!   a spelling of the source directory that the target node cannot
//!   resolve (the directory exists only on the source).
//! - Only names are looked at; files are listed without following a
//!   symbolic link (a link in a memory directory could point anywhere),
//!   and special files are skipped.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::HarnessKind;

use crate::harnesses::descriptor;

/// Why a profile's memory cannot be listed.
#[derive(Debug, thiserror::Error)]
pub enum MemoryFilesError {
    /// The harness keeps no memory Vessel knows how to find, or none per
    /// project when one was asked for.
    #[error("{harness} memory cannot be copied{}", if *per_project { " per project" } else { "" })]
    Unsupported {
        /// Which harness.
        harness: HarnessKind,
        /// Whether a project was asked for.
        per_project: bool,
    },
    /// The working directory cannot name a project (not absolute, or not
    /// Unicode).
    #[error("working directory {} cannot name a project", cwd.display())]
    InvalidDirectory {
        /// The directory.
        cwd: PathBuf,
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

/// The memory files of `harness` under the home `home`, relative to it:
/// one project's (`project`, a working directory) or every project's. None
/// is no error (a profile that has none yet).
///
/// # Errors
/// See [`MemoryFilesError`].
pub fn memory_files(
    harness: HarnessKind,
    home: &Path,
    project: Option<&Path>,
) -> Result<Vec<PathBuf>, MemoryFilesError> {
    let Some(find) = descriptor(harness).memory else {
        return Err(MemoryFilesError::Unsupported {
            harness,
            per_project: project.is_some(),
        });
    };
    let directories = find(home, project)?;
    let mut files = Vec::new();
    for directory in directories {
        walk(home, &directory, &mut files)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// The regular files below `directory` (relative to `home`), without
/// following symbolic links.
pub(crate) fn walk(
    home: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), MemoryFilesError> {
    let full = home.join(directory);
    let entries = match fs::read_dir(&full) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(MemoryFilesError::Io { path: full, source }),
    };
    for entry in entries {
        let entry = entry.map_err(|source| MemoryFilesError::Io {
            path: full.clone(),
            source,
        })?;
        let kind = entry.file_type().map_err(|source| MemoryFilesError::Io {
            path: entry.path(),
            source,
        })?;
        let relative = directory.join(entry.file_name());
        if kind.is_dir() {
            walk(home, &relative, files)?;
        } else if kind.is_file() {
            files.push(relative);
        }
    }
    Ok(())
}

/// Why a path cannot be renamed for another working directory.
#[derive(Debug, thiserror::Error)]
#[error("working directory {} cannot name a project", cwd.display())]
pub struct RebaseError {
    /// The directory.
    pub cwd: PathBuf,
}

/// `path` (relative to a harness home: one of a session's or a memory's
/// files) as it is named for working directory `to` instead of `from`.
/// Paths that do not depend on the directory come back as they are.
///
/// # Errors
/// A directory that cannot name a project (relative, or not Unicode).
pub fn rebase(
    harness: HarnessKind,
    path: &Path,
    from: &Path,
    to: &Path,
) -> Result<PathBuf, RebaseError> {
    match descriptor(harness).rebase {
        Some(rename) => rename(path, from, to),
        None => Ok(path.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    /// An absolute directory that does not exist, so it is named as
    /// given (`/srv/...` is not absolute on Windows), and its project's
    /// name.
    fn directory(name: &str) -> (PathBuf, String) {
        let path = std::env::current_dir()
            .unwrap()
            .join("vessel-test-absent")
            .join(name);
        let slug = crate::harnesses::claude_slug(&path).unwrap();
        (path, slug)
    }

    #[test]
    fn claude_memory_is_per_project_and_links_are_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let (project, slug) = directory("app");
        let (_, other) = directory("other");
        let projects = root.join("projects");
        write(&projects.join(&slug).join("memory/MEMORY.md"));
        write(&projects.join(&slug).join("memory/notes/a.md"));
        write(&projects.join(&slug).join("abc.jsonl"));
        write(&projects.join(&other).join("memory/b.md"));
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.md"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), projects.join(&slug).join("memory/linked"))
            .unwrap();

        let one = memory_files(HarnessKind::ClaudeCode, root, Some(&project)).unwrap();
        let mine = Path::new("projects").join(&slug).join("memory");
        assert_eq!(
            one,
            [mine.join("MEMORY.md"), mine.join("notes").join("a.md")]
        );
        let all = memory_files(HarnessKind::ClaudeCode, root, None).unwrap();
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all.iter().all(|path| !path.ends_with("secret.md")));
        assert!(memory_files(HarnessKind::ClaudeCode, root, Some(Path::new("relative"))).is_err());
    }

    #[test]
    fn codex_memory_is_all_or_nothing_and_others_have_none() {
        let home = tempfile::tempdir().unwrap();
        write(&home.path().join("memories/one.md"));
        assert_eq!(
            memory_files(HarnessKind::Codex, home.path(), None).unwrap(),
            [PathBuf::from("memories/one.md")]
        );
        assert!(matches!(
            memory_files(HarnessKind::Codex, home.path(), Some(Path::new("/srv"))),
            Err(MemoryFilesError::Unsupported {
                per_project: true,
                ..
            })
        ));
        assert!(matches!(
            memory_files(HarnessKind::Gemini, home.path(), None),
            Err(MemoryFilesError::Unsupported { .. })
        ));
    }

    #[test]
    fn claude_paths_follow_the_project_and_codex_paths_stay() {
        let ((from, source), (to, target)) = (directory("work/app"), directory("u/app"));
        let renamed =
            |path: &str| rebase(HarnessKind::ClaudeCode, Path::new(path), &from, &to).unwrap();
        let projects = Path::new("projects");
        assert_eq!(
            renamed(&format!("projects/{source}/abc.jsonl")),
            projects.join(&target).join("abc.jsonl")
        );
        assert_eq!(
            renamed(&format!("projects/{source}/abc/subagents/x.jsonl")),
            projects.join(&target).join("abc/subagents/x.jsonl")
        );
        assert_eq!(
            renamed("projects/-private-srv-work-app/abc.jsonl"),
            projects.join(&target).join("abc.jsonl"),
            "a spelling of the directory only the source node could resolve"
        );
        assert_eq!(
            renamed("file-history/abc/1"),
            Path::new("file-history/abc/1"),
            "not named after the directory"
        );
        assert_eq!(renamed("projects/x"), Path::new("projects/x"), "no project");
        let rollout = Path::new("sessions/2026/11/06/rollout-x-abc.jsonl");
        assert_eq!(
            rebase(HarnessKind::Codex, rollout, &from, &to).unwrap(),
            rollout
        );
        assert!(
            rebase(
                HarnessKind::ClaudeCode,
                rollout,
                &from,
                Path::new("relative")
            )
            .is_err()
        );
    }
}
