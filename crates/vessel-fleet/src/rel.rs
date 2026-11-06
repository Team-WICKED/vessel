//! [`Rel`]: where a place is below its target's root, as ordinary names
//! only.
//!
//! [Decision Log]
//! - Purpose: every path the materialiser acts on is a canonical root plus
//!   a `Rel`, so no step can name a place outside its root. `managed.json`
//!   is read back through the same type, so a crafted record
//!   (`/home/u/.claude/../.ssh/x`) is refused when it is loaded, not when
//!   it is acted on.
//! - Choice: UTF-8 names, compared exactly. Bundle paths and the harness
//!   names they are placed under are UTF-8 already; a `Rel` written to
//!   `managed.json` reads the same on every OS (`a/b/c`).
//! - [`Rel::folded`] is the key two names collide under on a filesystem
//!   that ignores case or Unicode normalisation (macOS and Windows by
//!   default): placements are told apart by it, and a file is never
//!   removed because its name differs from a placed one only that way.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A relative path of ordinary names, at least one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Rel(Vec<String>);

impl Rel {
    /// `text` split at `/`, if every segment is an ordinary name.
    #[must_use]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::from_names(text.split('/'))
    }

    /// `path`, if it is relative and made of ordinary UTF-8 names only.
    #[must_use]
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let mut names = Vec::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                return None;
            };
            names.push(name.to_str()?);
        }
        Self::from_names(names)
    }

    fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let names: Vec<String> = names.into_iter().map(str::to_owned).collect();
        (!names.is_empty() && names.iter().all(|name| ordinary(name))).then_some(Self(names))
    }

    /// The names, outermost first.
    #[must_use]
    pub(crate) fn names(&self) -> &[String] {
        &self.0
    }

    /// The last name.
    #[must_use]
    pub(crate) fn name(&self) -> &str {
        self.0.last().map_or("", String::as_str)
    }

    /// The same path with `suffix` added to its last name (an incoming
    /// version beside a file), if that is still an ordinary name.
    #[must_use]
    pub(crate) fn with_suffix(&self, suffix: &str) -> Option<Self> {
        let mut names = self.0.clone();
        let last = names.last_mut()?;
        last.push_str(suffix);
        Self::from_names(names.iter().map(String::as_str))
    }

    /// `self` followed by `more`.
    #[must_use]
    pub(crate) fn join(&self, more: &Self) -> Self {
        Self(self.0.iter().chain(&more.0).cloned().collect())
    }

    /// Whether `self` is `prefix` or below it.
    #[must_use]
    pub(crate) fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }

    /// This path below `root`.
    #[must_use]
    pub(crate) fn under(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        path.extend(&self.0);
        path
    }

    /// The key under which paths that are one file on macOS or Windows
    /// collide ([`fold`]).
    #[must_use]
    pub(crate) fn folded(&self) -> String {
        fold(&self.to_string())
    }
}

/// The key under which names that are one file on macOS or Windows
/// collide: lower case, then NFC. APFS ignores both case and Unicode
/// normalisation (`café` written as one code point or as `e` and a
/// combining accent), NTFS ignores case; one key for both reports a
/// collision wherever it would be one.
pub(crate) fn fold(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    name.to_lowercase().nfc().collect()
}

/// An ordinary name: not empty, not `.` or `..`, no separator or NUL.
fn ordinary(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

impl fmt::Display for Rel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.join("/"))
    }
}

impl Serialize for Rel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("`{text}` is not a relative path of plain names"))
        })
    }
}

/// Whether `path` is absolute and made of ordinary names only (no `.` or
/// `..`): what a canonical root looks like.
pub(crate) fn plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ordinary_names_make_a_rel() {
        assert_eq!(Rel::parse("skills/a/SKILL.md").unwrap().names().len(), 3);
        for bad in ["", "a//b", "../x", "a/./b", "a/..", "/abs", "a\\b", "a/\0"] {
            assert!(Rel::parse(bad).is_none(), "{bad:?}");
        }
        assert!(Rel::from_path(Path::new("a/../b")).is_none());
        assert!(Rel::from_path(Path::new("/a")).is_none());
        assert_eq!(
            Rel::parse("a/b").unwrap().with_suffix(".x").unwrap(),
            Rel::parse("a/b.x").unwrap()
        );
        assert!(plain_absolute(Path::new("/home/u/.claude")));
        assert!(!plain_absolute(Path::new("/home/u/.claude/../.ssh")));
        assert!(!plain_absolute(Path::new("home")));
    }

    #[test]
    fn names_one_file_on_macos_or_windows_fold_alike() {
        assert_eq!(fold("README.md"), fold("readme.md"));
        assert_eq!(fold("caf\u{e9}.md"), fold("cafe\u{301}.md"));
        assert_eq!(fold("CAFE\u{301}"), fold("caf\u{e9}"));
        assert_ne!(fold("a.md"), fold("b.md"));
    }

    #[test]
    fn a_crafted_record_path_is_refused_when_read() {
        let refused: Result<Rel, _> = serde_json::from_str("\"../.ssh/id\"");
        assert!(refused.is_err());
        let read: Rel = serde_json::from_str("\"skills/a/SKILL.md\"").unwrap();
        assert_eq!(
            serde_json::to_string(&read).unwrap(),
            "\"skills/a/SKILL.md\""
        );
    }
}
