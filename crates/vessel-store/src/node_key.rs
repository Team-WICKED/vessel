//! The node's static key material (ADR 0009): `node.key`, 32 random bytes,
//! owner-only, created on first start.
//!
//! [Decision Log]
//! - Purpose: the secret half of the node's identity on secure channels.
//!   Clients pin the public half at pairing (ADR 0010).
//! - Constraint: this crate stores the bytes and nothing more. Deriving the
//!   X25519 public key is `vessel-secure`'s job (plan, "Crates"), so the
//!   store needs no crypto dependency.
//! - Format: exactly the 32 raw bytes. The token is text because people
//!   paste it into headers; the key is never typed, and a length check is
//!   its whole validation.
//! - Never replaced: a file of the wrong size is an error, not a reason to
//!   make a new key. A new key would silently unpair every device (ADR 0009:
//!   losing `node.key` means pairing them all again), so that has to be the
//!   operator's decision.
//! - Cost: the bytes sit in memory for the node's life without being wiped;
//!   `vessel-secure`'s keypair, which does the cryptography, zeroizes its own
//!   copy.

use std::fmt;

/// Bytes in a node key: one X25519 secret.
pub const NODE_KEY_BYTES: usize = 32;

/// The node's secret key bytes. `Debug` never prints them and there is no
/// `Display`; [`expose`](Self::expose) is the one way to get them, for
/// `vessel-secure`.
#[derive(Clone)]
pub struct NodeKeySecret([u8; NODE_KEY_BYTES]);

impl NodeKeySecret {
    /// Fresh key bytes from the operating system's random source.
    pub(crate) fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; NODE_KEY_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// The key as stored, if `bytes` has the right length.
    pub(crate) fn from_stored(bytes: &[u8]) -> Option<Self> {
        <[u8; NODE_KEY_BYTES]>::try_from(bytes).ok().map(Self)
    }

    /// The secret bytes. Never log them.
    #[must_use]
    pub const fn expose(&self) -> &[u8; NODE_KEY_BYTES] {
        &self.0
    }
}

impl fmt::Debug for NodeKeySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeKeySecret(<redacted>)")
    }
}
