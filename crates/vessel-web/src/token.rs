//! The per-launch token a browser presents to `vessel ui` (ADR 0017).

use std::fmt::{self, Write as _};
use std::net::SocketAddr;
use std::sync::Arc;

use subtle::ConstantTimeEq as _;

/// The secret a browser presents to `vessel ui`: 256 random bits, as hex,
/// minted for one launch and never written to disk. It reaches the page in
/// the URL's fragment, which browsers send to no server.
#[derive(Clone)]
pub struct LaunchToken(Arc<str>);

/// The system's random number generator failed.
#[derive(Debug, thiserror::Error)]
#[error("no randomness for the UI's token")]
pub struct MintError(#[source] getrandom::Error);

impl LaunchToken {
    /// A fresh token.
    ///
    /// # Errors
    /// The system's random number generator failed.
    pub fn mint() -> Result<Self, MintError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(MintError)?;
        let hex = bytes
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                let _written = write!(hex, "{byte:02x}");
                hex
            });
        Ok(Self(hex.into()))
    }

    /// Whether `presented` is this token, compared in constant time (its
    /// length is not secret).
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        self.0.as_bytes().ct_eq(presented.as_bytes()).into()
    }

    /// The address to open: the UI at `address`, with the token in the
    /// fragment.
    #[must_use]
    pub fn url(&self, address: SocketAddr) -> String {
        format!("http://{address}/#token={}", self.0)
    }
}

impl fmt::Debug for LaunchToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_fresh_hex_and_compared_whole() {
        let one = LaunchToken::mint().unwrap();
        let two = LaunchToken::mint().unwrap();
        assert_eq!(one.0.len(), 64);
        assert!(one.0.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(one.matches(&one.0));
        assert!(!one.matches(&two.0));
        assert!(!one.matches(""));
        let shorter: String = one.0.chars().take(63).collect();
        assert!(!one.matches(&shorter));
        assert_eq!(format!("{one:?}"), "LaunchToken(..)");
        let url = one.url("127.0.0.1:7079".parse().unwrap());
        assert_eq!(url, format!("http://127.0.0.1:7079/#token={}", one.0));
        let six = one.url("[::1]:7079".parse().unwrap());
        assert!(six.starts_with("http://[::1]:7079/#token="));
    }
}
