//! Small files in the data directory that are written once and then only
//! read (`node.id`, `token`).

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Who may read a file this module creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Anyone the directory lets in.
    Shared,
    /// The owner only (mode 0600 on Unix). On Windows the file inherits the
    /// directory's ACL, which for a directory under the user profile is the
    /// user only; Vessel does not set ACLs itself.
    Private,
}

/// Create `path` holding `contents`, unless it already exists. Returns
/// whether this call created it.
///
/// The file is written under a temporary name and then hard-linked into
/// place, so a concurrent reader (or a second node starting at the same
/// time) sees either no file or the complete one, never a partial write,
/// and exactly one of two racing creators wins.
pub(crate) fn create_once(path: &Path, contents: &[u8], access: Access) -> io::Result<bool> {
    let temporary = temporary_name(path)?;
    let written = write_new(&temporary, contents, access);
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

fn write_new(path: &Path, contents: &[u8], access: Access) -> io::Result<()> {
    let mut file = open_new(path, access)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(unix)]
fn open_new(path: &Path, access: Access) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mode = match access {
        Access::Shared => 0o644,
        Access::Private => 0o600,
    };
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

#[cfg(not(unix))]
fn open_new(path: &Path, _access: Access) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Whether users other than the owner can read `path` (Unix only; always
/// `false` elsewhere, see [`Access::Private`]).
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
