//! Terminal-mode payloads: raw PTY bytes and window size.

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Terminal window size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TerminalSize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

impl Default for TerminalSize {
    /// 80x24, the size every terminal program assumes when told nothing.
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// Raw bytes to or from a PTY.
///
/// [Decision Log]
/// - Purpose: PTY output is arbitrary bytes. A read can end in the middle of
///   a UTF-8 sequence or an escape sequence, so it cannot be a `String`.
/// - Alternatives: a JSON array of numbers (4x size); lossy UTF-8 (corrupts
///   split characters); separate binary websocket frames (a second framing
///   scheme next to the typed JSON frames, and harder to relay and to log).
/// - Choice: `Bytes` in memory (cheap to fan out), base64 text on the wire
///   and in the session log.
/// - Cost: about 33% size overhead on terminal traffic. Revisit with a binary
///   codec if terminal bandwidth ever matters; the type does not change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct TerminalBytes(Bytes);

impl TerminalBytes {
    /// Wrap bytes.
    #[must_use]
    pub const fn new(bytes: Bytes) -> Self {
        Self(bytes)
    }

    /// The bytes.
    #[must_use]
    pub const fn bytes(&self) -> &Bytes {
        &self.0
    }

    /// Unwrap into the underlying buffer.
    #[must_use]
    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

impl From<Bytes> for TerminalBytes {
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl Serialize for TerminalBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::base64_bytes::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for TerminalBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::base64_bytes::deserialize(deserializer).map(Self)
    }
}

/// Input to a terminal-mode session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalInput {
    /// Keystrokes or pasted text, written to the PTY verbatim.
    Data {
        /// The bytes.
        data: TerminalBytes,
    },
    /// The client's window changed size.
    Resize {
        /// New size.
        size: TerminalSize,
    },
}

/// A chunk of PTY output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TerminalOutput {
    /// The bytes, as read from the PTY.
    pub data: TerminalBytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_survive_split_utf8() {
        // "한" is three bytes; keep only the first two, as a PTY read might.
        let partial = TerminalBytes::new(Bytes::from_static(&[0xED, 0x95]));
        let json = serde_json::to_string(&partial).unwrap();
        assert_eq!(json, r#""7ZU=""#);
        assert_eq!(
            serde_json::from_str::<TerminalBytes>(&json).unwrap(),
            partial
        );
    }
}
