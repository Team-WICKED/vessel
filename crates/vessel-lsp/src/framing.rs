//! Bounded LSP Content-Length framing over any async byte stream.
//!
//! Limits apply before body allocation and while reading headers. The caller
//! owns deadlines and must discard a connection after a framing error; trying
//! to recover would let a malformed peer choose where the next frame starts.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum complete header, including the empty line (8 KiB).
const MAX_HEADER: usize = 8192;
/// Maximum JSON payload (4 MiB), before any decoder allocates result objects.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// A peer violated framing or its pipe failed.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// A pipe read or write failed.
    #[error("language-server pipe failed")]
    Io(#[from] std::io::Error),
    /// Header is malformed, missing a length or contains duplicate lengths.
    #[error("invalid language-server Content-Length header")]
    Header,
    /// Headers must be valid ASCII-compatible UTF-8.
    #[error("invalid language-server header encoding")]
    Encoding(#[from] std::str::Utf8Error),
    /// The byte length is not a representable integer.
    #[error("invalid language-server byte length")]
    Length(#[from] std::num::ParseIntError),
    /// Header or body exceeded the fixed transport budget.
    #[error("language-server frame exceeds the transport limit")]
    TooLarge,
    /// JSON does not match the expected typed message.
    #[error("invalid language-server JSON message")]
    Json(#[from] serde_json::Error),
}

/// Read one frame and decode directly into the edge's message type.
///
/// Returns `None` only for clean EOF between frames; EOF within one is an error.
/// This future is not cancel safe. Keep it in a dedicated reader task.
///
/// # Errors
/// Fails on malformed/oversized/truncated frames and invalid typed JSON.
pub async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>, FrameError> {
    let mut header = Vec::with_capacity(128);
    loop {
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte).await? == 0 {
            return if header.is_empty() {
                Ok(None)
            } else {
                Err(truncated())
            };
        }
        header.extend_from_slice(&byte);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() >= MAX_HEADER {
            return Err(FrameError::TooLarge);
        }
    }
    let length = body_length(&header)?;
    let mut body = vec![0; length];
    let _count = reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn truncated() -> FrameError {
    FrameError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "truncated LSP header",
    ))
}

fn body_length(header: &[u8]) -> Result<usize, FrameError> {
    let header = std::str::from_utf8(header)?;
    let mut length = None;
    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(FrameError::Header)?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() || !value.trim().bytes().all(|b| b.is_ascii_digit()) {
                return Err(FrameError::Header);
            }
            let parsed = value.trim().parse::<usize>()?;
            if parsed == 0 || parsed > MAX_BODY {
                return Err(FrameError::TooLarge);
            }
            length = Some(parsed);
        }
    }
    length.ok_or(FrameError::Header)
}

/// Encode one typed message with its byte length and flush the pipe.
///
/// # Errors
/// Fails if serialization, the output budget or the pipe fails.
pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_BODY {
        return Err(FrameError::TooLarge);
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Message {
        text: String,
    }

    #[tokio::test]
    async fn byte_lengths_and_adjacent_frames() {
        let mut bytes = Vec::new();
        let message = Message {
            text: "😀한글".into(),
        };
        write(&mut bytes, &message).await.unwrap();
        write(&mut bytes, &message).await.unwrap();
        let mut input = bytes.as_slice();
        assert_eq!(read::<_, Message>(&mut input).await.unwrap(), Some(message));
        assert!(read::<_, Message>(&mut input).await.unwrap().is_some());
        assert!(read::<_, Message>(&mut input).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refuses_ambiguous_oversized_and_truncated_frames() {
        for bytes in [
            "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            "Content-Length: 4194305\r\n\r\n",
            "Content-Length: +2\r\n\r\n{}",
            "Other: 2\r\n\r\n{}",
            "Content-Length: 2\r\n\r\n{",
            "Content-Length: 2\r\n",
        ] {
            assert!(
                read::<_, Message>(&mut bytes.as_bytes()).await.is_err(),
                "{bytes}"
            );
        }
        let bytes = vec![b'x'; MAX_HEADER];
        assert!(matches!(
            read::<_, Message>(&mut bytes.as_slice()).await,
            Err(FrameError::TooLarge)
        ));
    }
}
