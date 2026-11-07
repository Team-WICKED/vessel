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
//! - Wiped: the bytes live in one heap allocation that `zeroize` clears
//!   when the secret is dropped, and so does the buffer the file is read
//!   into (`DataDir::read_node_key`). Boxed so that moving the secret (out
//!   of `load_or_create_node_key`, into the node's start-up) moves a
//!   pointer, not the bytes: a moved array leaves a copy behind that no
//!   destructor sees. The rest is out of this crate's reach: the copy
//!   handed to `NodeKeypair::from_secret`, which takes the array by value
//!   and wipes its own copy, and the kernel's page cache of the file.

use std::fmt;

use zeroize::Zeroizing;

/// Bytes in a node key: one X25519 secret.
pub const NODE_KEY_BYTES: usize = 32;

/// The node's secret key bytes, wiped from memory when dropped. `Debug`
/// never prints them and there is no `Display`; [`expose`](Self::expose) is
/// the one way to get them, for `vessel-secure`.
#[derive(Clone)]
pub struct NodeKeySecret(Box<Zeroizing<[u8; NODE_KEY_BYTES]>>);

impl NodeKeySecret {
    /// Fresh key bytes from the operating system's random source, written
    /// straight into their final place.
    pub(crate) fn generate() -> Result<Self, getrandom::Error> {
        let mut key = Self::zeroed();
        getrandom::fill(key.0.as_mut_slice())?;
        Ok(key)
    }

    /// The key as stored, if `bytes` has the right length.
    pub(crate) fn from_stored(bytes: &[u8]) -> Option<Self> {
        let bytes = <&[u8; NODE_KEY_BYTES]>::try_from(bytes).ok()?;
        let mut key = Self::zeroed();
        key.0.copy_from_slice(bytes);
        Some(key)
    }

    fn zeroed() -> Self {
        Self(Box::new(Zeroizing::new([0_u8; NODE_KEY_BYTES])))
    }

    /// The secret bytes. Never log them.
    #[must_use]
    pub fn expose(&self) -> &[u8; NODE_KEY_BYTES] {
        &self.0
    }
}

impl fmt::Debug for NodeKeySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeKeySecret(<redacted>)")
    }
}
