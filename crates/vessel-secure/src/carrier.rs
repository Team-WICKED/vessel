//! Carriers: whatever moves whole binary messages between the two ends.
//!
//! The channel needs nothing else from a transport: a websocket's binary
//! messages (the relay and `/secure`, in `vessel-relay` and `vessel-node`),
//! a byte stream with a `u16` length before each message
//! ([`LengthPrefixed`], for stdio later), or memory ([`memory_pair`]).

use std::io;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::PollSender;

use crate::record::MAX_RECORD;

/// Any carrier of whole binary messages, in order and reliably. Messages
/// longer than [`MAX_RECORD`](crate::MAX_RECORD) should be refused by the
/// carrier before it reads them; the channel refuses them too.
pub trait Carrier:
    Stream<Item = io::Result<Bytes>> + Sink<Bytes, Error = io::Error> + Unpin + Send + 'static
{
}

impl<T> Carrier for T where
    T: Stream<Item = io::Result<Bytes>> + Sink<Bytes, Error = io::Error> + Unpin + Send + 'static
{
}

/// One end of an in-memory carrier.
#[derive(Debug)]
pub struct MemoryCarrier {
    incoming: mpsc::Receiver<Bytes>,
    outgoing: PollSender<Bytes>,
}

/// Two connected in-memory carrier ends; each direction holds at most
/// `capacity` messages, so a reader that stops makes the writer wait.
#[must_use]
pub fn memory_pair(capacity: NonZeroUsize) -> (MemoryCarrier, MemoryCarrier) {
    let (a_tx, a_rx) = mpsc::channel(capacity.get());
    let (b_tx, b_rx) = mpsc::channel(capacity.get());
    (
        MemoryCarrier {
            incoming: b_rx,
            outgoing: PollSender::new(a_tx),
        },
        MemoryCarrier {
            incoming: a_rx,
            outgoing: PollSender::new(b_tx),
        },
    )
}

fn gone<T>(_error: T) -> io::Error {
    io::ErrorKind::BrokenPipe.into()
}

impl Stream for MemoryCarrier {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut()
            .incoming
            .poll_recv(cx)
            .map(|message| message.map(Ok))
    }
}

impl Sink<Bytes> for MemoryCarrier {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().outgoing)
            .poll_ready(cx)
            .map_err(gone)
    }

    fn start_send(self: Pin<&mut Self>, message: Bytes) -> io::Result<()> {
        Pin::new(&mut self.get_mut().outgoing)
            .start_send(message)
            .map_err(gone)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().outgoing)
            .poll_flush(cx)
            .map_err(gone)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().outgoing)
            .poll_close(cx)
            .map_err(gone)
    }
}

/// A carrier over a byte stream: each message is a `u16` big-endian length
/// and that many bytes, so no message can exceed one record, and the
/// length is checked before the message is read.
#[derive(Debug)]
pub struct LengthPrefixed<R, W> {
    reader: FramedRead<R, LengthDelimitedCodec>,
    writer: FramedWrite<W, LengthDelimitedCodec>,
}

impl<R: AsyncRead, W: AsyncWrite> LengthPrefixed<R, W> {
    /// Messages read from `reader` and written to `writer`.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: FramedRead::new(reader, codec()),
            writer: FramedWrite::new(writer, codec()),
        }
    }
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_type::<u16>()
        .max_frame_length(MAX_RECORD)
        .new_codec()
}

impl<R: AsyncRead + Unpin, W: Unpin> Stream for LengthPrefixed<R, W> {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().reader)
            .poll_next(cx)
            .map(|message| message.map(|bytes| bytes.map(bytes::BytesMut::freeze)))
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> Sink<Bytes> for LengthPrefixed<R, W> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Sink::<Bytes>::poll_ready(Pin::new(&mut self.get_mut().writer), cx)
    }

    fn start_send(self: Pin<&mut Self>, message: Bytes) -> io::Result<()> {
        Pin::new(&mut self.get_mut().writer).start_send(message)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Sink::<Bytes>::poll_flush(Pin::new(&mut self.get_mut().writer), cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Sink::<Bytes>::poll_close(Pin::new(&mut self.get_mut().writer), cx)
    }
}
