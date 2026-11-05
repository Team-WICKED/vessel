//! The node's access token (ADR 0005).

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use subtle::ConstantTimeEq as _;

/// Random bytes in a token: 256 bits, as ADR 0005 specifies.
const TOKEN_BYTES: usize = 32;

/// The secret every client presents as `Authorization: Bearer <token>`.
///
/// `Debug` never prints it and there is no `Display`, so it cannot end up in
/// a log line by accident. [`expose`](Self::expose) is the one way to get
/// the text, for writing it into a request header.
#[derive(Clone, PartialEq, Eq)]
pub struct NodeToken(String);

/// Text that is not a token Vessel could have generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a Vessel node token (expected 43 base64url characters)")]
pub struct InvalidToken;

impl NodeToken {
    /// A fresh random token.
    pub(crate) fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// The token text, for an `Authorization` header. Never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `presented` is this token. Takes the same time wherever the
    /// first difference is, so response timing does not leak the token one
    /// byte at a time. (It does reveal a length mismatch, and every token
    /// has the same public length.)
    #[must_use]
    pub fn verify(&self, presented: &str) -> bool {
        self.0.as_bytes().ct_eq(presented.as_bytes()).into()
    }
}

impl FromStr for NodeToken {
    type Err = InvalidToken;

    /// Accepts exactly what [`generate`](Self::generate) produces (surrounding
    /// whitespace aside, so a hand-edited file with a newline still works).
    fn from_str(text: &str) -> Result<Self, InvalidToken> {
        let text = text.trim();
        let decoded = URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|_not_base64| InvalidToken)?;
        if decoded.len() == TOKEN_BYTES {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidToken)
        }
    }
}

impl fmt::Debug for NodeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_parse_and_verify() {
        let token = NodeToken::generate().unwrap();
        assert_eq!(token.expose().len(), 43);
        assert_eq!(token.expose().parse::<NodeToken>().unwrap(), token);
        assert!(token.verify(token.expose()));
        assert!(!token.verify(""));
        let mut other = token.expose().to_owned();
        let last = other.pop().unwrap();
        other.push(if last == 'A' { 'B' } else { 'A' });
        assert!(!token.verify(&other));
        assert_ne!(NodeToken::generate().unwrap(), token);
    }

    #[test]
    fn debug_hides_the_secret() {
        let token = NodeToken::generate().unwrap();
        assert_eq!(format!("{token:?}"), "NodeToken(..)");
    }

    #[test]
    fn rejects_other_text() {
        assert_eq!("short".parse::<NodeToken>(), Err(InvalidToken));
        assert_eq!("".parse::<NodeToken>(), Err(InvalidToken));
    }
}
