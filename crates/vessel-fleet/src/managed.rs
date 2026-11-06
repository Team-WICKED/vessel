//! What the materialiser wrote, and the content it wrote: `managed.json`
//! (`<data_dir>/fleet/managed.json`, owner-only).
//!
//! [Decision Log]
//! - Purpose: a file whose content (hash and executable bit) is still what
//!   Vessel recorded is Vessel's to update or remove; any other content is
//!   somebody's edit. Files never recorded are never touched. Incoming
//!   versions written beside edited files are recorded too, so a stale one
//!   can be cleaned up once the conflict is settled.
//! - Keys: a record is filed under the target's canonical root and a
//!   [`Rel`] below it. Keyed by the path a profile happened to name
//!   (`~/.claude-alt`, a link to `~/.claude`), the same file had two
//!   records, and planning the profiles in another order removed it as a
//!   leftover of the other name (the review's aliased-roots case).
//! - Adopted files: a file that already held the bundle's content before
//!   Vessel came is recorded as adopted. Updates reach it, but when it
//!   leaves the bundle it is forgotten, never removed: it was the user's
//!   before it was Vessel's.
//! - Loading checks every root (absolute, no `.` or `..`) and every
//!   relative path (ordinary names only), so a crafted or damaged record
//!   cannot point a removal outside its root.
//! - Version 2. Version 1 (keyed by plain paths) was never released, so it
//!   is refused rather than converted.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vessel_proto::{BlobHash, BundleFile};

use crate::files::{self, Mode};
use crate::rel::{Rel, plain_absolute};

/// The record's format version.
const VERSION: u32 = 2;

/// A file's content as the materialiser compares it: its hash and whether
/// it is executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Content {
    /// The SHA-256 of its bytes.
    pub hash: BlobHash,
    /// Whether its owner may execute it.
    pub executable: bool,
}

impl Content {
    /// The content a bundle file stands for.
    #[must_use]
    pub const fn of(file: &BundleFile) -> Self {
        Self {
            hash: file.hash,
            executable: file.executable,
        }
    }

    /// Whether `self` and `other` are the same content. The executable bit
    /// counts only where files have one (not on Windows), so a member there
    /// does not see every script as edited.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        self.hash == other.hash && (!cfg!(unix) || self.executable == other.executable)
    }
}

/// One file the materialiser wrote or adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The hash of what it wrote.
    pub hash: BlobHash,
    /// Whether it wrote it executable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
    /// Whether the file held the bundle's content before Vessel recorded
    /// it: it is then forgotten, never removed, when it leaves the bundle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adopted: bool,
}

impl Record {
    /// The content it records.
    #[must_use]
    pub const fn content(&self) -> Content {
        Content {
            hash: self.hash,
            executable: self.executable,
        }
    }

    pub(crate) const fn new(content: Content, adopted: bool) -> Self {
        Self {
            hash: content.hash,
            executable: content.executable,
            adopted,
        }
    }
}

/// The records under one target root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Records {
    /// Files written into the target.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    files: BTreeMap<Rel, Record>,
    /// Incoming versions written beside locally edited files.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    incoming: BTreeMap<Rel, Record>,
}

impl Records {
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.incoming.is_empty()
    }
}

/// Files the materialiser wrote, by canonical target root and relative
/// path, with what it wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Managed {
    version: u32,
    #[serde(default)]
    roots: BTreeMap<PathBuf, Records>,
}

impl Default for Managed {
    fn default() -> Self {
        Self {
            version: VERSION,
            roots: BTreeMap::new(),
        }
    }
}

/// `managed.json` could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ManagedError {
    /// The file could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The file is not a record this version reads.
    #[error("{}: not a managed-files record: {message}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

impl Managed {
    /// The record at `path`; an empty one if there is none yet.
    ///
    /// # Errors
    /// The file cannot be read, or is not a record (a damaged record is
    /// an error, not an empty one: forgetting would make Vessel treat its
    /// own files as the user's, which is safe but leaves them stale).
    pub fn load(path: &Path) -> Result<Self, ManagedError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ManagedError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let invalid = |message: String| ManagedError::Invalid {
            path: path.to_owned(),
            message,
        };
        let managed: Self =
            serde_json::from_str(&text).map_err(|error| invalid(error.to_string()))?;
        if managed.version != VERSION {
            return Err(invalid(format!(
                "version {} (this build reads {VERSION})",
                managed.version
            )));
        }
        if let Some(root) = managed.roots.keys().find(|root| !plain_absolute(root)) {
            return Err(invalid(format!(
                "{} is not an absolute path of plain names",
                root.display()
            )));
        }
        Ok(managed)
    }

    /// Write the record to `path`, owner-only, replacing it atomically.
    ///
    /// # Errors
    /// The directory or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), ManagedError> {
        let io = |source| ManagedError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            files::private_dir(dir).map_err(io)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|error| ManagedError::Invalid {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        files::replace(path, Mode::Private, text.as_bytes()).map_err(io)
    }

    /// What Vessel recorded for the file at `path` (a canonical path: a
    /// target's canonical root and the file below it), if anything.
    #[must_use]
    pub fn file(&self, path: &Path) -> Option<&Record> {
        self.split(path)
            .and_then(|(root, rel)| self.file_at(root, &rel))
    }

    /// Every file Vessel recorded, by full path.
    pub fn files(&self) -> impl Iterator<Item = (PathBuf, &Record)> {
        self.roots.iter().flat_map(|(root, records)| {
            records
                .files
                .iter()
                .map(move |(rel, record)| (rel.under(root), record))
        })
    }

    /// Settle files deleted here: forget the records of recorded files at
    /// `path`, or below it if it is a directory, that are missing on disk,
    /// so the next apply writes them again. How many were forgotten (files
    /// that are there are settled by editing them, not here).
    ///
    /// `path` may be relative to nothing canonical (`~/.claude-alt/...`
    /// through a link); it is resolved through its nearest existing
    /// ancestor.
    pub fn restore(&mut self, path: &Path) -> usize {
        let Some(wanted) = resolve_missing(path) else {
            return 0;
        };
        let mut restored = 0_usize;
        for (root, records) in &mut self.roots {
            records.files.retain(|rel, _record| {
                let full = rel.under(root);
                let missing = matches!(
                    std::fs::symlink_metadata(&full),
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                );
                let chosen = full.starts_with(&wanted) && missing;
                if chosen {
                    restored = restored.saturating_add(1);
                }
                !chosen
            });
        }
        self.roots.retain(|_root, records| !records.is_empty());
        restored
    }

    /// The root `path` is under (the longest), and `path` below it.
    fn split<'a>(&'a self, path: &Path) -> Option<(&'a Path, Rel)> {
        self.roots
            .keys()
            .filter_map(|root| {
                let rel = Rel::from_path(path.strip_prefix(root).ok()?)?;
                Some((root.as_path(), rel))
            })
            .max_by_key(|(root, _rel)| root.components().count())
    }

    pub(crate) fn file_at(&self, root: &Path, rel: &Rel) -> Option<&Record> {
        self.roots.get(root)?.files.get(rel)
    }

    pub(crate) fn incoming_at(&self, root: &Path, rel: &Rel) -> Option<&Record> {
        self.roots.get(root)?.incoming.get(rel)
    }

    /// The files recorded under `root`.
    pub(crate) fn files_under(&self, root: &Path) -> Vec<(Rel, Record)> {
        self.roots.get(root).map_or_else(Vec::new, |records| {
            records
                .files
                .iter()
                .map(|(rel, record)| (rel.clone(), *record))
                .collect()
        })
    }

    /// The incoming versions recorded under `root`.
    pub(crate) fn incoming_under(&self, root: &Path) -> Vec<(Rel, Record)> {
        self.roots.get(root).map_or_else(Vec::new, |records| {
            records
                .incoming
                .iter()
                .map(|(rel, record)| (rel.clone(), *record))
                .collect()
        })
    }

    pub(crate) fn record(&mut self, root: &Path, rel: Rel, record: Record) {
        let _previous = self
            .roots
            .entry(root.to_path_buf())
            .or_default()
            .files
            .insert(rel, record);
    }

    pub(crate) fn record_incoming(&mut self, root: &Path, rel: Rel, record: Record) {
        let _previous = self
            .roots
            .entry(root.to_path_buf())
            .or_default()
            .incoming
            .insert(rel, record);
    }

    pub(crate) fn forget(&mut self, root: &Path, rel: &Rel, incoming: bool) {
        if let Some(records) = self.roots.get_mut(root) {
            let _previous = if incoming {
                records.incoming.remove(rel)
            } else {
                records.files.remove(rel)
            };
            if records.is_empty() {
                let _empty = self.roots.remove(root);
            }
        }
    }
}

/// `path` with its nearest existing ancestor made canonical.
fn resolve_missing(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(current) {
            let mut resolved = canonical;
            resolved.extend(rest.iter().rev());
            return Some(resolved);
        }
        rest.push(current.file_name()?);
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_pointing_outside_its_root_is_refused_on_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed.json");
        let hash = "00".repeat(32);
        for crafted in [
            format!(
                r#"{{"version":2,"roots":{{"/home/u/.claude":{{"files":{{"../.ssh/x":{{"hash":"{hash}"}}}}}}}}}}"#
            ),
            format!(
                r#"{{"version":2,"roots":{{"/home/u/.claude/../.ssh":{{"files":{{"x":{{"hash":"{hash}"}}}}}}}}}}"#
            ),
            r#"{"version":2,"roots":{"relative":{}}}"#.to_owned(),
            r#"{"version":1}"#.to_owned(),
        ] {
            std::fs::write(&path, &crafted).unwrap();
            assert!(Managed::load(&path).is_err(), "{crafted}");
        }
    }
}
