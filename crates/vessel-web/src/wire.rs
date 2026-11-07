//! The frames between the browser and `vessel ui` (ADR 0017): JSON text
//! frames on `/api/ws`, one message each, and the HTTP API's bodies.
//!
//! The browser's first frame must be [`ClientFrame::Hello`] with the
//! launch's token, within a short deadline; anything else closes the
//! socket with [`CLOSE_UNAUTHORIZED`].

use std::fmt;

use serde::{Deserialize, Serialize};

/// Closes a websocket that sent no valid token in time.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// A frame from the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// The first frame: the launch's token.
    Hello {
        /// The token from the page's URL.
        token: PresentedToken,
    },
}

/// A frame to the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The token was right: the socket is the browser's.
    Welcome {
        /// The server's version (`vessel`'s).
        version: String,
    },
}

/// `GET /api/status`: what is serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The server's version (`vessel`'s).
    pub version: String,
    /// Whether the real UI is embedded (else the placeholder page).
    pub bundled: bool,
}

/// A token as a browser presented it: not yet checked, never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PresentedToken(String);

impl PresentedToken {
    /// The token as given.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The text, for comparing.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PresentedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PresentedToken(..)")
    }
}
