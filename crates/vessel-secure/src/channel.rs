//! The channel after the handshake: protocol frames in, records out.
//!
//! [Decision Log]
//! - Purpose: give the RPC layer a `FrameTransport` (a stream and a sink of
//!   `String` frames), so `NodeClient::over` and the node's `serve` work on
//!   a secure channel unchanged.
//! - Constraint: Noise nonces are an implicit counter per direction, so
//!   records must reach the carrier in the order they were sealed, and a
//!   record sealed but never sent would desynchronise the two ends.
//! - Choice: a frame is chunked lazily. Each chunk is sealed only once the
//!   carrier is ready to take it, then handed over at once. Nothing sealed
//!   is ever held back or dropped, and a slow carrier holds back plaintext,
//!   not records.
//! - Any failure closes the channel for good (`broken`): the stream ends
//!   and sends fail with `Closed`. There is no resynchronising after a bad
//!   record (ADR 0009). The one exception is refusing to send a frame over
//!   the limit, which happens before anything is sealed.
//! - A carrier that ends between frames ends the stream cleanly; one that
//!   ends inside a frame is an error, so a cut connection never delivers
//!   half a frame.
//! - The frame limit can change while the channel is open
//!   ([`SecureChannel::set_max_frame`]): a node admits a client under a
//!   small limit until it knows who the client is, so an unauthenticated
//!   peer cannot make it buffer 16 MiB, then raises it. The limit is checked
//!   where a frame is announced, so the change takes effect at the next
//!   announcement; a frame already announced is bounded by its own length,
//!   checked when it was announced, and finishes under that.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_util::{Sink, Stream};
use snow::TransportState;
use snow::error::StateProblem;

use crate::Config;
use crate::carrier::Carrier;
use crate::error::{SecureError, Tamper};
use crate::record::{MAX_RECORD, Outgoing, Reassembly, TAG};

/// A secure channel over the carrier `C`: a stream and a sink of protocol
/// frames (`String`), each frame chunked into records sealed with the
/// handshake's keys. Made by [`initiate`](crate::initiate) or
/// [`respond`](crate::respond).
pub struct SecureChannel<C> {
    carrier: C,
    noise: TransportState,
    max_frame: u32,
    reassembly: Reassembly,
    /// The frame being chunked, if any.
    outgoing: Option<Outgoing>,
    /// One chunk's plaintext, sealed or opened.
    scratch: Vec<u8>,
    /// The carrier ended cleanly between frames.
    read_ended: bool,
    /// The channel failed or was tampered with: closed for good.
    broken: bool,
}

impl<C> fmt::Debug for SecureChannel<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecureChannel")
            .field("read_ended", &self.read_ended)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl<C> SecureChannel<C> {
    /// The longest frame this channel takes or sends now.
    #[must_use]
    pub const fn max_frame(&self) -> u32 {
        self.max_frame
    }

    /// Change the frame limit, in both directions, from the next frame
    /// announcement on.
    ///
    /// - Receiving: the next `FIRST` chunk read is checked against
    ///   `max_frame` before anything is buffered; over it, the channel
    ///   closes with [`SecureError::TooLarge`]. Frames the peer sent but
    ///   this end has not read yet are judged when they are read, so a
    ///   limit raised after a first frame covers whatever the peer pipelined
    ///   behind it.
    /// - Sending: the next frame passed to `send` is refused over it
    ///   (`TooLarge`, the channel stays open).
    /// - A frame already announced, half received or half sent, is not
    ///   affected: its length was checked against the limit in force when it
    ///   was announced, it can never grow past that length, and it finishes
    ///   normally. Lowering the limit under it therefore never fails it.
    ///
    /// For example, a node runs the device preamble under 64 KiB and raises
    /// the limit to [`DEFAULT_MAX_FRAME`](crate::DEFAULT_MAX_FRAME) once the
    /// client is admitted.
    pub fn set_max_frame(&mut self, max_frame: u32) {
        self.max_frame = max_frame;
        self.reassembly.set_max_frame(max_frame);
    }
}

impl<C: Carrier> SecureChannel<C> {
    pub(crate) fn new(carrier: C, noise: TransportState, config: &Config) -> Self {
        Self {
            carrier,
            noise,
            max_frame: config.max_frame,
            reassembly: Reassembly::new(config.max_frame),
            outgoing: None,
            scratch: Vec::new(),
            read_ended: false,
            broken: false,
        }
    }

    /// Close the channel for good and pass `error` on.
    fn fail(&mut self, error: SecureError) -> SecureError {
        self.broken = true;
        self.outgoing = None;
        error
    }

    /// Seal and hand over the chunks of the frame being sent, each as soon
    /// as the carrier can take it.
    fn drive(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SecureError>> {
        loop {
            let Some(outgoing) = self.outgoing.as_mut() else {
                return Poll::Ready(Ok(()));
            };
            if outgoing.is_done() {
                self.outgoing = None;
                continue;
            }
            if let Err(error) = ready!(Pin::new(&mut self.carrier).poll_ready(cx)) {
                return Poll::Ready(Err(self.fail(SecureError::Carrier(error))));
            }
            outgoing.write_next(&mut self.scratch);
            let record = match seal(&mut self.noise, &self.scratch) {
                Ok(record) => record,
                Err(error) => return Poll::Ready(Err(self.fail(error))),
            };
            if let Err(error) = Pin::new(&mut self.carrier).start_send(record) {
                return Poll::Ready(Err(self.fail(SecureError::Carrier(error))));
            }
        }
    }

    /// Open one record and feed its chunk to the reassembly.
    fn open(&mut self, record: &[u8]) -> Result<Option<String>, SecureError> {
        if record.len() > MAX_RECORD {
            return Err(SecureError::Tampered(Tamper::RecordTooLong));
        }
        self.scratch.clear();
        self.scratch.resize(record.len().saturating_sub(TAG), 0);
        let len = self
            .noise
            .read_message(record, &mut self.scratch)
            .map_err(noise_error)?;
        let chunk = self.scratch.get(..len).unwrap_or_default();
        let Some(frame) = self.reassembly.push(chunk)? else {
            return Ok(None);
        };
        String::from_utf8(frame)
            .map(Some)
            .map_err(|_not_utf8| SecureError::NotUtf8)
    }
}

/// Seal one chunk into a record.
fn seal(noise: &mut TransportState, plain: &[u8]) -> Result<Bytes, SecureError> {
    let mut record = vec![0_u8; plain.len().saturating_add(TAG)];
    let len = noise
        .write_message(plain, &mut record)
        .map_err(noise_error)?;
    record.truncate(len);
    Ok(Bytes::from(record))
}

fn noise_error(error: snow::Error) -> SecureError {
    if matches!(error, snow::Error::Decrypt) {
        SecureError::Tampered(Tamper::Authentication)
    } else if matches!(error, snow::Error::State(StateProblem::Exhausted)) {
        SecureError::Exhausted
    } else {
        SecureError::Noise(error)
    }
}

impl<C: Carrier> Stream for SecureChannel<C> {
    type Item = Result<String, SecureError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if this.broken || this.read_ended {
                return Poll::Ready(None);
            }
            let result = match ready!(Pin::new(&mut this.carrier).poll_next(cx)) {
                Some(Ok(record)) => this.open(&record),
                Some(Err(error)) => Err(SecureError::Carrier(error)),
                None => {
                    this.read_ended = true;
                    if !this.reassembly.in_frame() {
                        return Poll::Ready(None);
                    }
                    Err(SecureError::eof())
                }
            };
            match result {
                Ok(Some(frame)) => return Poll::Ready(Some(Ok(frame))),
                Ok(None) => {}
                Err(error) => return Poll::Ready(Some(Err(this.fail(error)))),
            }
        }
    }
}

impl<C: Carrier> Sink<String> for SecureChannel<C> {
    type Error = SecureError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SecureError>> {
        let this = self.get_mut();
        if this.broken {
            return Poll::Ready(Err(SecureError::Closed));
        }
        this.drive(cx)
    }

    fn start_send(self: Pin<&mut Self>, frame: String) -> Result<(), SecureError> {
        let this = self.get_mut();
        if this.broken {
            return Err(SecureError::Closed);
        }
        if this.outgoing.is_some() {
            return Err(SecureError::NotReady);
        }
        this.outgoing = Some(Outgoing::new(frame.into_bytes(), this.max_frame)?);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SecureError>> {
        let this = self.get_mut();
        if this.broken {
            return Poll::Ready(Err(SecureError::Closed));
        }
        ready!(this.drive(cx))?;
        Pin::new(&mut this.carrier)
            .poll_flush(cx)
            .map_err(|error| this.fail(SecureError::Carrier(error)))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SecureError>> {
        let this = self.get_mut();
        if this.broken {
            // Nothing more can be sent; dropping the channel drops the
            // carrier.
            return Poll::Ready(Ok(()));
        }
        ready!(this.drive(cx))?;
        Pin::new(&mut this.carrier)
            .poll_close(cx)
            .map_err(|error| this.fail(SecureError::Carrier(error)))
    }
}

#[cfg(test)]
impl<C: Carrier> SecureChannel<C> {
    /// Seal arbitrary chunk plaintext, for tests that break the chunk rules
    /// with records that still authenticate.
    pub(crate) fn seal_raw(&mut self, plain: &[u8]) -> Result<Bytes, SecureError> {
        seal(&mut self.noise, plain)
    }

    /// Move the receiving nonce, to reach its maximum without 2^64 records.
    pub(crate) fn set_receiving_nonce(&mut self, nonce: u64) {
        self.noise.set_receiving_nonce(nonce);
    }

    /// The carrier, to send records around the channel.
    pub(crate) fn carrier_mut(&mut self) -> &mut C {
        &mut self.carrier
    }

    /// Memory held for a half-received frame.
    pub(crate) fn reassembly_capacity(&self) -> usize {
        self.reassembly.capacity()
    }
}
