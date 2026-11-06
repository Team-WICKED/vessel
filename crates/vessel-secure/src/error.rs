//! Why a handshake or a channel failed.

use std::io;
use std::time::Duration;

/// Why a handshake or a secure channel failed.
///
/// After any error but [`SecureError::TooLarge`] on the sending side and
/// [`SecureError::NotReady`], the channel is closed for good: the stream
/// ends and every send fails with [`SecureError::Closed`] (ADR 0009: no
/// recovery, no rekeying).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecureError {
    /// The handshake failed. On the client: whoever answered does not hold
    /// the node's key, or speaks another version (the prologue differs). On
    /// the node: the client used another key or version. NK gives a client
    /// with the wrong key no answer of its own: the node closes, and the
    /// client sees the carrier end ([`SecureError::Carrier`]).
    #[error("the secure handshake failed")]
    Handshake(#[source] snow::Error),
    /// A handshake message had the wrong size: ours are exactly 48 bytes,
    /// with empty payloads.
    #[error("a secure handshake message has {len} bytes instead of 48")]
    HandshakeMessage {
        /// Bytes received.
        len: usize,
    },
    /// The handshake did not finish in time.
    #[error("the secure handshake did not finish within {0:?}")]
    Timeout(Duration),
    /// A record failed authentication or broke the chunk rules. Something
    /// between the two ends changed, replayed, reordered or dropped records,
    /// or the peer is broken.
    #[error("the secure channel was tampered with ({0})")]
    Tampered(Tamper),
    /// A frame is longer than the limit: one we were asked to send (the
    /// channel stays usable), or one the peer announced (it closes).
    #[error("a frame of {len} bytes is over the limit of {max}")]
    TooLarge {
        /// The frame's length.
        len: u64,
        /// The limit.
        max: u32,
    },
    /// A received frame is not UTF-8 (protocol frames are JSON text).
    #[error("a received frame is not UTF-8")]
    NotUtf8,
    /// The nonce counter of one direction reached its maximum (2^64 - 1
    /// records; never in practice). Noise forbids reusing it, and there is
    /// no rekeying (ADR 0009).
    #[error("the secure channel used up its nonces")]
    Exhausted,
    /// The channel already failed or was closed.
    #[error("the secure channel is closed")]
    Closed,
    /// A frame was sent before the previous one was taken (`start_send`
    /// without `poll_ready`): a bug in the caller.
    #[error("a frame was sent before the secure channel was ready")]
    NotReady,
    /// The carrier failed, or ended during the handshake or in the middle
    /// of a frame (`UnexpectedEof`).
    #[error("the secure channel's carrier failed")]
    Carrier(#[source] io::Error),
    /// The Noise state machine refused an operation the channel should
    /// never ask for. A bug, not something a peer can cause.
    #[error("the Noise state machine failed")]
    Noise(#[source] snow::Error),
}

impl SecureError {
    /// Whether trying again may succeed: the network failed or was slow.
    /// Everything else is a verdict about the peer or the path (a wrong
    /// key, tampering, a broken peer), which another attempt would meet
    /// again.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Carrier(_) | Self::Timeout(_))
    }

    /// The carrier ended where it must not.
    pub(crate) fn eof() -> Self {
        Self::Carrier(io::ErrorKind::UnexpectedEof.into())
    }
}

/// Which rule a record broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Tamper {
    /// The record failed authentication: changed, forged, replayed,
    /// reordered, or a record before it went missing (the nonce is an
    /// implicit counter, so all of these look the same).
    #[error("a record failed authentication")]
    Authentication,
    /// A record longer than 65,535 bytes.
    #[error("a record is longer than 65,535 bytes")]
    RecordTooLong,
    /// A chunk shorter than its header.
    #[error("a chunk is shorter than its header")]
    Truncated,
    /// A chunk with reserved flag bits set.
    #[error("a chunk has reserved flags")]
    ReservedFlags,
    /// A frame starts with a chunk without `FIRST`.
    #[error("a frame starts without FIRST")]
    MissingFirst,
    /// `FIRST` in the middle of a frame.
    #[error("FIRST in the middle of a frame")]
    UnexpectedFirst,
    /// A chunk carries more than is left of the announced frame.
    #[error("a chunk overruns its frame")]
    Overrun,
    /// `FINAL` before the frame's end.
    #[error("FINAL before the frame's end")]
    EarlyFinal,
    /// The frame's last byte arrived without `FINAL`.
    #[error("a frame ended without FINAL")]
    MissingFinal,
    /// A chunk that is neither the frame's last nor full-size: chunking is
    /// canonical, so a frame cannot be stretched into many tiny records.
    #[error("a chunk before the frame's end is not full")]
    ShortChunk,
}
