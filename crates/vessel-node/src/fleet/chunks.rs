//! Blobs moved between nodes chunk by chunk: serving one ([`serve`]) and
//! fetching a set into a store, checked ([`fetch`], over the client's
//! per-file loop with the store's writer, `Blob`). What the home does
//! for `fleet.blob` and a member with its answers, and what a transfer's
//! source does for `transfer.read` and an import with its answers
//! (protocol 6, plan F4): the same shapes, so the same code.

use std::io;

use vessel_client::{ChunkSink, FetchError, fetch_chunks};
use vessel_fleet::{BlobStore, BlobWriter, PastEnd};
use vessel_proto::{BlobChunk, BlobHash, BundleFile, ErrorCode, WireError};

/// Up to [`BlobChunk::MAX_LEN`] bytes of blob `hash` in `blobs` from
/// `offset`.
pub(crate) async fn serve(
    blobs: BlobStore,
    hash: BlobHash,
    offset: u64,
) -> Result<BlobChunk, WireError> {
    let read =
        tokio::task::spawn_blocking(move || blobs.read_chunk(&hash, offset, BlobChunk::MAX_LEN))
            .await
            .map_err(|error| WireError::new(ErrorCode::Internal, error.to_string()))?;
    match read {
        Ok((data, total)) => Ok(BlobChunk {
            data: data.into(),
            total,
        }),
        Err(error)
            if error
                .get_ref()
                .is_some_and(<dyn std::error::Error + Send + Sync>::is::<PastEnd>) =>
        {
            Err(WireError::new(ErrorCode::InvalidRequest, error.to_string()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(WireError::new(
            ErrorCode::NotFound,
            format!("blob {hash} is not in the store"),
        )),
        Err(error) => Err(WireError::new(ErrorCode::Internal, error.to_string())),
    }
}

/// A blob being fetched: the store's writer, which hashes it as it is
/// written and puts it in the store only if the hash is right, as the
/// client's chunk loop wants it.
struct Blob(BlobWriter);

impl ChunkSink for Blob {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        io::Write::write_all(&mut self.0, data)
    }

    fn finish(self) -> io::Result<()> {
        self.0.finish().map(drop)
    }
}

/// Fetch each of `files` that `blobs` lacks, chunk by chunk through `read`
/// (a blob and an offset), each checked against its hash before it counts
/// as present, and none taken longer than its manifest says (the client's
/// loop, `vessel_client::fetch_chunks`).
///
/// `read` is a plain `FnMut` returning a future rather than an async
/// closure: a future of an async closure that borrows the client is not
/// `Send` for every lifetime, so the member's pull loop could not be
/// spawned.
pub(crate) async fn fetch<'a, E, F>(
    files: impl IntoIterator<Item = &'a BundleFile>,
    blobs: &BlobStore,
    mut read: impl FnMut(BlobHash, u64) -> F,
) -> Result<(), FetchError<E>>
where
    F: Future<Output = Result<BlobChunk, E>>,
{
    let mut wanted: Vec<&BundleFile> = files.into_iter().collect();
    wanted.sort_by_key(|file| file.hash);
    wanted.dedup_by_key(|file| file.hash);
    for file in wanted {
        if blobs.contains(&file.hash) {
            continue;
        }
        let writer = Blob(blobs.writer(file.hash).map_err(FetchError::Sink)?);
        fetch_chunks(file, writer, |offset| read(file.hash, offset)).await?;
    }
    Ok(())
}
