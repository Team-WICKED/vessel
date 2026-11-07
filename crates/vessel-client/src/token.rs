//! The credential a client presents to a node.

use std::fmt;
use std::str::FromStr;

/// A node's bearer token as a client holds it: from `--token`,
/// `VESSEL_TOKEN`, or the node's token file.
///
/// `Debug` never prints it and there is no `Display`, so it cannot end up in
/// a log line by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

/// Text that cannot be a bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a token must be non-empty printable ASCII without spaces")]
pub struct InvalidToken;

impl BearerToken {
    /// The token text, for the `Authorization` header. Never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl FromStr for BearerToken {
    type Err = InvalidToken;

    /// Any non-empty run of visible ASCII (what an HTTP header value can
    /// carry unescaped), surrounding whitespace removed. The node decides
    /// whether it is the right one.
    fn from_str(text: &str) -> Result<Self, InvalidToken> {
        let text = text.trim();
        if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_graphic()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidToken)
        }
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(..)")
    }
}
