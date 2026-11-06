//! The content-addressed blob store: one file per blob, named by its
//! SHA-256, under `<root>/<first two hex digits>/`.
//!
//! The home fills it when it bundles ([`BlobStore::insert_file`]) and
//! serves it chunk by chunk (`fleet.blob`, [`BlobStore::read_chunk`]); a
//! member fills it from those chunks ([`BlobStore::writer`]), which are
//! checked against the hash before the blob counts as present. Every blob
//! is written under a temporary name and renamed into place, so a blob
//! that exists is complete. Owner-only: the store is Vessel's state.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use vessel_proto::BlobHash;

use crate::files::{self, Mode};

/// How old a temporary file in the store must be before [`BlobStore::retain`]
/// takes it for a crash's leftover rather than a blob being written.
const STALE_TEMPORARY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Content-addressed blobs in a directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

/// Content that does not hash to the blob it was received as.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("blob {expected}: received {size} bytes hashing to {found}")]
pub struct InvalidBlob {
    /// The hash asked for.
    pub expected: BlobHash,
    /// The hash of what arrived.
    pub found: BlobHash,
    /// How much arrived.
    pub size: u64,
}

/// A read from past a blob's end (`fleet.blob` with too large an offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("offset {offset} is past the blob's {total} bytes")]
pub struct PastEnd {
    /// The offset asked for.
    pub offset: u64,
    /// The blob's size.
    pub total: u64,
}

/// A writer used after its blob was committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the blob was already committed")]
struct Committed;

impl BlobStore {
    /// The store in `root`, created (owner-only) if missing.
    ///
    /// # Errors
    /// The directory cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        files::private_dir(&root)?;
        Ok(Self { root })
    }

    /// Where the store is.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where blob `hash` is (or would be).
    #[must_use]
    pub fn path(&self, hash: &BlobHash) -> PathBuf {
        let name = hash.to_string();
        let prefix = name.get(..2).unwrap_or("00").to_owned();
        self.root.join(prefix).join(name)
    }

    /// Whether blob `hash` is present.
    #[must_use]
    pub fn contains(&self, hash: &BlobHash) -> bool {
        self.path(hash).is_file()
    }

    /// Copy the file at `source` into the store; its hash and size.
    ///
    /// # Errors
    /// The source cannot be read, or the store cannot be written.
    pub fn insert_file(&self, source: &Path) -> io::Result<(BlobHash, u64)> {
        let mut writer = self.writer_unchecked()?;
        let mut file = File::open(source)?;
        let _copied = io::copy(&mut file, &mut writer)?;
        writer.commit(None)
    }

    /// Put `bytes` in the store; their hash.
    ///
    /// # Errors
    /// The store cannot be written.
    pub fn insert(&self, bytes: &[u8]) -> io::Result<BlobHash> {
        let mut writer = self.writer_unchecked()?;
        writer.write_all(bytes)?;
        writer.commit(None).map(|(hash, _size)| hash)
    }

    /// A writer for blob `expected` arriving in chunks (a member fetching
    /// from the home). [`BlobWriter::finish`] checks the hash before the
    /// blob appears in the store.
    ///
    /// # Errors
    /// The store cannot be written.
    pub fn writer(&self, expected: BlobHash) -> io::Result<BlobWriter> {
        let mut writer = self.writer_unchecked()?;
        writer.expected = Some(expected);
        Ok(writer)
    }

    fn writer_unchecked(&self) -> io::Result<BlobWriter> {
        let temporary = files::temporary_beside(&self.root.join("incoming"))?;
        let file = files::create_new(&temporary, Mode::Private)?;
        Ok(BlobWriter {
            store: self.clone(),
            temporary,
            file: Some(file),
            hasher: Sha256::new(),
            size: 0,
            expected: None,
        })
    }

    /// Up to `max` bytes of blob `hash` from `offset`, and the blob's size.
    ///
    /// # Errors
    /// The blob is missing (`NotFound`), `offset` is past its end
    /// (`InvalidInput`, carrying [`PastEnd`]), or it cannot be read.
    pub fn read_chunk(
        &self,
        hash: &BlobHash,
        offset: u64,
        max: usize,
    ) -> io::Result<(Vec<u8>, u64)> {
        let mut file = File::open(self.path(hash))?;
        let total = file.metadata()?.len();
        if offset > total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                PastEnd { offset, total },
            ));
        }
        let _start = file.seek(SeekFrom::Start(offset))?;
        let left = total.saturating_sub(offset);
        let wanted = usize::try_from(left).map_or(max, |left| left.min(max));
        let mut data = Vec::with_capacity(wanted);
        let _read = file
            .take(u64::try_from(wanted).unwrap_or(u64::MAX))
            .read_to_end(&mut data)?;
        Ok((data, total))
    }

    /// Remove blob `hash` (found damaged), so it is fetched again.
    ///
    /// # Errors
    /// It cannot be removed; a missing blob is no error.
    pub fn remove(&self, hash: &BlobHash) -> io::Result<()> {
        match fs::remove_file(self.path(hash)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            Ok(()) | Err(_) => Ok(()),
        }
    }

    /// Open blob `hash` for reading.
    ///
    /// # Errors
    /// The blob is missing or cannot be opened.
    pub fn open_blob(&self, hash: &BlobHash) -> io::Result<File> {
        File::open(self.path(hash))
    }

    /// Remove every blob not in `keep`, and temporary files over an hour
    /// old (a crash's leftovers; younger ones may belong to a writer still
    /// filling them); how many blobs were removed.
    ///
    /// # Errors
    /// The store cannot be listed or a blob cannot be removed.
    pub fn retain(&self, keep: &BTreeSet<BlobHash>) -> io::Result<usize> {
        let mut removed = 0_usize;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_file() {
                // Only temporary files live directly in the root: a writer
                // still filling one is left alone, one left over from a
                // crash goes once it is old.
                let stale = entry
                    .metadata()?
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age > STALE_TEMPORARY);
                if stale {
                    fs::remove_file(entry.path())?;
                }
                continue;
            }
            if !kind.is_dir() {
                continue;
            }
            for blob in fs::read_dir(entry.path())? {
                let blob = blob?;
                let name = blob.file_name();
                let kept = name
                    .to_str()
                    .and_then(|name| name.parse::<BlobHash>().ok())
                    .is_some_and(|hash| keep.contains(&hash));
                if !kept {
                    fs::remove_file(blob.path())?;
                    removed = removed.saturating_add(1);
                }
            }
        }
        Ok(removed)
    }
}

/// A blob being written; it appears in the store only once committed.
pub struct BlobWriter {
    store: BlobStore,
    temporary: PathBuf,
    file: Option<File>,
    hasher: Sha256,
    size: u64,
    expected: Option<BlobHash>,
}

impl fmt::Debug for BlobWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobWriter")
            .field("expected", &self.expected)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl io::Write for BlobWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other(Committed))?;
        let written = file.write(data)?;
        let chunk = data.get(..written).unwrap_or_default();
        self.hasher.update(chunk);
        self.size = self
            .size
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), File::flush)
    }
}

impl BlobWriter {
    /// How many bytes were written so far.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Check the content against the expected hash and move it into the
    /// store.
    ///
    /// # Errors
    /// The content hashes to something else (`InvalidData`, carrying
    /// [`InvalidBlob`]), or the store cannot be written.
    pub fn finish(self) -> io::Result<(BlobHash, u64)> {
        let expected = self.expected;
        self.commit(expected)
    }

    fn commit(mut self, expected: Option<BlobHash>) -> io::Result<(BlobHash, u64)> {
        let file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other(Committed))?;
        file.sync_all()?;
        drop(file);
        let found = BlobHash::from_bytes(std::mem::take(&mut self.hasher).finalize().into());
        if let Some(expected) = expected
            && expected != found
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                InvalidBlob {
                    expected,
                    found,
                    size: self.size,
                },
            ));
        }
        let path = self.store.path(&found);
        if let Some(dir) = path.parent() {
            files::private_dir(dir)?;
        }
        // Equal content may already be there; either copy is the same.
        fs::rename(&self.temporary, &path)?;
        Ok((found, self.size))
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        // Committed blobs were renamed away; anything left is unfinished.
        drop(self.file.take());
        if let Err(error) = fs::remove_file(&self.temporary)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.temporary.display(), %error, "could not remove an unfinished blob");
        }
    }
}
