//! The Windows pseudo console's startup cursor query, answered here.
//!
//! [Decision Log]
//! - Purpose: `portable-pty` creates the pseudo console with
//!   `PSEUDOCONSOLE_INHERIT_CURSOR`. The console then writes a cursor
//!   position query (`ESC[6n`) as its first output and renders nothing
//!   until a report (`ESC[row;colR`) arrives on its input. A terminal
//!   emulator answers at once; a session nobody watches (every session
//!   started detached, every test) showed no output at all, so on Windows
//!   no terminal session ever got past its program's first line.
//! - Alternatives: create the console without the flag (needs a fork of
//!   `portable-pty`, or our own `ConPTY` FFI with `unsafe`); let the first
//!   attached client's terminal answer (output stays blocked until someone
//!   attaches, and a client attaching late answers a query that may no
//!   longer be pending).
//! - Choice: answer that one query ourselves, with the cursor at the top
//!   left (a new console's cursor), and drop it from the output, so a
//!   client replaying the session never sees it and never answers again.
//! - Constraint: only the console's own query is answered, never a
//!   program's. The console sends it before it renders anything a program
//!   wrote, so the first query in the stream is the console's. Watching
//!   stops after that query, or after [`WATCH_LIMIT`] bytes without one
//!   (a console that does not ask), and queries after that pass through to
//!   whichever terminal is attached.
//!
//! Pure byte handling, compiled on every platform so its tests run
//! everywhere; only the Windows reader uses it.

/// The query: Device Status Report, cursor position.
const QUERY: &[u8] = b"\x1b[6n";

/// The report sent back: row 1, column 1.
#[cfg_attr(
    not(windows),
    expect(dead_code, reason = "only the Windows reader sends it")
)]
pub(crate) const REPLY: &[u8] = b"\x1b[1;1R";

/// Output bytes watched for the query before giving up.
const WATCH_LIMIT: usize = 4096;

/// Filters the start of a pseudo console's output.
#[derive(Debug, Default)]
pub(crate) struct CursorQuery {
    /// A trailing part of the last chunk that may be the start of the
    /// query (at most three bytes), held back until the next chunk.
    held: Vec<u8>,
    /// Output bytes watched so far.
    watched: usize,
    /// The query was answered, or watching gave up.
    done: bool,
}

/// What to do with one chunk of output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Filtered {
    /// The bytes to publish (possibly empty).
    pub(crate) output: Vec<u8>,
    /// Whether to write [`REPLY`] to the console's input now.
    pub(crate) answer: bool,
}

impl CursorQuery {
    /// Whether chunks still need [`feed`](Self::feed).
    pub(crate) const fn watching(&self) -> bool {
        !self.done
    }

    /// Filter one chunk of output.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Filtered {
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(chunk);
        if self.done {
            return Filtered {
                output: data,
                answer: false,
            };
        }
        if let Some(at) = data.windows(QUERY.len()).position(|window| window == QUERY) {
            self.done = true;
            drop(data.drain(at..at.saturating_add(QUERY.len())));
            return Filtered {
                output: data,
                answer: true,
            };
        }
        self.watched = self.watched.saturating_add(chunk.len());
        if self.watched >= WATCH_LIMIT {
            self.done = true;
            return Filtered {
                output: data,
                answer: false,
            };
        }
        let keep = partial_query_at_end(&data);
        self.held = data.split_off(data.len().saturating_sub(keep));
        Filtered {
            output: data,
            answer: false,
        }
    }

    /// Bytes still held back when the output ends.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        self.done = true;
        std::mem::take(&mut self.held)
    }
}

/// Length of the longest end of `data` that is a proper start of the query.
fn partial_query_at_end(data: &[u8]) -> usize {
    (1..QUERY.len())
        .rev()
        .find(|&length| {
            QUERY
                .get(..length)
                .is_some_and(|start| data.ends_with(start))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passed(output: &[u8]) -> Filtered {
        Filtered {
            output: output.to_vec(),
            answer: false,
        }
    }

    #[test]
    fn the_startup_query_is_answered_once_and_removed() {
        let mut query = CursorQuery::default();
        assert_eq!(
            query.feed(b"\x1b[?9001h\x1b[6nready\r\n"),
            Filtered {
                output: b"\x1b[?9001hready\r\n".to_vec(),
                answer: true,
            }
        );
        assert!(!query.watching());
        // A program's own query later goes to the attached terminal.
        assert_eq!(query.feed(b"\x1b[6n"), passed(b"\x1b[6n"));
    }

    #[test]
    fn a_query_split_across_reads_is_still_found() {
        let mut query = CursorQuery::default();
        assert_eq!(query.feed(b"ab\x1b["), passed(b"ab"));
        assert_eq!(
            query.feed(b"6ncd"),
            Filtered {
                output: b"cd".to_vec(),
                answer: true,
            }
        );
    }

    #[test]
    fn held_bytes_that_are_not_the_query_are_released() {
        let mut query = CursorQuery::default();
        assert_eq!(query.feed(b"x\x1b"), passed(b"x"));
        assert_eq!(query.feed(b"[0m"), passed(b"\x1b[0m"));
        assert_eq!(query.feed(b"\x1b[6"), passed(b""));
        assert_eq!(query.finish(), b"\x1b[6".to_vec());
    }

    #[test]
    fn watching_stops_after_the_limit() {
        let mut query = CursorQuery::default();
        let plain = vec![b'.'; WATCH_LIMIT];
        assert_eq!(query.feed(&plain), passed(&plain));
        assert!(!query.watching());
        assert_eq!(query.feed(b"\x1b[6n"), passed(b"\x1b[6n"));
    }
}
