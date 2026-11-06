//! Transfers (protocol 6, plan F4): files under a harness home copied into
//! a staging blob store ([`snapshot`]), and a received set written into
//! another home ([`place`]).
//!
//! [Decision Log]
//! - Purpose: a session's conversation or a profile's memory goes to
//!   another profile or node as files. Both are "a set of files under a
//!   harness home"; only which files and where they land differ, so one
//!   mechanism serves both, on the bundle's own shapes (`BundlePath` to
//!   `BundleFile`, blobs by SHA-256 read in chunks): `transfer.read` is
//!   `fleet.blob` on another store.
//! - A snapshot, not the live files: a transcript is appended to while it
//!   is read. Each file is copied into the staging store when the transfer
//!   opens, which fixes its hash, so reads can resume and be checked.
//!   Copied, not hard-linked: an append to a link would change the
//!   snapshot too. The cost is the disk it takes until it is closed or
//!   expires.
//! - Read without following links: every file is opened through
//!   directories held open from the canonical home (`dir`, as the
//!   materialiser does), so a symbolic link swapped in after the files
//!   were listed is not followed, and a FIFO is not opened (it would hang
//!   the export). Such a file is left out and listed in
//!   [`Snapshot::skipped`].
//! - Limits as each file is read, since a live file can grow after it was
//!   listed: the per-file limit by reading at most one byte more than it,
//!   the total as files are added. Over either is an error naming the limit
//!   and the size (protocol 6: `invalid_request`); dropping the staging
//!   store is the caller's.
//! - Placing never overwrites. Every place is looked at first, through
//!   directories reached without following a link. In [`OnConflict::Refuse`]
//!   (sessions) an existing file with other content, or anything other than
//!   a file there, refuses the whole set before anything is written: a
//!   conversation half replaced would be neither. In [`OnConflict::Beside`]
//!   (memory) the incoming version of such a file is written beside it as
//!   `<name>.vessel-incoming`, as the materialiser does, and the existing
//!   one is kept. A directory on the way that is a link, or not a
//!   directory, refuses the set in both modes. Each write looks once more
//!   right before it acts (a place filled since is `Changed`, and what was
//!   written until then stays: whole files, each checked against its hash),
//!   and goes through a temporary file renamed into place, owner-only.
//! - Alternatives: hard links (`link_session`, a move between profiles of
//!   one node; impossible across nodes); an archive stream (another format,
//!   no deduplication, no resuming); reading the live files at
//!   `transfer.read` (hashes would change under the reader).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::{
    BlobHash, BundleFile, BundlePath, InvalidBundlePath, MAX_TRANSFER_FILE, MAX_TRANSFER_TOTAL,
};

use crate::apply::{ApplyError, write_blob};
use crate::blobs::BlobStore;
use crate::dir::Entry;
use crate::place::{Found, Protected, Walk, look, reach};
use crate::rel::Rel;

/// The most one transfer may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes in one file.
    pub file: u64,
    /// Bytes in all the files together.
    pub total: u64,
}

impl Limits {
    /// Protocol 6's: `MAX_TRANSFER_FILE` and `MAX_TRANSFER_TOTAL`.
    pub const PROTOCOL: Self = Self {
        file: MAX_TRANSFER_FILE,
        total: MAX_TRANSFER_TOTAL,
    };
}

/// A set of files copied into a staging store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The files, by their place under the home they came from.
    pub files: BTreeMap<BundlePath, BundleFile>,
    /// Files left out: gone since they were listed, or no longer a regular
    /// file reached without a link.
    pub skipped: Vec<PathBuf>,
}

/// Why files could not be snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A path that is not a plain relative path, or not valid as a file
    /// name on every OS Vessel runs on (so it could not be placed on some
    /// target).
    #[error("{} cannot be carried to another node{}", path.display(), reason.as_ref().map(|reason| format!(": {reason}")).unwrap_or_default())]
    Path {
        /// The path.
        path: PathBuf,
        /// The rule it breaks, when it is relative and Unicode.
        reason: Option<InvalidBundlePath>,
    },
    /// One file is over the per-file limit.
    #[error("{path} is {size} bytes, over the limit of {limit} bytes for one file in a transfer")]
    FileTooLarge {
        /// The file.
        path: BundlePath,
        /// Its size, or the limit plus one when it grew while being read.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// The files together are over the per-transfer limit.
    #[error(
        "the files come to at least {size} bytes, over the limit of {limit} bytes for a transfer"
    )]
    TotalTooLarge {
        /// How many bytes they came to when the limit was passed.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// Reading the home or writing the store failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// What was being read or written.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
}

impl SnapshotError {
    /// Whether the files are over a limit (the caller's request, not the
    /// node's failure).
    #[must_use]
    pub const fn is_limit(&self) -> bool {
        matches!(self, Self::FileTooLarge { .. } | Self::TotalTooLarge { .. })
    }
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> SnapshotError + '_ {
    move |source| SnapshotError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Copy `files` (regular files, relative to `home`) into `blobs`, within
/// `limits`. Nothing below `home` is reached through a symbolic link.
///
/// # Errors
/// See [`SnapshotError`]. Blobs copied before an error stay in `blobs`.
pub fn snapshot(
    home: &Path,
    files: &[PathBuf],
    blobs: &BlobStore,
    limits: Limits,
) -> Result<Snapshot, SnapshotError> {
    let root = fs::canonicalize(home).map_err(io_at(home))?;
    let none = Protected::default();
    let mut snapshot = Snapshot::default();
    let mut total = 0_u64;
    for relative in files {
        let refuse = |reason| SnapshotError::Path {
            path: relative.clone(),
            reason,
        };
        let rel = Rel::from_path(relative).ok_or_else(|| refuse(None))?;
        let path: BundlePath = rel
            .to_string()
            .parse()
            .map_err(|error| refuse(Some(error)))?;
        let full = rel.under(&root);
        let Walk::Reached(reached) = reach(&root, &rel, &none, false).map_err(io_at(&full))? else {
            snapshot.skipped.push(relative.clone());
            continue;
        };
        let dir = reached.parent();
        if dir.entry(rel.name()).map_err(io_at(&full))? != Entry::File {
            snapshot.skipped.push(relative.clone());
            continue;
        }
        let opened = dir.open_file(rel.name()).map_err(io_at(&full))?;
        let listed = opened.file.metadata().map_err(io_at(&full))?.len();
        let too_large = |size| SnapshotError::FileTooLarge {
            path: path.clone(),
            size,
            limit: limits.file,
        };
        if listed > limits.file {
            return Err(too_large(listed));
        }
        let (hash, size) = blobs
            .insert_limited(&opened.file, limits.file)
            .map_err(io_at(&full))?
            .ok_or_else(|| too_large(limits.file.saturating_add(1)))?;
        total = total.saturating_add(size);
        if total > limits.total {
            return Err(SnapshotError::TotalTooLarge {
                size: total,
                limit: limits.total,
            });
        }
        let file = BundleFile {
            hash,
            size,
            executable: opened.executable,
        };
        let _previous = snapshot.files.insert(path, file);
    }
    Ok(snapshot)
}

/// What to do where a different file is already in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Write nothing at all (a session).
    Refuse,
    /// Keep it and write the incoming version beside it (memory).
    Beside,
}

/// One file of a received set, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    /// Its path in the transfer (what is reported).
    pub bundle: BundlePath,
    /// Where it goes, relative to the target home.
    pub place: PathBuf,
    /// Its content (in the store).
    pub file: BundleFile,
}

/// What placing a set did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placed {
    /// Files written where there were none.
    pub written: u32,
    /// Files already in place with the same content.
    pub same: u32,
    /// Files whose incoming version was written beside a different one.
    pub beside: Vec<BundlePath>,
}

/// Why a set could not be placed.
#[derive(Debug, thiserror::Error)]
pub enum PlaceError {
    /// A place that is not a plain relative path.
    #[error("{} is not a plain relative path", path.display())]
    Path {
        /// The place.
        path: PathBuf,
    },
    /// A different file is in place (in [`OnConflict::Refuse`]). Nothing
    /// was written.
    #[error("{} already exists with other content; nothing was written", path.display())]
    Conflict {
        /// The existing file.
        path: PathBuf,
    },
    /// A symbolic link, or something other than a directory, on the way to
    /// a place; or something other than a regular file at the place or at
    /// its incoming name. Never followed; nothing was written.
    #[error("{} is a symbolic link or not a plain file or directory; nothing was written", path.display())]
    Blocked {
        /// Where.
        path: PathBuf,
    },
    /// A place changed between looking and writing; the files written
    /// before it stay.
    #[error("{} changed while the files were placed; left alone", path.display())]
    Changed {
        /// The place.
        path: PathBuf,
    },
    /// A blob does not hash to its name; it was removed from the store.
    #[error("blob {expected} in the store is damaged (it hashes to {found}); removed")]
    DamagedBlob {
        /// The blob.
        expected: BlobHash,
        /// What it hashes to.
        found: BlobHash,
    },
    /// Reading or writing failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// Where.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
}

/// A place decided on: write `file` at `rel`, which is the incoming name
/// beside a different file when `beside`.
struct Write<'a> {
    rel: Rel,
    file: &'a BundleFile,
    beside: bool,
}

/// Write `files` from `blobs` into the home `home` (which must exist),
/// never overwriting: see the module's Decision Log for the rules.
///
/// # Errors
/// See [`PlaceError`].
pub fn place(
    home: &Path,
    files: &[Incoming],
    blobs: &BlobStore,
    on_conflict: OnConflict,
) -> Result<Placed, PlaceError> {
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| PlaceError::Io { path, source }
    };
    let root = fs::canonicalize(home).map_err(io_error(home))?;
    let none = Protected::default();
    let mut placed = Placed::default();
    let mut writes = Vec::new();
    for incoming in files {
        let rel = Rel::from_path(&incoming.place).ok_or_else(|| PlaceError::Path {
            path: incoming.place.clone(),
        })?;
        let full = rel.under(&root);
        let blocked = || PlaceError::Blocked { path: full.clone() };
        let found = match reach(&root, &rel, &none, false).map_err(io_error(&full))? {
            Walk::Stopped(Found::Missing) => Found::Missing,
            Walk::Stopped(
                Found::File { .. } | Found::Source | Found::Symlink(_) | Found::Blocked,
            ) => {
                return Err(blocked());
            }
            Walk::Reached(reached) => {
                look(reached.parent(), rel.name(), &none).map_err(io_error(&full))?
            }
        };
        match found {
            Found::Missing => writes.push(Write {
                rel,
                file: &incoming.file,
                beside: false,
            }),
            Found::File { content, .. } if content.hash == incoming.file.hash => {
                placed.same = placed.same.saturating_add(1);
            }
            Found::File { .. } | Found::Source | Found::Symlink(_) | Found::Blocked => {
                match on_conflict {
                    OnConflict::Refuse => return Err(PlaceError::Conflict { path: full }),
                    OnConflict::Beside => {
                        let beside = rel
                            .with_suffix(crate::INCOMING_SUFFIX)
                            .ok_or_else(blocked)?;
                        placed.beside.push(incoming.bundle.clone());
                        if incoming_is_there(&root, &beside, &incoming.file, &none)? {
                            continue;
                        }
                        writes.push(Write {
                            rel: beside,
                            file: &incoming.file,
                            beside: true,
                        });
                    }
                }
            }
        }
    }
    for write in writes {
        let full = write.rel.under(&root);
        let changed = || PlaceError::Changed { path: full.clone() };
        let Walk::Reached(reached) =
            reach(&root, &write.rel, &none, true).map_err(io_error(&full))?
        else {
            return Err(changed());
        };
        let dir = reached.parent();
        let name = write.rel.name();
        let free = match look(dir, name, &none).map_err(io_error(&full))? {
            Found::Missing => true,
            // An earlier incoming version beside a file is Vessel's own.
            Found::File { .. } => write.beside,
            Found::Source | Found::Symlink(_) | Found::Blocked => false,
        };
        if !free {
            return Err(changed());
        }
        write_blob(dir, name, write.file, blobs).map_err(|error| match error {
            ApplyError::DamagedBlob { expected, found } => {
                PlaceError::DamagedBlob { expected, found }
            }
            ApplyError::Io(source) => PlaceError::Io {
                path: full.clone(),
                source,
            },
            ApplyError::Changed | ApplyError::NoIncomingName => changed(),
        })?;
        if !write.beside {
            placed.written = placed.written.saturating_add(1);
        }
    }
    Ok(placed)
}

/// Whether the incoming version of a file is already beside it, with the
/// same content; an error if something other than a regular file (or
/// nothing) is at that name.
fn incoming_is_there(
    root: &Path,
    beside: &Rel,
    file: &BundleFile,
    none: &Protected,
) -> Result<bool, PlaceError> {
    let full = beside.under(root);
    let found = match reach(root, beside, none, false) {
        Ok(Walk::Reached(reached)) => look(reached.parent(), beside.name(), none),
        Ok(Walk::Stopped(found)) => Ok(found),
        Err(error) => Err(error),
    }
    .map_err(|source| PlaceError::Io {
        path: full.clone(),
        source,
    })?;
    match found {
        Found::Missing => Ok(false),
        Found::File { content, .. } => Ok(content.hash == file.hash),
        Found::Source | Found::Symlink(_) | Found::Blocked => {
            Err(PlaceError::Blocked { path: full })
        }
    }
}
