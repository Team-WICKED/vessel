//! Fetching one file chunk by chunk ([`fetch_chunks`]) into a
//! [`ChunkSink`] that writes each chunk as it arrives and checks the whole:
//! a fleet member's blobs (`fleet.blob`), an import's transfer files, and
//! a client reading a transfer (`transfer.read`) all go through this one
//! loop.
//!
//! [Decision Log]
//! - Purpose: a file of up to 256 MiB (`MAX_TRANSFER_FILE`) is never held
//!   in memory; it streams into a writer that hashes it as it goes and
//!   keeps it only if the hash is right.
//! - The writer is the caller's, behind [`ChunkSink`]: the one Vessel has
//!   is `vessel_fleet::BlobStore::writer` (a temporary file, hashed while
//!   written, renamed into the store once checked), and every caller,
//!   the node and tests alike, plugs that in. Defining the trait here
//!   keeps the client from depending on `vessel-fleet` (and through it
//!   `vessel-harness`), which ADR 0001's crate order would need, and still
//!   leaves one loop and one writer rather than a copy of each.
//! - Bounded: a chunk that would take the file past its manifest size (or
//!   past the total the source reports) ends the fetch; an empty chunk
//!   means nothing more will come, and the sink's check decides.

use std::future::Future;
use std::io;

use vessel_proto::{BlobChunk, BundleFile};

/// Where a file fetched chunk by chunk goes.
pub trait ChunkSink: Send + 'static {
    /// Append `data`. Called on a blocking thread.
    ///
    /// # Errors
    /// The data cannot be written.
    fn write(&mut self, data: &[u8]) -> io::Result<()>;

    /// All of it arrived: check it (its hash) and keep it. Called on a
    /// blocking thread.
    ///
    /// # Errors
    /// It is not the file it should be, or it cannot be kept.
    fn finish(self) -> io::Result<()>;
}

/// Why fetching a file failed.
#[derive(Debug, thiserror::Error)]
pub enum FetchError<E> {
    /// Asking for a chunk failed.
    #[error(transparent)]
    Read(E),
    /// More arrived than the manifest says, or the manifest says more than
    /// a transfer may hold.
    #[error("the file is {0} bytes, more than its manifest or the limit allows")]
    Oversized(u64),
    /// The sink could not write it, or what arrived is not the file (its
    /// hash).
    #[error("a fetched file: {0}")]
    Sink(#[source] io::Error),
    /// A blocking task failed.
    #[error("a blocking task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Fetch `file` into `sink`, chunk by chunk through `read` (an offset),
/// none taken past the manifest's size, and finish it.
///
/// `read` is a plain `FnMut` returning a future rather than an async
/// closure: a future of an async closure that borrows a client is not
/// `Send` for every lifetime, so a loop using it could not be spawned.
///
/// # Errors
/// See [`FetchError`]. On an error the sink is dropped unfinished (a blob
/// writer then removes what it wrote).
pub async fn fetch_chunks<E, F, S>(
    file: &BundleFile,
    sink: S,
    mut read: impl FnMut(u64) -> F,
) -> Result<(), FetchError<E>>
where
    F: Future<Output = Result<BlobChunk, E>>,
    S: ChunkSink,
{
    let mut sink = Some(sink);
    let mut offset = 0_u64;
    loop {
        let BlobChunk { data, total } = read(offset).await.map_err(FetchError::Read)?;
        let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
        offset = offset.saturating_add(length);
        if offset > file.size || offset > total {
            return Err(FetchError::Oversized(offset));
        }
        let mut writing = sink.take().ok_or(FetchError::Oversized(offset))?;
        writing = tokio::task::spawn_blocking(move || writing.write(&data).map(|()| writing))
            .await?
            .map_err(FetchError::Sink)?;
        if offset >= total || length == 0 {
            return tokio::task::spawn_blocking(move || writing.finish())
                .await?
                .map_err(FetchError::Sink);
        }
        sink = Some(writing);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use vessel_proto::BlobHash;

    use super::*;

    /// Collects what it is given; `finish` keeps it.
    #[derive(Clone, Default)]
    struct Collect {
        written: Arc<Mutex<Vec<u8>>>,
        kept: Arc<Mutex<bool>>,
    }

    impl ChunkSink for Collect {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            self.written
                .lock()
                .map_err(|_poisoned| io::Error::other("poisoned"))?
                .extend_from_slice(data);
            Ok(())
        }

        fn finish(self) -> io::Result<()> {
            *self
                .kept
                .lock()
                .map_err(|_poisoned| io::Error::other("poisoned"))? = true;
            Ok(())
        }
    }

    fn file(size: u64) -> BundleFile {
        BundleFile {
            hash: BlobHash::from_bytes([1; 32]),
            size,
            executable: false,
        }
    }

    /// `content` served `chunk` bytes at a time.
    fn serve(
        content: &'static [u8],
        chunk: usize,
    ) -> impl FnMut(u64) -> std::future::Ready<Result<BlobChunk, ()>> {
        move |offset| {
            let start = usize::try_from(offset).unwrap();
            let end = start.saturating_add(chunk).min(content.len());
            std::future::ready(Ok(BlobChunk {
                data: content.get(start..end).unwrap_or_default().to_vec().into(),
                total: u64::try_from(content.len()).unwrap(),
            }))
        }
    }

    #[tokio::test]
    async fn chunks_are_written_in_order_and_the_whole_is_finished() {
        let sink = Collect::default();
        fetch_chunks(&file(10), sink.clone(), serve(b"0123456789", 3))
            .await
            .unwrap();
        assert_eq!(sink.written.lock().unwrap().as_slice(), b"0123456789");
        assert!(*sink.kept.lock().unwrap());
    }

    #[tokio::test]
    async fn more_than_the_manifest_says_is_refused_unfinished() {
        let sink = Collect::default();
        let error = fetch_chunks(&file(4), sink.clone(), serve(b"0123456789", 3))
            .await
            .unwrap_err();
        assert!(matches!(error, FetchError::Oversized(6)), "{error:?}");
        assert!(!*sink.kept.lock().unwrap());

        let failed = fetch_chunks(&file(4), Collect::default(), |_offset| {
            std::future::ready(Err::<BlobChunk, _>("gone"))
        })
        .await
        .unwrap_err();
        assert!(matches!(failed, FetchError::Read("gone")), "{failed:?}");
    }
}
