//! Where a bundle goes on a node: [`Target`], one harness home, and what
//! is found at each place in it.

use std::path::{Path, PathBuf};

use vessel_proto::{BundlePath, HarnessKind};

use crate::rel::{Rel, fold};

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
    /// Here, below the target's root.
    At(Rel),
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
            Place::At(rel) => Some(rel.under(&self.root)),
            Place::Unused | Place::Invalid => None,
        }
    }

    pub(crate) fn placement(&self, path: &BundlePath) -> Place {
        // A name that is not a plain relative path makes the target
        // unusable, whichever file is placed.
        let name = |name: &Option<PathBuf>| match name.as_deref() {
            None => Some(None),
            Some(name) => Rel::from_path(name).map(Some),
        };
        let (Some(instructions), Some(skills)) = (name(&self.instructions), name(&self.skills))
        else {
            return Place::Invalid;
        };
        if path.as_str() == crate::INSTRUCTIONS {
            return instructions.map_or(Place::Unused, Place::At);
        }
        let mut segments = path.segments();
        if segments.next() != Some(crate::SKILLS) {
            // Other top-level paths are reserved for later versions: a
            // member ignores what it does not know (PROTOCOL.md, 5).
            return Place::Unused;
        }
        let Some(skills) = skills else {
            return Place::Unused;
        };
        let rest = segments.collect::<Vec<_>>().join("/");
        // Members write incoming versions under this suffix.
        if fold(&rest).ends_with(crate::INCOMING_SUFFIX) {
            return Place::Invalid;
        }
        Rel::parse(&rest).map_or(Place::Invalid, |rest| Place::At(skills.join(&rest)))
    }

    /// The skills directory below the root, if the harness has one: the
    /// directory removals may empty, never remove.
    pub(crate) fn skills_rel(&self) -> Option<Rel> {
        self.skills.as_deref().and_then(Rel::from_path)
    }

    /// The root as given (for messages).
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
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
