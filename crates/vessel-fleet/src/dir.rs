//! [`Dir`]: an open directory in a target, reached from its canonical root
//! one name at a time without following a symlink.
//!
//! [Decision Log]
//! - Purpose: a place is checked, then acted on. Checking a path and then
//!   opening it again by name leaves a window in which a directory on the
//!   way can be swapped for a symlink (into a source, or `~/.ssh`), and the
//!   act lands there. Holding each directory open and naming only one
//!   entry inside it closes that window: the act happens in the directory
//!   that was checked.
//! - Choice (Unix): `openat` with `O_NOFOLLOW | O_DIRECTORY` for each
//!   directory, `fstatat(AT_SYMLINK_NOFOLLOW)` to look at an entry, files
//!   opened with `O_NOFOLLOW | O_NONBLOCK` (a FIFO put there must not hang
//!   the apply) and checked to be regular with `fstat`, and `mkdirat`,
//!   `renameat` and `unlinkat` relative to the open directory. Through
//!   `rustix`, already in the workspace, which wraps these calls safely
//!   (the workspace forbids `unsafe`).
//! - Alternatives: `cap-std` (a capability filesystem: more than this
//!   needs, and a new dependency tree); `openat` (unmaintained); paths
//!   checked with `symlink_metadata` (what F1 did; the review showed the
//!   window).
//! - Windows: no stable safe API opens a file relative to a directory
//!   handle, so the same interface works on paths, checking each entry
//!   with `symlink_metadata` (which sees symlinks and junctions) right
//!   before using it. The window stays there; creating a symlink on
//!   Windows needs a privilege or developer mode, which narrows who can
//!   use it. File identity is not available there either (see
//!   [`Identity`]).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::files::Mode;

/// What an entry of a directory is, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    /// Nothing by that name.
    Missing,
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symlink (on Windows also a junction).
    Symlink,
    /// A FIFO, socket, device or anything else.
    Other,
}

/// Which file a path leads to, independent of its name: device and inode
/// on Unix. Two names for one file (a case-insensitive filesystem, a hard
/// link) have the same identity. `None` on Windows, where stable Rust has
/// no safe way to read the file index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Identity {
    device: u64,
    inode: u64,
}

/// A regular file opened for reading.
#[derive(Debug)]
pub(crate) struct Opened {
    /// The file.
    pub(crate) file: File,
    /// Whether its owner may execute it.
    pub(crate) executable: bool,
    /// Which file it is, where that can be known.
    pub(crate) identity: Option<Identity>,
}

/// A file that is not a regular file was found where one was expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a regular file")]
pub(crate) struct NotRegular;

/// Something other than a plain directory where one was needed (Windows;
/// on Unix the open itself refuses it).
#[cfg_attr(unix, expect(dead_code, reason = "Unix refuses it in `openat`"))]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{} is not a plain directory", .0.display())]
pub(crate) struct NotADirectory(PathBuf);

/// An open directory.
#[derive(Debug)]
pub(crate) struct Dir {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    path: PathBuf,
}

impl Dir {
    /// Where it is (for messages, and for the source check: a directory
    /// reached from a canonical root without crossing a symlink has this
    /// canonical path).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::Path;

    use rustix::fs::{AtFlags, CWD, FileType, Mode as RawMode, OFlags};
    use rustix::io::Errno;

    use super::{Dir, Entry, Identity, NotRegular, Opened};
    use crate::files::Mode;

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    impl Dir {
        /// Open `path` (a canonical root, so no symlink on the way).
        pub(crate) fn open_root(path: &Path) -> io::Result<Self> {
            let fd = rustix::fs::openat(CWD, path, DIRECTORY, RawMode::empty())?;
            Ok(Self {
                fd,
                path: path.to_path_buf(),
            })
        }

        /// What `name` is here.
        pub(crate) fn entry(&self, name: &str) -> io::Result<Entry> {
            match rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => Ok(match FileType::from_raw_mode(stat.st_mode) {
                    FileType::RegularFile => Entry::File,
                    FileType::Directory => Entry::Dir,
                    FileType::Symlink => Entry::Symlink,
                    FileType::Fifo
                    | FileType::Socket
                    | FileType::CharacterDevice
                    | FileType::BlockDevice
                    | FileType::Unknown => Entry::Other,
                }),
                Err(Errno::NOENT) => Ok(Entry::Missing),
                Err(error) => Err(error.into()),
            }
        }

        /// The directory `name` here; an error if it is a symlink or not a
        /// directory (checked by the open itself).
        pub(crate) fn open_dir(&self, name: &str) -> io::Result<Self> {
            let fd = rustix::fs::openat(&self.fd, name, DIRECTORY, RawMode::empty())?;
            Ok(Self {
                fd,
                path: self.path.join(name),
            })
        }

        /// Create the directory `name` here, owner-only, and open it.
        pub(crate) fn create_dir(&self, name: &str) -> io::Result<Self> {
            rustix::fs::mkdirat(&self.fd, name, RawMode::RWXU)?;
            self.open_dir(name)
        }

        /// Open the regular file `name` here for reading; an error for a
        /// symlink or anything but a regular file.
        pub(crate) fn open_file(&self, name: &str) -> io::Result<Opened> {
            let flags = OFlags::RDONLY
                .union(OFlags::NOFOLLOW)
                .union(OFlags::NONBLOCK)
                .union(OFlags::CLOEXEC);
            let file = File::from(rustix::fs::openat(&self.fd, name, flags, RawMode::empty())?);
            let metadata = file.metadata()?;
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, NotRegular));
            }
            Ok(Opened {
                executable: metadata.permissions().mode() & 0o100 != 0,
                identity: Some(Identity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }),
                file,
            })
        }

        pub(super) fn create_new(&self, name: &str, mode: Mode) -> io::Result<File> {
            let flags = OFlags::WRONLY
                .union(OFlags::CREATE)
                .union(OFlags::EXCL)
                .union(OFlags::NOFOLLOW)
                .union(OFlags::CLOEXEC);
            let bits = match mode {
                Mode::Private => RawMode::RUSR | RawMode::WUSR,
                Mode::Executable => RawMode::RWXU,
            };
            Ok(File::from(rustix::fs::openat(&self.fd, name, flags, bits)?))
        }

        pub(super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            Ok(rustix::fs::renameat(&self.fd, from, &self.fd, to)?)
        }

        /// Remove the file (or symlink) `name` here.
        pub(crate) fn remove_file(&self, name: &str) -> io::Result<()> {
            Ok(rustix::fs::unlinkat(&self.fd, name, AtFlags::empty())?)
        }

        /// Remove the empty directory `name` here.
        pub(crate) fn remove_dir(&self, name: &str) -> io::Result<()> {
            Ok(rustix::fs::unlinkat(&self.fd, name, AtFlags::REMOVEDIR)?)
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::Path;

    use super::{Dir, Entry, NotRegular, Opened};
    use crate::files::Mode;

    fn not_a_directory(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            super::NotADirectory(path.to_path_buf()),
        )
    }

    impl Dir {
        /// Open `path` (a canonical root).
        pub(crate) fn open_root(path: &Path) -> io::Result<Self> {
            if !fs::symlink_metadata(path)?.file_type().is_dir() {
                return Err(not_a_directory(path));
            }
            Ok(Self {
                path: path.to_path_buf(),
            })
        }

        /// What `name` is here.
        pub(crate) fn entry(&self, name: &str) -> io::Result<Entry> {
            match fs::symlink_metadata(self.path.join(name)) {
                Ok(metadata) => {
                    let kind = metadata.file_type();
                    Ok(if kind.is_symlink() {
                        Entry::Symlink
                    } else if kind.is_dir() {
                        Entry::Dir
                    } else if kind.is_file() {
                        Entry::File
                    } else {
                        Entry::Other
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Entry::Missing),
                Err(error) => Err(error),
            }
        }

        /// The directory `name` here; an error if it is a symlink or not a
        /// directory.
        pub(crate) fn open_dir(&self, name: &str) -> io::Result<Self> {
            let path = self.path.join(name);
            if self.entry(name)? != Entry::Dir {
                return Err(not_a_directory(&path));
            }
            Ok(Self { path })
        }

        /// Create the directory `name` here and open it.
        pub(crate) fn create_dir(&self, name: &str) -> io::Result<Self> {
            fs::create_dir(self.path.join(name))?;
            self.open_dir(name)
        }

        /// Open the regular file `name` here for reading.
        pub(crate) fn open_file(&self, name: &str) -> io::Result<Opened> {
            if self.entry(name)? != Entry::File {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, NotRegular));
            }
            let file = File::open(self.path.join(name))?;
            if !file.metadata()?.file_type().is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, NotRegular));
            }
            Ok(Opened {
                file,
                executable: false,
                identity: None,
            })
        }

        pub(super) fn create_new(&self, name: &str, _mode: Mode) -> io::Result<File> {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.path.join(name))
        }

        pub(super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            fs::rename(self.path.join(from), self.path.join(to))
        }

        /// Remove the file `name` here.
        pub(crate) fn remove_file(&self, name: &str) -> io::Result<()> {
            fs::remove_file(self.path.join(name))
        }

        /// Remove the empty directory `name` here.
        pub(crate) fn remove_dir(&self, name: &str) -> io::Result<()> {
            fs::remove_dir(self.path.join(name))
        }
    }
}

impl Dir {
    /// Write what `fill` produces to a new temporary file here, then
    /// rename it over `name`: a crash leaves the old content or the new,
    /// and a symlink at `name` is replaced, never followed. The temporary
    /// file is removed if anything fails.
    pub(crate) fn replace<E: From<io::Error>>(
        &self,
        name: &str,
        mode: Mode,
        fill: impl FnOnce(&mut File) -> Result<(), E>,
    ) -> Result<(), E> {
        let temporary = temporary_name(name)?;
        let mut created = self.create_new(&temporary, mode)?;
        let written = fill(&mut created)
            .and_then(|()| created.sync_all().map_err(E::from))
            .and_then(|()| self.rename(&temporary, name).map_err(E::from));
        drop(created);
        if written.is_err()
            && let Err(error) = self.remove_file(&temporary)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.join(&temporary).display(),
                %error,
                "could not remove a temporary file"
            );
        }
        written
    }
}

/// `.<name>.<random>.tmp`.
fn temporary_name(name: &str) -> io::Result<String> {
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).map_err(io::Error::other)?;
    Ok(format!(".{name}.{:016x}.tmp", u64::from_le_bytes(suffix)))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn symlinks_and_special_files_are_never_followed_or_read() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("file"), "x").unwrap();
        symlink(root.join("real"), root.join("link")).unwrap();
        symlink(root.join("file"), root.join("file-link")).unwrap();
        let dir = Dir::open_root(&root).unwrap();
        assert_eq!(dir.entry("link").unwrap(), Entry::Symlink);
        assert_eq!(dir.entry("none").unwrap(), Entry::Missing);
        assert!(dir.open_dir("link").is_err());
        assert!(dir.open_file("file-link").is_err());
        assert!(dir.open_dir("real").is_ok());
        let opened = dir.open_file("file").unwrap();
        assert!(opened.identity.is_some());

        // A FIFO is refused at once instead of hanging the reader.
        let status = std::process::Command::new("mkfifo")
            .arg(root.join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(dir.entry("fifo").unwrap(), Entry::Other);
        let refused = dir.open_file("fifo").unwrap_err();
        assert!(refused.get_ref().unwrap().is::<NotRegular>());
    }

    #[test]
    fn a_replace_never_follows_a_symlink_at_its_name() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("target"), "keep").unwrap();
        symlink(root.join("target"), root.join("name")).unwrap();
        let dir = Dir::open_root(&root).unwrap();
        dir.replace::<io::Error>("name", Mode::Private, |file| {
            io::Write::write_all(file, b"new")
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("target")).unwrap(),
            "keep"
        );
        assert_eq!(std::fs::read_to_string(root.join("name")).unwrap(), "new");
        assert!(
            !std::fs::symlink_metadata(root.join("name"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "no temporary left"
        );
    }
}
