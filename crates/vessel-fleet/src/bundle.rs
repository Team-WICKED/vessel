//! Making a bundle from the home's sources: [`bundle`].
//!
//! [Decision Log]
//! - Purpose: the home turns its instructions file and skills directory
//!   (`fleet.toml`'s sources) into a manifest and a blob store members
//!   fetch from.
//! - Skills: each directory under the skills source with a `SKILL.md` is a
//!   skill, bundled as `skills/<name>/...` with `<name>` from its
//!   frontmatter. Files at the top of the skills directory are not skills,
//!   and dot directories there are a harness's own (Codex's `.system`).
//!   A directory without a usable name, or with a name another directory
//!   already took (the first in name order wins), is left out and
//!   reported, never guessed at.
//! - `skip` globs (`*` and `?`, within one name) leave out anything whose
//!   name matches: a skill directory or its `SKILL.md` name (skills another
//!   tool manages, such as `paseo*`), or any file or directory inside a
//!   skill (`__pycache__`). Configured skips are not reported.
//! - Symlinks in the sources are followed only while they stay inside the
//!   skills directory: a link that leads elsewhere (to `~/.ssh`, by
//!   mistake) would publish that file to every member, so it is left out
//!   and reported. The instructions file is named in `fleet.toml` and is
//!   read wherever it points.
//! - A missing or unreadable source fails the whole bundle, and the home
//!   keeps publishing the previous one: a bundle without its instructions
//!   or skills would make every member remove theirs.
//! - The generation goes up only when the files or the MCP servers change,
//!   so bundling again on a timer or on start costs members nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use vessel_proto::{BundleFile, BundlePath, Generation, Manifest, McpDefinitions};

use crate::blobs::BlobStore;
use crate::files;
use crate::skill::{SkillNameError, skill_name};

/// The largest file a bundle carries; larger ones are left out.
pub const MAX_FILE: u64 = 16 * 1024 * 1024;

/// What the home bundles (`fleet.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// The instructions file (`~/.codex/AGENTS.md`).
    pub instructions: Option<PathBuf>,
    /// The skills directory (`~/.codex/skills`).
    pub skills: Option<PathBuf>,
    /// Names to leave out.
    pub skip: Vec<SkipGlob>,
    /// MCP server definitions, carried as they are.
    pub mcp: McpDefinitions,
}

impl Sources {
    /// The source paths on this node, for [`plan`](crate::plan()): targets
    /// in them, or linked to them, are left alone.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.instructions
            .iter()
            .chain(self.skills.iter())
            .cloned()
            .collect()
    }
}

/// A name pattern: `*` matches any run of characters, `?` any one, within
/// one name (no `/`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkipGlob(String);

/// Text that is not a [`SkipGlob`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a skip pattern (a name with `*` and `?`, without `/`)")]
pub struct InvalidSkipGlob(String);

impl FromStr for SkipGlob {
    type Err = InvalidSkipGlob;

    fn from_str(text: &str) -> Result<Self, InvalidSkipGlob> {
        if text.is_empty() || text.contains(['/', '\\']) {
            return Err(InvalidSkipGlob(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for SkipGlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl SkipGlob {
    /// Whether `name` matches.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        let pattern: Vec<char> = self.0.chars().collect();
        let name: Vec<char> = name.chars().collect();
        // Greedy match with one backtrack point for the last `*`.
        let (mut p, mut n) = (0_usize, 0_usize);
        let mut star: Option<(usize, usize)> = None;
        while n < name.len() {
            match pattern.get(p) {
                Some('*') => {
                    star = Some((p, n));
                    p = p.saturating_add(1);
                }
                Some(&c) if c == '?' || Some(&c) == name.get(n) => {
                    p = p.saturating_add(1);
                    n = n.saturating_add(1);
                }
                Some(_) | None => match star {
                    Some((star_p, star_n)) => {
                        p = star_p.saturating_add(1);
                        n = star_n.saturating_add(1);
                        star = Some((star_p, n));
                    }
                    None => return false,
                },
            }
        }
        pattern
            .get(p..)
            .is_some_and(|rest| rest.iter().all(|&c| c == '*'))
    }
}

/// Something in the sources that was left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceProblem {
    /// A skill directory without a `SKILL.md`.
    #[error("{}: no SKILL.md", .0.display())]
    NoSkillFile(PathBuf),
    /// A `SKILL.md` without a usable name.
    #[error("{}: {error}", dir.display())]
    SkillName {
        /// The skill directory.
        dir: PathBuf,
        /// Why.
        error: SkillNameError,
    },
    /// Two skills with one name; the first (in directory name order) is
    /// bundled.
    #[error("{}: skill `{name}` is already {}", dir.display(), kept.display())]
    DuplicateSkill {
        /// The name.
        name: String,
        /// The directory left out.
        dir: PathBuf,
        /// The directory bundled.
        kept: PathBuf,
    },
    /// A symlink leading out of the skills directory.
    #[error("{}: a symlink out of the skills directory", .0.display())]
    Symlink(PathBuf),
    /// A file over [`MAX_FILE`].
    #[error("{}: {size} bytes is over the bundle's limit", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
        /// Its size.
        size: u64,
    },
    /// A name that cannot be a bundle path (not valid on every OS, or not
    /// UTF-8).
    #[error("{}: not a name every system can hold", .0.display())]
    BadName(PathBuf),
}

/// A bundle made from the sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundled {
    /// The manifest; its blobs are in the store.
    pub manifest: Manifest,
    /// Whether it differs from the previous one (and so has a new
    /// generation).
    pub changed: bool,
    /// What was left out.
    pub problems: Vec<SourceProblem>,
}

/// The sources could not be bundled.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// A source could not be read.
    #[error("{}: {source}", path.display())]
    Read {
        /// The file or directory.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// A file could not be put in the blob store.
    #[error("the blob store: {0}")]
    Store(#[source] io::Error),
    /// The generation number ran out.
    #[error("the bundle generation ran out")]
    Generation,
}

/// Bundle `sources` into `blobs`. `previous` is the manifest published
/// before, if any: the generation stays the same when nothing changed.
///
/// # Errors
/// A source is missing or unreadable, or the store cannot be written; see
/// [`BundleError`]. What is only left out is in [`Bundled::problems`].
pub fn bundle(
    sources: &Sources,
    blobs: &BlobStore,
    previous: Option<&Manifest>,
) -> Result<Bundled, BundleError> {
    let mut builder = Builder {
        blobs,
        skip: &sources.skip,
        files: BTreeMap::new(),
        problems: Vec::new(),
        skills_root: None,
        visited: BTreeSet::new(),
    };
    if let Some(instructions) = &sources.instructions {
        let path = bundle_path(crate::INSTRUCTIONS)
            .ok_or_else(|| read_error(instructions, invalid("no bundle path")))?;
        builder.add_file(instructions, path, true)?;
    }
    if let Some(skills) = &sources.skills {
        builder.add_skills(skills)?;
    }
    let Builder {
        files, problems, ..
    } = builder;
    let unchanged =
        previous.filter(|previous| previous.files == files && previous.mcp == sources.mcp);
    let generation = match (unchanged, previous) {
        (Some(previous), _) => previous.generation,
        (None, Some(previous)) => previous.generation.next().ok_or(BundleError::Generation)?,
        (None, None) => Generation::new(1),
    };
    Ok(Bundled {
        manifest: Manifest {
            generation,
            files,
            mcp: sources.mcp.clone(),
        },
        changed: unchanged.is_none(),
        problems,
    })
}

struct Builder<'a> {
    blobs: &'a BlobStore,
    skip: &'a [SkipGlob],
    files: BTreeMap<BundlePath, BundleFile>,
    problems: Vec<SourceProblem>,
    /// The canonical skills directory; symlinks may lead only inside it.
    skills_root: Option<PathBuf>,
    /// Directories already walked, by canonical path (symlink loops).
    visited: BTreeSet<PathBuf>,
}

impl Builder<'_> {
    fn skipped(&self, name: &str) -> bool {
        self.skip.iter().any(|glob| glob.matches(name))
    }

    /// Bundle the file at `path` (following symlinks only if `follow`).
    fn add_file(
        &mut self,
        path: &Path,
        bundle: BundlePath,
        follow: bool,
    ) -> Result<(), BundleError> {
        let metadata = if follow {
            fs::metadata(path)
        } else {
            fs::symlink_metadata(path)
        }
        .map_err(|source| read_error(path, source))?;
        if metadata.len() > MAX_FILE {
            self.problems.push(SourceProblem::TooLarge {
                path: path.to_path_buf(),
                size: metadata.len(),
            });
            return Ok(());
        }
        let (hash, size) = self.blobs.insert_file(path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound
                || source.kind() == io::ErrorKind::PermissionDenied
            {
                read_error(path, source)
            } else {
                BundleError::Store(source)
            }
        })?;
        let _previous = self.files.insert(
            bundle,
            BundleFile {
                hash,
                size,
                executable: files::is_executable(&metadata),
            },
        );
        Ok(())
    }

    fn add_skills(&mut self, skills: &Path) -> Result<(), BundleError> {
        let root = fs::canonicalize(skills).map_err(|source| read_error(skills, source))?;
        self.skills_root = Some(root.clone());
        let _first = self.visited.insert(root.clone());
        let mut names: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (name, path) in sorted_entries(&root)? {
            // Dot directories are a harness's own (Codex's `.system`).
            if name.starts_with('.') || self.skipped(&name) {
                continue;
            }
            let Some(dir) = self.follow(&path)? else {
                continue;
            };
            if !dir.is_dir() {
                continue;
            }
            let text = match fs::read_to_string(dir.join("SKILL.md")) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.problems.push(SourceProblem::NoSkillFile(path));
                    continue;
                }
                Err(source) => return Err(read_error(&dir.join("SKILL.md"), source)),
            };
            let skill = match skill_name(&text) {
                Ok(skill) => skill,
                Err(error) => {
                    self.problems
                        .push(SourceProblem::SkillName { dir: path, error });
                    continue;
                }
            };
            if self.skipped(&skill) {
                continue;
            }
            if let Some(kept) = names.get(&skill) {
                self.problems.push(SourceProblem::DuplicateSkill {
                    name: skill,
                    dir: path,
                    kept: kept.clone(),
                });
                continue;
            }
            let _new = names.insert(skill.clone(), path);
            self.add_tree(&dir, &format!("{}/{skill}", crate::SKILLS))?;
        }
        Ok(())
    }

    /// Bundle everything under `dir` as `prefix/...`.
    fn add_tree(&mut self, dir: &Path, prefix: &str) -> Result<(), BundleError> {
        let canonical = fs::canonicalize(dir).map_err(|source| read_error(dir, source))?;
        if !self.visited.insert(canonical) {
            // Reached again through a link (or a loop of links): walked
            // once is enough.
            return Ok(());
        }
        for (name, path) in sorted_entries(dir)? {
            if self.skipped(&name) {
                continue;
            }
            let joined = format!("{prefix}/{name}");
            let Some(bundle) = bundle_path(&joined) else {
                self.problems.push(SourceProblem::BadName(path));
                continue;
            };
            let Some(resolved) = self.follow(&path)? else {
                continue;
            };
            if resolved.is_dir() {
                self.add_tree(&resolved, &joined)?;
            } else if resolved.is_file() {
                self.add_file(&resolved, bundle, false)?;
            }
        }
        Ok(())
    }

    /// `path` itself, or where its symlink leads if that is inside the
    /// skills directory; `None` (reported) for a link leading out.
    fn follow(&mut self, path: &Path) -> Result<Option<PathBuf>, BundleError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| read_error(path, source))?;
        if !metadata.file_type().is_symlink() {
            return Ok(Some(path.to_path_buf()));
        }
        let inside = fs::canonicalize(path).ok().filter(|target| {
            self.skills_root
                .as_ref()
                .is_some_and(|root| target.starts_with(root))
        });
        if inside.is_none() {
            self.problems
                .push(SourceProblem::Symlink(path.to_path_buf()));
        }
        Ok(inside)
    }
}

/// The entries of `dir` with UTF-8 names, by name.
fn sorted_entries(dir: &Path) -> Result<Vec<(String, PathBuf)>, BundleError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|source| read_error(dir, source))? {
        let entry = entry.map_err(|source| read_error(dir, source))?;
        if let Ok(name) = entry.file_name().into_string() {
            entries.push((name, entry.path()));
        }
    }
    entries.sort();
    Ok(entries)
}

fn bundle_path(text: &str) -> Option<BundlePath> {
    text.parse().ok()
}

fn read_error(path: &Path, source: io::Error) -> BundleError {
    BundleError::Read {
        path: path.to_path_buf(),
        source,
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(text: &str) -> SkipGlob {
        text.parse().unwrap()
    }

    #[test]
    fn skip_globs_match_one_name() {
        assert!(glob("paseo*").matches("paseo"));
        assert!(glob("paseo*").matches("paseo-advisor"));
        assert!(!glob("paseo*").matches("my-paseo"));
        assert!(glob(".system").matches(".system"));
        assert!(!glob(".system").matches(".systems"));
        assert!(glob("*cache*").matches("__pycache__"));
        assert!(glob("a?c").matches("abc"));
        assert!(!glob("a?c").matches("ac"));
        assert!(glob("*").matches(""));
        assert!(glob("*.md").matches("x.y.md"));
        assert!(!glob("*.md").matches("x.mdx"));
        assert!("a/b".parse::<SkipGlob>().is_err());
        assert!("".parse::<SkipGlob>().is_err());
    }
}
