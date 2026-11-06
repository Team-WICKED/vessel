//! Blobs moved between nodes chunk by chunk: serving one ([`serve`]) and
//! fetching a set into a store, checked ([`fetch`]). What the home does
//! for `fleet.blob` and a member with its answers, and what a transfer's
//! source does for `transfer.read` and an import with its answers
//! (protocol 6, plan F4): the same shapes, so the same code.

use std::io;

use vessel_fleet::{BlobStore, PastEnd};
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

/// Why fetching blobs failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError<E> {
    /// Asking for a chunk failed.
    #[error(transparent)]
    Read(E),
    /// A blob arrived longer than the manifest said.
    #[error("a blob arrived longer than its manifest said ({0} bytes)")]
    Oversized(u64),
    /// The store could not be written, or a blob did not hash to its name.
    #[error("a blob: {0}")]
    Blob(#[source] io::Error),
    /// A blocking task failed.
    #[error("a blocking task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Fetch each of `files` that `blobs` lacks, chunk by chunk through `read`
/// (a blob and an offset), each checked against its hash before it counts
/// as present, and none taken longer than its manifest says.
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
        let mut writer = Some(blobs.writer(file.hash).map_err(FetchError::Blob)?);
        let mut offset = 0_u64;
        loop {
            let BlobChunk { data, total } =
                read(file.hash, offset).await.map_err(FetchError::Read)?;
            let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
            offset = offset.saturating_add(length);
            if offset > file.size || offset > total {
                return Err(FetchError::Oversized(offset));
            }
            let mut chunk_writer = writer.take().ok_or(FetchError::Oversized(offset))?;
            chunk_writer = tokio::task::spawn_blocking(move || {
                io::Write::write_all(&mut chunk_writer, &data).map(|()| chunk_writer)
            })
            .await?
            .map_err(FetchError::Blob)?;
            if offset >= total || length == 0 {
                // All of it arrived, or the source sent an empty chunk
                // (nothing more will come): the hash decides.
                let _stored = tokio::task::spawn_blocking(move || chunk_writer.finish())
                    .await?
                    .map_err(FetchError::Blob)?;
                break;
            }
            writer = Some(chunk_writer);
        }
    }
    Ok(())
}
