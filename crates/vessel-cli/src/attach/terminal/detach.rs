//! Finding the detach key (`Ctrl-]`) in the local terminal's keystrokes.
//!
//! [Decision Log]
//! - Purpose: `Ctrl-]` must detach in every keyboard mode a harness can put
//!   the local terminal in. Attach passes the harness's output through
//!   unchanged, so the harness's mode requests reach the local terminal.
//! - Constraint: Codex pushes the kitty keyboard protocol's flags; the
//!   terminal then reports `Ctrl-]` as `CSI 93;5u` (or `CSI 93;5:1u` with
//!   event types, `CSI 93:125;5u` with alternate keys), never as the byte
//!   0x1d. xterm's `modifyOtherKeys` reports it as `CSI 27;5;93~`. Input
//!   arrives in chunks of whatever a read returned, so a sequence can be
//!   split across chunks.
//! - Choice: a scanner over the input that recognises the byte and both
//!   encodings (lock keys such as Num Lock ignored, key presses only, so a
//!   release report cannot detach a second time), and holds back the tail
//!   of a chunk that could still become one. The caller forwards held bytes
//!   when nothing follows within [`ESCAPE_WAIT`], as terminals do to tell a
//!   lone Escape from the start of a sequence.
//! - Alternatives: matching only 0x1d (detach silently stops working under
//!   Codex); turning the protocols off locally (the harness enabled them for
//!   its own use and relies on them).
//! - Cost: a lone Escape (or `Alt-[`) at the very end of a read reaches the
//!   harness up to [`ESCAPE_WAIT`] late.

use std::time::Duration;

use bytes::Bytes;

/// How long held bytes wait for the rest of a possible sequence. Bytes of
/// one key report arrive together in practice; this only covers a report
/// split by a full read buffer, while keeping a lone Escape responsive.
pub(super) const ESCAPE_WAIT: Duration = Duration::from_millis(50);

/// `Ctrl-]` as a plain byte.
const DETACH: u8 = 0x1d;
const ESC: u8 = 0x1b;
/// The `]` key's code in kitty and `modifyOtherKeys` reports.
const BRACKET: u32 = 93;
/// The kitty modifier bits of Caps Lock and Num Lock, which do not change
/// what the key means here.
const LOCKS: u32 = 64 | 128;
/// The kitty modifier bit of Ctrl.
const CTRL: u32 = 4;
/// Longest sequence held back while it may still turn into a detach key
/// (the longest form above with every optional field is under 32 bytes).
const LONGEST: usize = 32;

/// What a chunk of input amounts to.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Scanned {
    /// Keystrokes to forward (everything before a detach key).
    pub(super) keys: Bytes,
    /// A detach key was found; the input after it is dropped.
    pub(super) detach: bool,
}

/// Scans keystrokes for the detach key across chunks.
#[derive(Debug, Default)]
pub(super) struct Scanner {
    /// The start of a sequence that may still become a detach key.
    held: Vec<u8>,
}

/// What the bytes at an `ESC` are.
enum Sequence {
    /// A complete detach key.
    Detach,
    /// Could still become one; more input is needed.
    Incomplete,
    /// Not a detach key; scanning goes on this many bytes later.
    Other(usize),
}

impl Scanner {
    /// Scan the next chunk.
    pub(super) fn feed(&mut self, chunk: &[u8]) -> Scanned {
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(chunk);
        let mut at = 0_usize;
        while let Some(&byte) = data.get(at) {
            let step = match byte {
                DETACH => return detach_at(data, at),
                ESC => match sequence(data.get(at..).unwrap_or_default()) {
                    Sequence::Detach => return detach_at(data, at),
                    Sequence::Incomplete => {
                        self.held = data.split_off(at);
                        return Scanned {
                            keys: Bytes::from(data),
                            detach: false,
                        };
                    }
                    Sequence::Other(length) => length,
                },
                _ => 1,
            };
            at = at.saturating_add(step);
        }
        Scanned {
            keys: Bytes::from(data),
            detach: false,
        }
    }

    /// Whether bytes are held back waiting for more input.
    pub(super) const fn is_holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// Give up waiting: the held bytes are ordinary keystrokes.
    pub(super) fn release(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.held))
    }
}

/// The input before `at`, which holds a detach key.
fn detach_at(mut data: Vec<u8>, at: usize) -> Scanned {
    data.truncate(at);
    Scanned {
        keys: Bytes::from(data),
        detach: true,
    }
}

/// Classify the bytes starting at an `ESC`.
fn sequence(bytes: &[u8]) -> Sequence {
    match bytes.get(1) {
        None => return Sequence::Incomplete,
        Some(b'[') => {}
        Some(_) => return Sequence::Other(1),
    }
    let parameters = bytes.get(2..).unwrap_or_default();
    let length = parameters
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        .count();
    let (Some(parameters), Some(&last)) = (parameters.get(..length), parameters.get(length)) else {
        // Only parameter bytes so far: wait for the rest, within reason.
        return if bytes.len() <= LONGEST {
            Sequence::Incomplete
        } else {
            Sequence::Other(1)
        };
    };
    let detach = match last {
        b'u' => kitty_detach(parameters),
        b'~' => modify_other_keys_detach(parameters),
        _ => false,
    };
    if detach {
        Sequence::Detach
    } else {
        // ESC, `[`, the parameters and the final byte.
        Sequence::Other(length.saturating_add(3))
    }
}

/// `CSI key[:alternates] ; modifiers[:event] [; text] u` for `]` with Ctrl,
/// pressed.
fn kitty_detach(parameters: &[u8]) -> bool {
    let mut fields = parameters.split(|&byte| byte == b';');
    let key = fields.next().and_then(|field| number(sub(field, 0)));
    let Some(modifiers) = fields.next() else {
        return false;
    };
    let pressed = match sub(modifiers, 1) {
        [] => true,
        event => number(event) == Some(1),
    };
    key == Some(BRACKET) && ctrl_only(number(sub(modifiers, 0))) && pressed
}

/// `CSI 27 ; modifiers ; key ~` for `]` with Ctrl.
fn modify_other_keys_detach(parameters: &[u8]) -> bool {
    let mut fields = parameters.split(|&byte| byte == b';');
    let (Some(prefix), Some(modifiers), Some(key), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    number(prefix) == Some(27) && ctrl_only(number(modifiers)) && number(key) == Some(BRACKET)
}

/// The `index`th `:`-separated part of a field.
fn sub(field: &[u8], index: usize) -> &[u8] {
    field
        .split(|&byte| byte == b':')
        .nth(index)
        .unwrap_or_default()
}

/// A decimal parameter; `None` when empty or too large.
fn number(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Whether a kitty-style modifier value (one plus the modifier bits) means
/// Ctrl and nothing else besides lock keys.
fn ctrl_only(modifiers: Option<u32>) -> bool {
    modifiers
        .and_then(|value| value.checked_sub(1))
        .is_some_and(|bits| bits & !LOCKS == CTRL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut scanner = Scanner::default();
        let mut keys = Vec::new();
        for chunk in chunks {
            let typed = scanner.feed(chunk);
            keys.extend_from_slice(&typed.keys);
            if typed.detach {
                return (keys, true);
            }
        }
        keys.extend_from_slice(&scanner.release());
        (keys, false)
    }

    const DETACH_KEYS: [&[u8]; 7] = [
        b"\x1d",
        b"\x1b[93;5u",
        b"\x1b[93;5:1u",
        b"\x1b[93:125;5u",
        b"\x1b[93;133u",
        b"\x1b[93;5;29u",
        b"\x1b[27;5;93~",
    ];

    #[test]
    fn every_encoding_of_ctrl_bracket_detaches_and_keeps_the_keys_before_it() {
        for detach in DETACH_KEYS {
            let mut input = b"ls\r".to_vec();
            input.extend_from_slice(detach);
            input.extend_from_slice(b"after");
            assert_eq!(
                scan(&[&input]),
                (b"ls\r".to_vec(), true),
                "{}",
                String::from_utf8_lossy(detach)
            );
        }
    }

    #[test]
    fn a_detach_key_split_across_chunks_is_still_found() {
        for detach in DETACH_KEYS {
            let mut input = b"ab".to_vec();
            input.extend_from_slice(detach);
            for cut in 1..input.len() {
                let (first, second) = input.split_at(cut);
                assert_eq!(
                    scan(&[first, second]),
                    (b"ab".to_vec(), true),
                    "{} cut at {cut}",
                    String::from_utf8_lossy(detach)
                );
            }
        }
    }

    #[test]
    fn other_keys_pass_through_unchanged() {
        for keys in [
            &b"plain text"[..],
            b"\x1b[A\x1b[B",
            b"\x1b[93u",
            b"\x1b[93;3u",
            b"\x1b[93;5:3u",
            b"\x1b[93;6u",
            b"\x1b[94;5u",
            b"\x1b[27;5;94~",
            b"\x1b[27;6;93~",
            b"\x1b[200~pasted\x1b[201~",
            b"\x1b[<0;12;5M",
            b"\x1bOP",
            b"\x1b",
            b"\x1b[",
        ] {
            assert_eq!(scan(&[keys]), (keys.to_vec(), false), "{keys:?}");
        }
    }

    #[test]
    fn a_possible_sequence_is_held_until_the_next_chunk_or_released() {
        let mut scanner = Scanner::default();
        let typed = scanner.feed(b"x\x1b[93");
        assert_eq!(&typed.keys[..], b"x");
        assert!(!typed.detach);
        assert!(scanner.is_holding());
        assert_eq!(&scanner.release()[..], b"\x1b[93");
        assert!(!scanner.is_holding());

        // A lone Escape at the end of a read is held, then released.
        assert_eq!(&scanner.feed(b"\x1b").keys[..], b"");
        assert_eq!(&scanner.release()[..], b"\x1b");
        // An over-long parameter run is not held.
        let long = [b"\x1b[".as_slice(), &[b'1'; 40]].concat();
        assert_eq!(&scanner.feed(&long).keys[..], &long[..]);
        assert!(!scanner.is_holding());
    }
}
