//! [`Lookup`]: where harness binaries are searched for.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An ordered list of directories searched for harness binaries, like `PATH`.
///
/// The node normally uses [`Lookup::from_env`]. Tests build one over a
/// directory holding testkit fakes, so the real resolution logic runs
/// against fake binaries. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    dirs: Arc<[PathBuf]>,
    /// `dirs` joined in the platform's `PATH` syntax, computed once.
    joined: Option<OsString>,
}

impl Lookup {
    /// Search the directories in this process's `PATH`.
    #[must_use]
    pub fn from_env() -> Self {
        let path = std::env::var_os("PATH").unwrap_or_default();
        Self::new(std::env::split_paths(&path))
    }

    /// Search exactly these directories, in order.
    ///
    /// Relative entries are dropped: resolving them against whatever the
    /// node's working directory happens to be would make the result depend
    /// on how the node was started. So is a directory that `PATH` syntax
    /// cannot express (on Unix one whose name contains `:`; on Windows one
    /// containing `"`, since `;` is quoted there): the search runs over
    /// `PATH` syntax, and keeping it would make the whole list unusable, so
    /// that every harness looked uninstalled.
    #[must_use]
    pub fn new(dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        let dirs: Arc<[PathBuf]> = dirs
            .into_iter()
            .filter(|dir| dir.is_absolute())
            .filter(|dir| {
                let searchable = std::env::join_paths([dir]).is_ok();
                if !searchable {
                    tracing::warn!(dir = %dir.display(), "skipping a search directory PATH syntax cannot express");
                }
                searchable
            })
            .collect();
        // Every entry joins on its own, so together they do too.
        let joined = std::env::join_paths(dirs.iter()).ok();
        Self { dirs, joined }
    }

    /// The directories searched, in order.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// Absolute path of the executable `binary`, if any directory has it.
    ///
    /// Does filesystem lookups; call it from a blocking context. On Windows
    /// it also tries `PATHEXT` extensions (`.exe`, `.cmd`, ...). Public so
    /// first-run setup finds the tools it offers to run (an installer, the
    /// service manager) the way harnesses are found.
    #[must_use]
    pub fn find(&self, binary: &str) -> Option<PathBuf> {
        let joined = self.joined.as_ref()?;
        which::which_in(binary, Some(joined), Path::new("/")).ok()
    }
}
