//! [`Framed`]: protocol frames over a byte stream (SSH stdio, the node's
//! local socket), as PROTOCOL.md version 3 specifies.
//!
//! [Decision Log]
//! - Purpose: a websocket delimits messages itself; a pipe or a socket is a
//!   byte stream, so frames need a boundary of their own.
//! - Alternatives: newline-delimited JSON (simple, but the reader must scan
//!   every byte for the delimiter and cannot refuse an oversized frame until
//!   it has buffered it); a binary envelope (a second encoding to keep in
//!   step with the JSON one).
//! - Choice: a big-endian `u32` length, then the frame's UTF-8 JSON, the
//!   same text a websocket frame carries (`tokio-util`'s length-delimited
//!   codec). The length is checked against [`MAX_FRAME`] before anything is
//!   allocated, so a hostile or broken peer cannot make the reader buffer
//!   without bound.
//! - Because the framing on SSH stdio and on the node's local socket is the
//!   same, `vesseld attach` can copy bytes between the two without decoding
//!   them (ADR 0007).

use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

/// Largest frame, in bytes (PROTOCOL.md version 3: 16 MiB). Larger than any
/// frame the protocol produces (a page of terminal output is bounded by the
/// store's page byte limit), small enough that a bad length cannot exhaust
/// memory.
pub const MAX_FRAME: NonZeroUsize = match NonZeroUsize::new(16 * 1024 * 1024) {
    Some(max) => max,
    None => NonZeroUsize::MIN,
};

/// Why a framed byte stream failed.
#[derive(Debug, thiserror::Error)]
pub enum FramedError {
    /// Reading or writing failed, or a frame's length exceeded the maximum
    /// (the codec reports that as `InvalidData` or `InvalidInput`).
    #[error("the byte stream failed")]
    Io(#[from] std::io::Error),
    /// A frame was not UTF-8, so it cannot be a protocol frame.
    #[error("a frame is not UTF-8 text")]
    NotUtf8(#[source] std::string::FromUtf8Error),
}

/// A [`FrameTransport`](crate::FrameTransport) over a reader and a writer:
/// the two halves of a socket, or a child process's stdout and stdin.
#[derive(Debug)]
pub struct Framed<R, W> {
    reader: FramedRead<R, LengthDelimitedCodec>,
    writer: FramedWrite<W, LengthDelimitedCodec>,
}

impl<R: AsyncRead, W: AsyncWrite> Framed<R, W> {
    /// Frames read from `reader` and written to `writer`, each at most
    /// `max_frame` bytes ([`MAX_FRAME`] is the protocol's limit).
    pub fn new(reader: R, writer: W, max_frame: NonZeroUsize) -> Self {
        Self {
            reader: FramedRead::new(reader, codec(max_frame)),
            writer: FramedWrite::new(writer, codec(max_frame)),
        }
    }
}

/// The length-delimited codec of PROTOCOL.md version 3.
fn codec(max_frame: NonZeroUsize) -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_type::<u32>()
        .max_frame_length(max_frame.get())
        .new_codec()
}

impl<R: AsyncRead + Unpin, W: Unpin> Stream for Framed<R, W> {
    type Item = Result<String, FramedError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.reader).poll_next(cx).map(|frame| {
            frame.map(|frame| {
                // `Vec::from(BytesMut)` reuses the buffer; no copy.
                String::from_utf8(Vec::from(frame?)).map_err(FramedError::NotUtf8)
            })
        })
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> Sink<String> for Framed<R, W> {
    type Error = FramedError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), FramedError>> {
        Sink::<Bytes>::poll_ready(Pin::new(&mut self.writer), cx).map_err(FramedError::Io)
    }

    fn start_send(mut self: Pin<&mut Self>, frame: String) -> Result<(), FramedError> {
        Sink::<Bytes>::start_send(Pin::new(&mut self.writer), Bytes::from(frame.into_bytes()))
            .map_err(FramedError::Io)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), FramedError>> {
        Sink::<Bytes>::poll_flush(Pin::new(&mut self.writer), cx).map_err(FramedError::Io)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), FramedError>> {
        Sink::<Bytes>::poll_close(Pin::new(&mut self.writer), cx).map_err(FramedError::Io)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    fn limit(bytes: usize) -> NonZeroUsize {
        NonZeroUsize::new(bytes).unwrap()
    }

    /// A framed end over one side of an in-memory byte pipe, and the raw
    /// other side.
    fn framed(
        max: usize,
    ) -> (
        Framed<
            tokio::io::ReadHalf<tokio::io::DuplexStream>,
            tokio::io::WriteHalf<tokio::io::DuplexStream>,
        >,
        tokio::io::DuplexStream,
    ) {
        let (ours, theirs) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(ours);
        (Framed::new(reader, writer, limit(max)), theirs)
    }

    #[tokio::test]
    async fn frames_round_trip_with_a_big_endian_length() {
        let (mut framed, mut raw) = framed(1024);
        framed
            .send("{\"type\":\"hello\"}".to_owned())
            .await
            .unwrap();
        let mut header = [0_u8; 4];
        let _read = raw.read_exact(&mut header).await.unwrap();
        assert_eq!(u32::from_be_bytes(header), 16);
        let mut body = vec![0_u8; 16];
        let _read = raw.read_exact(&mut body).await.unwrap();
        assert_eq!(body, b"{\"type\":\"hello\"}");

        raw.write_all(&3_u32.to_be_bytes()).await.unwrap();
        raw.write_all(b"abc").await.unwrap();
        assert_eq!(framed.next().await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn a_frame_split_across_reads_is_reassembled() {
        let (mut framed, mut raw) = framed(1024);
        // Each piece is read before the next is written; the frame is
        // complete only after the last one.
        for piece in [&[0_u8, 0][..], &[0, 5, b'h', b'e'], b"ll"] {
            raw.write_all(piece).await.unwrap();
            assert!(framed.next().now_or_never().is_none());
        }
        raw.write_all(b"o").await.unwrap();
        assert_eq!(framed.next().await.unwrap().unwrap(), "hello");
    }

    #[tokio::test]
    async fn an_oversized_length_fails_before_its_body_arrives() {
        let (mut framed, mut raw) = framed(8);
        // Announces 4 GiB; no body follows. The reader must fail on the
        // header alone instead of waiting to buffer it.
        raw.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(matches!(framed.next().await, Some(Err(FramedError::Io(_)))));
        // Writing an oversized frame fails too.
        let (mut framed, _raw) = self::framed(8);
        assert!(framed.send("123456789".to_owned()).await.is_err());
    }

    #[tokio::test]
    async fn non_utf8_frames_are_refused_and_end_of_stream_ends_it() {
        let (mut framed, mut raw) = framed(64);
        raw.write_all(&2_u32.to_be_bytes()).await.unwrap();
        raw.write_all(&[0xff, 0xfe]).await.unwrap();
        assert!(matches!(
            framed.next().await,
            Some(Err(FramedError::NotUtf8(_)))
        ));
        drop(raw);
        assert!(framed.next().await.is_none());
    }
}
