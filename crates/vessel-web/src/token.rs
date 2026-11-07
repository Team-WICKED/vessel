//! The per-launch token a browser presents to `vessel ui` (ADR 0017).

use std::fmt::{self, Write as _};
use std::net::SocketAddr;
use std::sync::Arc;

use subtle::ConstantTimeEq as _;

/// The secret a browser presents to `vessel ui`: 256 random bits, as hex,
/// minted for a one-time bootstrap. The private browser handoff file and
/// terminal carry it; it reaches the page in
/// the URL's fragment, which browsers send to no server.
#[derive(Clone)]
pub struct LaunchToken {
    secret: Arc<str>,
    host: Arc<str>,
}

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
        let mut bytes = [0_u8; 48];
        getrandom::fill(&mut bytes).map_err(MintError)?;
        let hex = |bytes: &[u8]| {
            bytes.iter().fold(String::new(), |mut hex, byte| {
                let _written = write!(hex, "{byte:02x}");
                hex
            })
        };
        // A separate 128-bit hostname makes the browser origin fresh even
        // when the port is reused. The URL code retains its full 256 bits.
        let (secret, host) = bytes.split_at(32);
        Ok(Self {
            secret: hex(secret).into(),
            host: format!("vessel-{}.localhost", hex(host)).into(),
        })
    }

    pub(crate) fn expose(&self) -> &str {
        &self.secret
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// Whether `presented` is this token, compared in constant time (its
    /// length is not secret).
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        self.secret.as_bytes().ct_eq(presented.as_bytes()).into()
    }

    /// The address to open: the UI at `address`, with the token in the
    /// fragment.
    #[must_use]
    pub fn url(&self, address: SocketAddr) -> String {
        format!(
            "http://{}:{}/#bootstrap={}",
            self.host,
            address.port(),
            self.secret
        )
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
        assert_eq!(one.secret.len(), 64);
        assert!(one.secret.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(one.matches(&one.secret));
        assert!(!one.matches(&two.secret));
        assert!(!one.matches(""));
        let shorter: String = one.secret.chars().take(63).collect();
        assert!(!one.matches(&shorter));
        assert_eq!(format!("{one:?}"), "LaunchToken(..)");
        let url = one.url("127.0.0.1:7079".parse().unwrap());
        assert_eq!(
            url,
            format!("http://{}:7079/#bootstrap={}", one.host, one.secret)
        );
        assert_ne!(one.host, two.host);
        assert!(!one.host.contains(one.secret.as_ref()));
        let six = one.url("[::1]:7079".parse().unwrap());
        assert_eq!(six, url);
    }
}
