//! Where a bundle goes on a node: [`Target`], one harness home, and what
//! is found at each place in it.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use vessel_proto::{BlobHash, BundlePath, HarnessKind};

use crate::files;

/// One harness home to write the bundle into: its root, and the names the
/// harness gives the instructions file and the skills directory under it
/// (`CLAUDE.md` and `skills` for Claude Code, `AGENTS.md` and `skills` for
/// Codex), when it has them. The names are relative paths without `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The harness home (`~/.claude`, a profile's `CLAUDE_CONFIG_DIR`).
    pub root: PathBuf,
    /// The instructions file, relative to the root.
    pub instructions: Option<PathBuf>,
    /// The skills directory, relative to the root.
    pub skills: Option<PathBuf>,
}

/// Where a bundle file goes in a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// Here.
    At(PathBuf),
    /// The harness keeps no such file (Gemini CLI has no skills
    /// directory), or the bundle path is one this version does not know:
    /// nothing to write, nothing to report.
    Unused,
    /// The target's names are not plain relative paths.
    Invalid,
}

impl Target {
    /// The target for `harness` with its home at `root`, from the
    /// harness's layout (`vessel_harness::layout`); `None` for a harness
    /// that keeps neither instructions nor skills in its home.
    ///
    /// `OpenCode` also reads `~/.claude/CLAUDE.md` and `~/.claude/skills`
    /// besides its own home, so on a node whose default Claude Code home
    /// is a target too, `OpenCode` sees the fleet's files twice; the node
    /// leaves `OpenCode` out then.
    #[must_use]
    pub fn for_harness(harness: HarnessKind, root: PathBuf) -> Option<Self> {
        let layout = vessel_harness::layout(harness);
        if layout.instructions.is_none() && layout.skills.is_none() {
            return None;
        }
        Some(Self {
            root,
            instructions: layout.instructions.map(PathBuf::from),
            skills: layout.skills.map(PathBuf::from),
        })
    }

    /// Where bundle file `path` goes: the instructions file, or a file
    /// under the skills directory. `None` when the harness keeps no such
    /// file, or the path or the target's names are unusable.
    #[must_use]
    pub fn place(&self, path: &BundlePath) -> Option<PathBuf> {
        match self.placement(path) {
            Place::At(place) => Some(place),
            Place::Unused | Place::Invalid => None,
        }
    }

    pub(crate) fn placement(&self, path: &BundlePath) -> Place {
        let names = [&self.instructions, &self.skills];
        if names
            .iter()
            .any(|name| name.as_ref().is_some_and(|name| !plain(name)))
        {
            return Place::Invalid;
        }
        if path.as_str() == crate::INSTRUCTIONS {
            return self
                .instructions
                .as_ref()
                .map_or(Place::Unused, |name| Place::At(self.root.join(name)));
        }
        let mut segments = path.segments();
        if segments.next() != Some(crate::SKILLS) {
            // Other top-level paths are reserved for later versions: a
            // member ignores what it does not know (PROTOCOL.md, 5).
            return Place::Unused;
        }
        let Some(skills) = &self.skills else {
            return Place::Unused;
        };
        let mut place = self.root.join(skills);
        for segment in segments {
            place.push(segment);
        }
        Place::At(place)
    }

    /// The directory removals may empty, never remove: the skills
    /// directory, or the root.
    pub(crate) fn skills_dir(&self) -> PathBuf {
        self.skills
            .as_ref()
            .map_or_else(|| self.root.clone(), |skills| self.root.join(skills))
    }
}

/// A relative path of ordinary names only.
fn plain(path: &Path) -> bool {
    path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// What is at a place in a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// Nothing (and maybe no parent directory either): free to write.
    Missing,
    /// A regular file, reached without any symlink below the root.
    File {
        /// Its content's hash.
        hash: BlobHash,
    },
    /// The place is, or leads into, a source the bundle is made from: the
    /// home's own harness homes linked to (or being) the source.
    Source,
    /// A symlink below the root that does not lead into a source: never
    /// followed.
    Symlink(PathBuf),
    /// Something other than a directory where one is needed, or other than
    /// a file at the place itself.
    Blocked,
}

/// Look at `place` under `root` without following any symlink below the
/// root. `sources` are canonical paths (files or directories) the bundle
/// was made from; a place in one of them is [`Found::Source`].
pub(crate) fn inspect(root: &Path, place: &Path, sources: &[PathBuf]) -> io::Result<Found> {
    let Ok(relative) = place.strip_prefix(root) else {
        return Ok(Found::Blocked);
    };
    // The root itself may be a link (a profile home kept elsewhere); it is
    // the configured place, so it is resolved once.
    let canonical_root = match fs::canonicalize(root) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == io::ErrorKind::NotFound => root.to_path_buf(),
        Err(error) => return Err(error),
    };
    let names: Vec<&OsStr> = relative.iter().collect();
    let mut walked = root.to_path_buf();
    let mut canonical = canonical_root;
    for (index, name) in names.iter().enumerate() {
        walked.push(name);
        canonical.push(name);
        let last = index.saturating_add(1) == names.len();
        let metadata = match fs::symlink_metadata(&walked) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(if in_sources(&canonical, sources) {
                    Found::Source
                } else {
                    Found::Missing
                });
            }
            Err(error) => return Err(error),
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Ok(match fs::canonicalize(&walked) {
                Ok(target) if in_sources(&target, sources) => Found::Source,
                Ok(_) | Err(_) => Found::Symlink(walked),
            });
        }
        if in_sources(&canonical, sources) {
            return Ok(Found::Source);
        }
        if last {
            if !kind.is_file() {
                return Ok(Found::Blocked);
            }
            let (hash, _size) = files::hash_file(&walked)?;
            return Ok(Found::File { hash });
        }
        if !kind.is_dir() {
            return Ok(Found::Blocked);
        }
    }
    // `place` was the root itself: not a file.
    Ok(Found::Blocked)
}

fn in_sources(path: &Path, sources: &[PathBuf]) -> bool {
    sources.iter().any(|source| path.starts_with(source))
}

/// Create the directories above `place`, under `root`, one at a time,
/// refusing to go through anything that is not a plain directory.
pub(crate) fn create_parents(root: &Path, place: &Path) -> io::Result<()> {
    let Some(parent) = place.parent() else {
        return Ok(());
    };
    let relative = parent.strip_prefix(root).map_err(|_outside| {
        io::Error::new(io::ErrorKind::InvalidInput, "a place outside its target")
    })?;
    if !root.exists() {
        files::private_dir(root)?;
    }
    let mut walked = root.to_path_buf();
    for name in relative {
        walked.push(name);
        match fs::symlink_metadata(&walked) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is not a plain directory", walked.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                files::private_dir_one(&walked)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    #[test]
    fn bundle_paths_land_under_the_harness_names() {
        let target =
            Target::for_harness(HarnessKind::ClaudeCode, PathBuf::from("/home/u/.claude")).unwrap();
        let path = |text: &str| BundlePath::from_str(text).unwrap();
        assert_eq!(
            target.place(&path("instructions.md")),
            Some(PathBuf::from("/home/u/.claude/CLAUDE.md"))
        );
        assert_eq!(
            target.place(&path("skills/review/SKILL.md")),
            Some(PathBuf::from("/home/u/.claude/skills/review/SKILL.md"))
        );
        assert_eq!(
            target.placement(&path("other/file")),
            Place::Unused,
            "reserved, ignored"
        );
        let escaping = Target {
            skills: Some(PathBuf::from("../elsewhere")),
            ..target
        };
        assert_eq!(
            escaping.placement(&path("skills/review/SKILL.md")),
            Place::Invalid
        );
        let codex =
            Target::for_harness(HarnessKind::Codex, PathBuf::from("/home/u/.codex")).unwrap();
        assert_eq!(
            codex.place(&path("instructions.md")),
            Some(PathBuf::from("/home/u/.codex/AGENTS.md"))
        );
    }

    #[test]
    fn a_harness_without_skills_takes_only_the_instructions() {
        let gemini =
            Target::for_harness(HarnessKind::Gemini, PathBuf::from("/home/u/.gemini")).unwrap();
        let skill: BundlePath = "skills/review/SKILL.md".parse().unwrap();
        assert_eq!(gemini.placement(&skill), Place::Unused);
        assert_eq!(
            gemini.place(&"instructions.md".parse().unwrap()),
            Some(PathBuf::from("/home/u/.gemini/GEMINI.md"))
        );
    }
}
