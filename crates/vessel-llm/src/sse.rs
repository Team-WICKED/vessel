//! Server-Sent Events, the framing every streamed model response uses
//! (`text/event-stream`, the WHATWG HTML "event stream" format).
//!
//! [Decision Log]
//! - Purpose: each model API streams its response as SSE; the proxy reads
//!   one provider's stream and writes another (or the same) to the harness.
//!   Framing bugs here corrupt every translation above, so the decoder is
//!   its own small, pure piece, tested against every way a stream can be
//!   cut into chunks.
//! - Constraint: an upstream's bytes arrive in arbitrary chunks (a line,
//!   an event, a CR and its LF can each be split), and a hostile or broken
//!   upstream may send a line that never ends.
//! - Choice: an incremental decoder fed chunk by chunk, holding at most one
//!   partial line and one event in progress, both bounded
//!   ([`SseDecoder::new`]); lines end in CRLF, LF or CR, also across
//!   chunks; a leading byte-order mark is skipped; comments are reported
//!   (they keep a connection alive, which the proxy's stall watchdog
//!   counts); `id` and `retry` fields are ignored (no model API resumes a
//!   stream by id); other fields are ignored, as the format says.
//! - Deviation from the format, on purpose: at the end of the stream an
//!   event still in progress is dispatched rather than dropped
//!   ([`SseDecoder::finish`]). Providers do end a stream without the last
//!   blank line, and the proxy decides from the event's content whether
//!   the stream ended properly (opencodex learned this the same way).
//! - Encoding writes `data:` once per line of the payload, so a payload
//!   with newlines survives, and refuses an event name with a line break.

use bytes::{BufMut as _, Bytes, BytesMut};

/// The default bound on one event: a model's single delta is small, but a
/// tool call's arguments or a final message snapshot can be large.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Its `event:` field, if it had one.
    pub event: Option<String>,
    /// Its `data:` lines, joined with `\n`.
    pub data: String,
}

/// What a decoder produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFrame {
    /// An event with data.
    Event(SseEvent),
    /// A comment line (`:` and anything): a keepalive, nothing to act on.
    Comment,
}

/// Why a stream cannot be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SseError {
    /// A line, or an event's data, grew past the decoder's bound.
    #[error("an event in the stream is larger than {limit} bytes")]
    TooLarge {
        /// The bound.
        limit: usize,
    },
    /// A line is not UTF-8.
    #[error("the stream is not UTF-8")]
    NotUtf8,
}

/// An incremental decoder: feed it chunks as they arrive, then
/// [`finish`](Self::finish) it at the end of the stream.
#[derive(Debug)]
pub struct SseDecoder {
    /// The bound on a line and on an event's data.
    limit: usize,
    /// The current line, not yet ended.
    line: Vec<u8>,
    /// The previous chunk ended in CR: a LF starting this one belongs to it.
    after_cr: bool,
    /// Nothing decoded yet: a byte-order mark may come first.
    at_start: bool,
    /// The event in progress.
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseDecoder {
    /// A decoder refusing a line or an event larger than `limit` bytes
    /// ([`DEFAULT_MAX_EVENT_BYTES`] suits model streams).
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            line: Vec::new(),
            after_cr: false,
            at_start: true,
            event: None,
            data: String::new(),
            has_data: false,
        }
    }

    /// Decode `chunk`, appending every frame it completes to `frames`.
    ///
    /// # Errors
    /// A line or an event past the bound, or a line that is not UTF-8. The
    /// decoder is not usable after an error.
    pub fn feed(&mut self, chunk: &[u8], frames: &mut Vec<SseFrame>) -> Result<(), SseError> {
        let mut chunk = chunk;
        if self.at_start && !chunk.is_empty() {
            self.at_start = false;
            chunk = chunk.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(chunk);
        }
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.after_cr {
                self.after_cr = false;
                if let Some(after) = rest.strip_prefix(b"\n") {
                    rest = after;
                    continue;
                }
            }
            let Some(end) = rest
                .iter()
                .position(|byte| *byte == b'\n' || *byte == b'\r')
            else {
                self.extend_line(rest)?;
                break;
            };
            let (line, tail) = rest.split_at(end);
            self.extend_line(line)?;
            // `tail` starts with the line break.
            let (ending, after) = tail.split_at(1);
            self.after_cr = ending == b"\r";
            rest = after;
            let line = std::mem::take(&mut self.line);
            self.take_line(&line, frames)?;
        }
        Ok(())
    }

    /// The end of the stream: the last line, even without its line break,
    /// and the event in progress, even without its blank line.
    ///
    /// # Errors
    /// As for [`feed`](Self::feed).
    pub fn finish(mut self, frames: &mut Vec<SseFrame>) -> Result<(), SseError> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.take_line(&line, frames)?;
        }
        self.dispatch(frames);
        Ok(())
    }

    fn extend_line(&mut self, bytes: &[u8]) -> Result<(), SseError> {
        if self.line.len().saturating_add(bytes.len()) > self.limit {
            return Err(SseError::TooLarge { limit: self.limit });
        }
        self.line.extend_from_slice(bytes);
        Ok(())
    }

    /// One complete line.
    fn take_line(&mut self, line: &[u8], frames: &mut Vec<SseFrame>) -> Result<(), SseError> {
        if line.is_empty() {
            self.dispatch(frames);
            return Ok(());
        }
        if line.starts_with(b":") {
            frames.push(SseFrame::Comment);
            return Ok(());
        }
        let line = std::str::from_utf8(line).map_err(|_invalid| SseError::NotUtf8)?;
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                let grown = self
                    .data
                    .len()
                    .saturating_add(value.len())
                    .saturating_add(1);
                if grown > self.limit {
                    return Err(SseError::TooLarge { limit: self.limit });
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            // `id` and `retry` mean nothing to a model stream; anything
            // else is ignored, as the format says.
            _ => {}
        }
        Ok(())
    }

    /// End the event in progress: dispatched if it has data, dropped (with
    /// its name) if not, as the format says.
    fn dispatch(&mut self, frames: &mut Vec<SseFrame>) {
        let event = self.event.take();
        if !self.has_data {
            return;
        }
        self.has_data = false;
        frames.push(SseFrame::Event(SseEvent {
            event,
            data: std::mem::take(&mut self.data),
        }));
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_EVENT_BYTES)
    }
}

/// Why an event cannot be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// The event's name holds a line break, which would end its field.
    #[error("an event name cannot hold a line break")]
    NameLineBreak,
}

/// `event` (if any) and `data` as one SSE event, ending in its blank line.
/// A payload with line breaks becomes several `data:` lines, which decode
/// back to the same payload.
///
/// # Errors
/// The event's name holds a line break.
pub fn encode_event(event: Option<&str>, data: &str) -> Result<Bytes, EncodeError> {
    let mut out = BytesMut::with_capacity(data.len().saturating_add(32));
    if let Some(event) = event {
        if event.contains(['\n', '\r']) {
            return Err(EncodeError::NameLineBreak);
        }
        out.put_slice(b"event: ");
        out.put_slice(event.as_bytes());
        out.put_u8(b'\n');
    }
    // `lines` would drop a trailing empty line; split keeps every line.
    for line in data.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.put_slice(b"data: ");
        out.put_slice(line.as_bytes());
        out.put_u8(b'\n');
    }
    out.put_u8(b'\n');
    Ok(out.freeze())
}

/// A comment line, for a keepalive.
#[must_use]
pub fn encode_comment(text: &str) -> Bytes {
    let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    Bytes::from(format!(": {text}\n\n"))
}

#[cfg(test)]
mod tests;
