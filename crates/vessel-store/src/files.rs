//! Files and modes in the data directory: small files written once and then
//! only read (`node.id`, `token`), and keeping the directory and the event
//! log owner-only.
//!
//! [Decision Log]
//! - Purpose: session logs hold whole conversations and terminal output, and
//!   the token is a credential; other local users must not read either.
//! - Unix: the directory is 0700 and every file Vessel creates is 0600. A
//!   directory that already exists (a `--data-dir` someone made, or one left
//!   0775 by an earlier version) has group and other access removed when
//!   the node prepares it; changing a mode needs ownership, so a directory
//!   another user owns is refused instead. `SQLite` creates its `-wal` and
//!   `-shm` files with the database file's mode, so creating `vessel.db`
//!   0600 before `SQLite` opens it covers all three.
//! - Windows: there are no mode bits. Files inherit the directory's ACL,
//!   which under the user's profile (the default `~/.vessel`) admits only
//!   that user; Vessel sets no ACLs itself, so a data directory elsewhere
//!   gets whatever its parent grants.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Create `path` holding `contents`, owner-only, unless it already exists.
/// Returns whether this call created it.
///
/// The file is written under a temporary name and then hard-linked into
/// place, so a concurrent reader (or a second node starting at the same
/// time) sees either no file or the complete one, never a partial write,
/// and exactly one of two racing creators wins.
pub(crate) fn create_once(path: &Path, contents: &[u8]) -> io::Result<bool> {
    let temporary = temporary_name(path)?;
    let written = write_new(&temporary, contents);
    let linked = written.and_then(|()| std::fs::hard_link(&temporary, path));
    if let Err(error) = std::fs::remove_file(&temporary)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %temporary.display(), %error, "could not remove temporary file");
    }
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn temporary_name(path: &Path) -> io::Result<PathBuf> {
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).map_err(io::Error::other)?;
    let suffix = u64::from_le_bytes(suffix);
    let name = path
        .file_name()
        .map(|name| format!("{}.tmp-{suffix:016x}", name.to_string_lossy()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    Ok(path.with_file_name(name))
}

fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = open_new(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// A new owner-only file (0600 on Unix; see the module docs for Windows).
#[cfg(unix)]
fn open_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Replace `path` with `contents`, owner-only, atomically: readers see the
/// old file or the new one, never a partial write.
pub(crate) fn replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    let temporary = temporary_name(path)?;
    let written = write_new(&temporary, contents).and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err()
        && let Err(error) = std::fs::remove_file(&temporary)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %temporary.display(), %error, "could not remove temporary file");
    }
    written
}

/// Open `path` for reading and writing, creating it owner-only if it is
/// missing; existing contents are kept.
pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    create_private_file(path)?;
    OpenOptions::new().read(true).write(true).open(path)
}

/// Open `path` for appending, creating it owner-only if it is missing.
pub(crate) fn open_append_private(path: &Path) -> io::Result<File> {
    create_private_file(path)?;
    OpenOptions::new().append(true).open(path)
}

/// Create `path` as an empty owner-only file unless it exists.
pub(crate) fn create_private_file(path: &Path) -> io::Result<()> {
    match open_new(path) {
        Ok(_file) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Make the event log owner-only before `SQLite` opens it: create
/// `database` empty and 0600 if it is missing (`SQLite` takes an empty file
/// as an empty database, and creates its `-wal` and `-shm` files with the
/// database file's mode), and remove group and other access from all three
/// if an earlier version left them readable. A file whose mode cannot be
/// changed is only logged: the 0700 directory still keeps others out.
pub(crate) fn private_database(database: &Path) -> io::Result<()> {
    create_private_file(database)?;
    for path in [
        database.to_path_buf(),
        sidecar(database, "-wal"),
        sidecar(database, "-shm"),
    ] {
        match make_private(&path) {
            Ok(Privacy::Private) => {}
            Ok(Privacy::Tightened { from }) => tracing::warn!(
                path = %path.display(),
                mode = %format!("{from:o}"),
                "the event log was readable by other users; made it owner-only (0600)"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "cannot make the event log owner-only"
            ),
        }
    }
    Ok(())
}

/// `SQLite`'s name for a file next to `database` (`vessel.db-wal`).
fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// What [`make_private`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Privacy {
    /// Already owner-only (always, on Windows).
    Private,
    /// Group or other access was removed; the old mode.
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only Unix has mode bits to tighten")
    )]
    Tightened { from: u32 },
}

/// Remove group and other access from `path` (Unix). Changing a mode needs
/// ownership, so this fails with "permission denied" for a path another
/// user owns.
#[cfg(unix)]
pub(crate) fn make_private(path: &Path) -> io::Result<Privacy> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
    let group_and_others = mode & 0o077;
    if group_and_others == 0 {
        return Ok(Privacy::Private);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700))?;
    Ok(Privacy::Tightened { from: mode })
}

/// Windows has no mode bits (see the module docs).
#[cfg(not(unix))]
pub(crate) fn make_private(path: &Path) -> io::Result<Privacy> {
    std::fs::metadata(path).map(|_metadata| Privacy::Private)
}

/// Whether users other than the owner can read `path` (Unix only; always
/// `false` elsewhere, see the module docs).
#[cfg(unix)]
pub(crate) fn readable_by_others(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = std::fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 != 0)
}

#[cfg(not(unix))]
pub(crate) fn readable_by_others(path: &Path) -> io::Result<bool> {
    std::fs::metadata(path).map(|_metadata| false)
}

/// Create the directory (and parents) if missing; owner-only on Unix.
#[cfg(unix)]
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(not(unix))]
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}
