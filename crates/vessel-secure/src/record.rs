//! The record and chunk layer: one protocol frame as one or more chunks,
//! each sealed into one record (one carrier message).
//!
//! A chunk's plaintext is `flags: u8`, then for `FIRST` the frame's length
//! (`u32`, big-endian), then data. `FINAL` marks the frame's last chunk.
//!
//! [Decision Log]
//! - Purpose: a Noise message holds at most 65,535 bytes, tag included,
//!   and protocol frames go up to 16 MiB, so frames are cut into chunks.
//! - Constraint: a peer, or anything between the two ends, must not be able
//!   to make the receiver allocate more than one frame of the announced
//!   length, or keep a frame open forever (ADR 0009).
//! - Alternatives: a length prefix inside the first record only, with
//!   continuation records unmarked (a lost `FINAL` cannot be detected); a
//!   kind byte as in Paseo (we have one kind of payload); any chunk sizes.
//! - Choice: explicit `FIRST`/`FINAL` flags, the length announced up front
//!   and checked against the limit before anything is buffered, and
//!   canonical chunking: every chunk but the last is full. One frame has
//!   exactly one encoding, and a frame cannot be stretched into millions
//!   of one-byte records that each cost a decryption.
//! - Buffers grow with the data (doubling, capped at the announced
//!   length): announcing 16 MiB and sending one byte costs one chunk of
//!   memory, not 16 MiB.

use crate::error::{SecureError, Tamper};

/// The largest record, tag included: Noise's limit.
pub const MAX_RECORD: usize = 65_535;

/// The authentication tag ChaCha20-Poly1305 adds to every record.
pub(crate) const TAG: usize = 16;

/// The largest chunk plaintext.
pub(crate) const MAX_CHUNK: usize = MAX_RECORD - TAG;

/// Flag: the chunk starts a frame and carries its length.
pub(crate) const FIRST: u8 = 1;

/// Flag: the chunk ends a frame.
pub(crate) const FINAL: u8 = 2;

/// Data bytes in a full `FIRST` chunk (after the flags and the length).
pub(crate) const FIRST_DATA: usize = MAX_CHUNK - 5;

/// Data bytes in a full chunk after the first (after the flags).
pub(crate) const NEXT_DATA: usize = MAX_CHUNK - 1;

/// A frame on its way out, cut into chunks one at a time, so each chunk is
/// sealed only when the carrier can take it (nonces follow sending order).
#[derive(Debug)]
pub(crate) struct Outgoing {
    frame: Vec<u8>,
    /// The frame's length, as announced.
    total: u32,
    /// Data bytes already chunked.
    sent: usize,
    /// Whether the `FIRST` chunk was written.
    started: bool,
}

impl Outgoing {
    /// A frame to send, refused when longer than `max_frame`.
    pub(crate) fn new(frame: Vec<u8>, max_frame: u32) -> Result<Self, SecureError> {
        let too_large = || SecureError::TooLarge {
            len: u64::try_from(frame.len()).unwrap_or(u64::MAX),
            max: max_frame,
        };
        let total = u32::try_from(frame.len()).map_err(|_overflow| too_large())?;
        if total > max_frame {
            return Err(too_large());
        }
        Ok(Self {
            frame,
            total,
            sent: 0,
            started: false,
        })
    }

    /// Whether every chunk, `FINAL` included, was written.
    pub(crate) fn is_done(&self) -> bool {
        self.started && self.sent >= self.frame.len()
    }

    /// Write the next chunk's plaintext into `plain` (cleared first).
    /// Call only while not [`is_done`](Self::is_done).
    pub(crate) fn write_next(&mut self, plain: &mut Vec<u8>) {
        plain.clear();
        let rest = self.frame.get(self.sent..).unwrap_or_default();
        let room = if self.started { NEXT_DATA } else { FIRST_DATA };
        let take = rest.len().min(room);
        let mut flags = 0;
        if !self.started {
            flags |= FIRST;
        }
        if take == rest.len() {
            flags |= FINAL;
        }
        plain.push(flags);
        if !self.started {
            plain.extend_from_slice(&self.total.to_be_bytes());
        }
        plain.extend_from_slice(rest.get(..take).unwrap_or_default());
        self.sent = self.sent.saturating_add(take);
        self.started = true;
    }
}

/// Reassembles chunks into frames, enforcing every rule before buffering.
#[derive(Debug)]
pub(crate) struct Reassembly {
    max_frame: u32,
    partial: Option<Partial>,
}

/// A frame with some chunks received.
#[derive(Debug)]
struct Partial {
    data: Vec<u8>,
    total: usize,
}

impl Reassembly {
    pub(crate) fn new(max_frame: u32) -> Self {
        Self {
            max_frame,
            partial: None,
        }
    }

    /// Check the next announced frame against `max_frame`. A frame already
    /// announced keeps its own length as its bound.
    pub(crate) fn set_max_frame(&mut self, max_frame: u32) {
        self.max_frame = max_frame;
    }

    /// Whether a frame is half received.
    pub(crate) fn in_frame(&self) -> bool {
        self.partial.is_some()
    }

    /// Memory held for the frame being received.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.partial
            .as_ref()
            .map_or(0, |partial| partial.data.capacity())
    }

    /// Take one chunk's plaintext; a complete frame comes back. After an
    /// error the state is undefined: the channel closes.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>, SecureError> {
        let tampered = |rule| Err(SecureError::Tampered(rule));
        let Some((&flags, rest)) = chunk.split_first() else {
            return tampered(Tamper::Truncated);
        };
        if flags & !(FIRST | FINAL) != 0 {
            return tampered(Tamper::ReservedFlags);
        }
        let last = flags & FINAL != 0;
        match (self.partial.take(), flags & FIRST != 0) {
            (None, false) => tampered(Tamper::MissingFirst),
            (Some(_), true) => tampered(Tamper::UnexpectedFirst),
            (None, true) => {
                let Some((length, data)) = rest.split_first_chunk::<4>() else {
                    return tampered(Tamper::Truncated);
                };
                let announced = u32::from_be_bytes(*length);
                let too_large = SecureError::TooLarge {
                    len: u64::from(announced),
                    max: self.max_frame,
                };
                if announced > self.max_frame {
                    return Err(too_large);
                }
                let total = usize::try_from(announced).map_err(|_overflow| too_large)?;
                self.accept(Vec::new(), total, data, last, FIRST_DATA)
            }
            (
                Some(Partial {
                    data: buffer,
                    total,
                }),
                false,
            ) => self.accept(buffer, total, rest, last, NEXT_DATA),
        }
    }

    fn accept(
        &mut self,
        mut buffer: Vec<u8>,
        total: usize,
        data: &[u8],
        last: bool,
        full: usize,
    ) -> Result<Option<Vec<u8>>, SecureError> {
        let tampered = |rule| Err(SecureError::Tampered(rule));
        let left = total.saturating_sub(buffer.len());
        if data.len() > left {
            return tampered(Tamper::Overrun);
        }
        let complete = data.len() == left;
        if last && !complete {
            return tampered(Tamper::EarlyFinal);
        }
        if complete && !last {
            return tampered(Tamper::MissingFinal);
        }
        if !last && data.len() != full {
            return tampered(Tamper::ShortChunk);
        }
        reserve_within(&mut buffer, data.len(), total);
        buffer.extend_from_slice(data);
        if last {
            return Ok(Some(buffer));
        }
        self.partial = Some(Partial {
            data: buffer,
            total,
        });
        Ok(None)
    }
}

/// Make room for `more` bytes, doubling as `Vec` does but never past
/// `total`, the frame's announced length.
fn reserve_within(buffer: &mut Vec<u8>, more: usize, total: usize) {
    let needed = buffer.len().saturating_add(more);
    if needed <= buffer.capacity() {
        return;
    }
    let target = buffer.capacity().saturating_mul(2).max(needed).min(total);
    buffer.reserve_exact(target.saturating_sub(buffer.len()));
}
