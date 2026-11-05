//! Errors as they appear on the wire.
//!
//! Each crate keeps its own typed error enum and maps it to a [`WireError`]
//! exactly once, at the RPC boundary. Clients branch on [`ErrorCode`], never
//! on the message text (Zed's string-only `bail!` errors all arrive as
//! "internal" and cannot be acted on; this is the fix).

use std::fmt;

use serde::{Deserialize, Serialize};

/// What kind of failure occurred. Clients branch on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was malformed or its arguments were invalid.
    InvalidRequest,
    /// Missing or wrong credentials.
    Unauthorized,
    /// The client and node share no protocol version.
    ProtocolMismatch,
    /// The referenced session, subscription or permission does not exist.
    NotFound,
    /// The target is in the wrong state (for example the session has ended).
    Conflict,
    /// The operation does not apply (for example terminal input sent to a
    /// structured session, or a harness without structured support).
    Unsupported,
    /// The harness binary (or its adapter) is not installed on the node.
    HarnessNotInstalled,
    /// The harness process could not be started.
    SpawnFailed,
    /// The node is shutting down or overloaded; retrying later may work.
    Unavailable,
    /// A bug or an unexpected I/O failure inside the node.
    Internal,
}

/// An error returned in place of a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    /// Machine-readable kind.
    pub code: ErrorCode,
    /// Human-readable explanation. Never contains secrets.
    pub message: String,
}

impl WireError {
    /// Build an error.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}
