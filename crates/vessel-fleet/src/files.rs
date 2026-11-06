//! File plumbing: hashing, temporary files beside a destination, renames,
//! owner-only directories.
//!
//! Every write goes to a new temporary file in the destination's directory
//! and is renamed over it, so a crash leaves the old content or the new,
//! never half. A rename replaces the directory entry itself: if the
//! destination is a symlink, the link is replaced, never followed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use vessel_proto::BlobHash;

/// How much is read at a time while hashing or copying.
const CHUNK: usize = 64 * 1024;

/// The SHA-256 and size of everything `reader` yields.
pub(crate) fn hash_reader(mut reader: impl Read) -> io::Result<(BlobHash, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut size = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            return Ok((BlobHash::from_bytes(hasher.finalize().into()), size));
        };
        hasher.update(chunk);
        size = size.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
}

/// A path that names no file (`/`, `..`) where a file was meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a path without a file name")]
pub(crate) struct NoFileName;

/// `.<name>.<random>.tmp` beside `path`.
pub(crate) fn temporary_beside(path: &Path) -> io::Result<PathBuf> {
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).map_err(io::Error::other)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, NoFileName))?
        .to_string_lossy()
        .into_owned();
    Ok(path.with_file_name(format!(".{name}.{:016x}.tmp", u64::from_le_bytes(suffix))))
}

/// How a new file is created. Everything Vessel writes is owner-only:
/// its own state, and fleet files in harness homes alike. A source file's
/// group and other bits are not carried (a manifest has only the
/// executable bit), so a member can never make a file readable by more
/// people than the home did; a harness home belongs to one user, who is
/// the only one who needs to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 0600 on Unix.
    Private,
    /// 0700 on Unix: a script in a skill.
    Executable,
}

/// A new file at `path` (failing if one exists), with `mode`.
#[cfg_attr(
    not(unix),
    expect(unused_variables, reason = "Windows files have no mode bits to set")
)]
pub(crate) fn create_new(path: &Path, mode: Mode) -> io::Result<File> {
    let mut options = OpenOptions::new();
    let _same = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _same = options.mode(match mode {
            Mode::Private => 0o600,
            Mode::Executable => 0o700,
        });
    }
    options.open(path)
}

/// Write what `fill` produces to a new temporary file beside `path`, then
/// rename it over `path`. The temporary file is removed if anything fails.
pub(crate) fn replace_with(
    path: &Path,
    mode: Mode,
    fill: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let temporary = temporary_beside(path)?;
    let written = create_new(&temporary, mode)
        .and_then(|mut file| {
            fill(&mut file)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, path));
    if written.is_err()
        && let Err(error) = fs::remove_file(&temporary)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %temporary.display(), %error, "could not remove a temporary file");
    }
    written
}

/// [`replace_with`] for bytes in memory.
pub(crate) fn replace(path: &Path, mode: Mode, contents: &[u8]) -> io::Result<()> {
    replace_with(path, mode, |file| file.write_all(contents))
}

/// Create `dir` and its parents, owner-only (0700 on Unix) where created.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Whether the file at `path` is executable by its owner (never on
/// Windows, which has no such bit).
#[cfg_attr(
    not(unix),
    expect(unused_variables, reason = "Windows has no executable bit to read")
)]
pub(crate) fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o100 != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}
