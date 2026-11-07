//! The node's static key pair, and what is computed from its public half.
//!
//! [Decision Log]
//! - Purpose: a node is reached by its X25519 public key (ADR 0009). The
//!   key's bytes and text form are wire data in `vessel-proto`
//!   ([`NodePublicKey`], [`RoutingId`]); this module owns the secret half
//!   and everything derived with cryptography: the public key from the
//!   secret, the relay routing id, the fingerprint people compare.
//! - Constraint: the secret is 32 random bytes that `vessel-store` keeps in
//!   `node.key`; this crate must not need the store, and the store must not
//!   need cryptography (plan, "Crates").
//! - Alternatives for deriving the public key: `snow`'s resolver
//!   (`resolve_dh` returns an `Option`, so `from_secret` would be fallible
//!   for a reason that cannot happen), or `curve25519-dalek` directly.
//! - Choice: `curve25519-dalek`'s `MontgomeryPoint::mul_base_clamped`, the
//!   function `snow` itself calls, from the crate it already pulls in.
//!   `from_secret` cannot fail, and no crate is added.
//! - The secret is held in [`Zeroizing`], wiped when the key pair is
//!   dropped. `snow` copies it into its own handshake state for the length
//!   of a handshake without wiping it; that copy is short-lived and is the
//!   library's to fix.
//! - Fingerprint: the routing id itself, 16 bytes in eight groups of four
//!   hex digits. One derived value instead of two, so a key's fingerprint
//!   also matches the id in relay logs, and 128 bits leave nothing to
//!   search for a look-alike key (the plan's sketch had 64).

use std::fmt;
use std::fmt::Write as _;

use curve25519_dalek::montgomery::MontgomeryPoint;
use sha2::{Digest as _, Sha256};
use vessel_proto::{NodePublicKey, RoutingId};
use zeroize::Zeroizing;

/// A node's static X25519 key pair: its identity on the wire.
///
/// `Debug` shows only the public key.
pub struct NodeKeypair {
    secret: Zeroizing<[u8; 32]>,
    public: NodePublicKey,
}

impl NodeKeypair {
    /// The key pair whose secret is `secret` (the 32 bytes of `node.key`).
    /// Any 32 bytes are a valid X25519 secret.
    #[must_use]
    pub fn from_secret(secret: [u8; 32]) -> Self {
        let secret = Zeroizing::new(secret);
        let public = NodePublicKey::from_bytes(MontgomeryPoint::mul_base_clamped(*secret).0);
        Self { secret, public }
    }

    /// A new key pair from the operating system's random source. Nodes keep
    /// theirs in `node.key` (`vessel-store`); this is for tests and tools.
    ///
    /// # Errors
    /// The operating system has no randomness to give.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut secret = Zeroizing::new([0_u8; 32]);
        getrandom::fill(secret.as_mut_slice())?;
        Ok(Self::from_secret(*secret))
    }

    /// The public half, which clients pin.
    #[must_use]
    pub fn public(&self) -> &NodePublicKey {
        &self.public
    }

    /// The secret half, for the handshake only.
    pub(crate) fn secret(&self) -> &[u8; 32] {
        &self.secret
    }
}

impl fmt::Debug for NodeKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeKeypair")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// The id a relay knows the node by: the first 16 bytes of
/// `SHA-256(public key)` (ADR 0009). The relay checks, with the possession
/// proof, that whoever registers it holds the key.
#[must_use]
pub fn routing_id(key: &NodePublicKey) -> RoutingId {
    let digest = Sha256::digest(key.as_bytes());
    let mut id = [0_u8; 16];
    for (byte, digest_byte) in id.iter_mut().zip(digest.iter()) {
        *byte = *digest_byte;
    }
    RoutingId::from_bytes(id)
}

/// The key's fingerprint for people to compare, `3fa9 c2d1 … e4b5`: the
/// routing id in eight groups of four lowercase hex digits.
#[must_use]
pub fn fingerprint(key: &NodePublicKey) -> String {
    let id = routing_id(key);
    let mut text = String::with_capacity(39);
    for (index, pair) in id.as_bytes().chunks(2).enumerate() {
        if index > 0 {
            text.push(' ');
        }
        for byte in pair {
            let _infallible = write!(text, "{byte:02x}");
        }
    }
    text
}
