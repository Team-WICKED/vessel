//! `vesseld cleanup`: delete installed `vesseld` binaries that nothing
//! needs any more (ADR 0008).
//!
//! Versions are installed side by side in `<data_dir>/bin/` and never
//! overwritten, so without cleanup they pile up. Kept: the binary the
//! running node was started from (named in its lock record), the binary
//! running this cleanup, and the newest `keep` others by modification
//! time. A deleted binary that an older `vesseld attach` still runs keeps
//! working on Unix until that process exits. Installer temporary files
//! (`.vesseld-*.tmp`) older than an hour are leftovers of an interrupted
//! install and go too.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use vessel_store::{DataDir, NodeLock, StoreError};

/// Installed binaries are named `vesseld-<version>[-dev-<hash>][.exe]`.
const BINARY_PREFIX: &str = "vesseld-";
/// The installer writes `.vesseld-<version>.<random>.tmp` first.
const TEMP_PREFIX: &str = ".vesseld-";
const TEMP_SUFFIX: &str = ".tmp";
/// A temporary file this old is not part of an install in progress.
const TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Why cleanup could not run.
#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    /// The binary directory could not be read, or a file not removed.
    #[error("{}", path.display())]
    Io {
        /// The directory or file.
        path: PathBuf,
        /// What failed.
        #[source]
        source: io::Error,
    },
    /// Whether a node runs (and from which binary) could not be told.
    #[error("cannot tell which binary the running node uses")]
    Lock(#[source] StoreError),
}

/// Delete what is not needed; returns the deleted paths. Blocking.
///
/// # Errors
/// The directory cannot be read, the node lock cannot be checked, or a file
/// cannot be deleted (files deleted before that stay deleted).
pub fn cleanup(dir: &DataDir, keep: usize) -> Result<Vec<PathBuf>, CleanupError> {
    let bin = dir.bin_dir();
    let entries = match std::fs::read_dir(&bin) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(CleanupError::Io { path: bin, source }),
    };
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| CleanupError::Io { path, source }
    };
    let now = SystemTime::now();
    let mut binaries = Vec::new();
    let mut leftovers = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(&bin))?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(io_error(&path))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().map_err(io_error(&path))?;
        if name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX) {
            let age = now.duration_since(modified).unwrap_or_default();
            if age > TEMP_AGE {
                leftovers.push(path);
            }
        } else if name.starts_with(BINARY_PREFIX) {
            binaries.push((path, modified));
        }
    }
    // Newest first.
    binaries.sort_by(|a, b| b.1.cmp(&a.1));

    let running = NodeLock::holder(dir)
        .map_err(CleanupError::Lock)?
        .map(|owner| owner.executable);
    let this = std::env::current_exe().ok();
    let protected = [running, this];
    let doomed = binaries
        .into_iter()
        .enumerate()
        .filter(|(rank, (path, _))| {
            *rank >= keep && !protected.iter().flatten().any(|kept| same_file(path, kept))
        })
        .map(|(_, (path, _))| path)
        .chain(leftovers);
    let mut deleted = Vec::new();
    for path in doomed {
        std::fs::remove_file(&path).map_err(io_error(&path))?;
        deleted.push(path);
    }
    Ok(deleted)
}

/// Whether two paths name the same file (through symlinks too).
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use chrono::Utc;
    use vessel_store::NodeOwner;

    use super::*;

    fn touch(path: &Path, age_secs: u64) {
        let file = File::create(path).unwrap();
        let modified = SystemTime::now()
            .checked_sub(Duration::from_secs(age_secs))
            .unwrap();
        file.set_modified(modified).unwrap();
    }

    #[test]
    fn keeps_the_running_binary_and_the_newest_and_drops_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let dir = DataDir::new(temp.path());
        dir.prepare().unwrap();
        let bin = dir.bin_dir();
        std::fs::create_dir(&bin).unwrap();
        let running = bin.join("vesseld-0.1.0");
        touch(&running, 300);
        touch(&bin.join("vesseld-0.2.0"), 200);
        touch(&bin.join("vesseld-0.3.0"), 100);
        touch(&bin.join("vesseld-0.4.0-dev-0123456789abcdef"), 10);
        touch(&bin.join(".vesseld-0.5.0.1a2b.tmp"), 2 * 60 * 60);
        touch(&bin.join(".vesseld-0.5.0.3c4d.tmp"), 5);
        touch(&bin.join("notes.txt"), 999);
        let _lock = NodeLock::acquire(
            &dir,
            &NodeOwner {
                pid: 1,
                executable: running.clone(),
                version: "0.1.0".to_owned(),
                started_at: Utc::now(),
            },
        )
        .unwrap();

        let mut deleted: Vec<String> = cleanup(&dir, 1)
            .unwrap()
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        deleted.sort();
        assert_eq!(
            deleted,
            [".vesseld-0.5.0.1a2b.tmp", "vesseld-0.2.0", "vesseld-0.3.0"]
        );
        for kept in [
            "vesseld-0.1.0",
            "vesseld-0.4.0-dev-0123456789abcdef",
            ".vesseld-0.5.0.3c4d.tmp",
            "notes.txt",
        ] {
            assert!(bin.join(kept).exists(), "{kept}");
        }
    }

    #[test]
    fn nothing_installed_is_nothing_to_do() {
        let temp = tempfile::tempdir().unwrap();
        assert!(cleanup(&DataDir::new(temp.path()), 1).unwrap().is_empty());
    }
}
