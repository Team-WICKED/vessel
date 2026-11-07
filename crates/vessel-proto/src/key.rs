//! Node keys and relay routing ids, as they appear on the wire.
//!
//! [Decision Log]
//! - Purpose: a node's X25519 public key travels in `NodeInfo`, pairing
//!   links and relay registration, and a relay finds nodes by an id derived
//!   from it.
//! - Constraint: wire types exist once, in this crate (ADR 0001), while the
//!   cryptography lives in `vessel-secure`.
//! - Alternatives: the key type in `vessel-secure` (then `NodeInfo` could not
//!   carry it without `vessel-proto` depending on crypto); keys as plain
//!   strings (untyped).
//! - Choice: this crate owns the data (32 key bytes, 16 routing-id bytes) and
//!   their text forms. `vessel-secure` owns everything computed from them
//!   (deriving the routing id with SHA-256, fingerprints, the handshake).
//! - Text forms: keys are `x25519:` plus unpadded base64url, so a stored key
//!   names its algorithm; routing ids are lowercase hex (URL-safe, no new
//!   dependency for base32).

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Why a key or id did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTextError {
    /// The `x25519:` prefix is missing.
    MissingPrefix,
    /// Not valid base64url or hex.
    Encoding,
    /// Decoded to the wrong number of bytes.
    Length {
        /// Bytes expected.
        expected: usize,
        /// Bytes found.
        found: usize,
    },
}

impl fmt::Display for KeyTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => f.write_str("expected `x25519:` followed by base64url"),
            Self::Encoding => f.write_str("invalid encoding"),
            Self::Length { expected, found } => {
                write!(f, "expected {expected} bytes, found {found}")
            }
        }
    }
}

impl std::error::Error for KeyTextError {}

/// Decode unpadded base64url into exactly `N` bytes.
pub(crate) fn base64url_array<const N: usize>(text: &str) -> Result<[u8; N], KeyTextError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_decode| KeyTextError::Encoding)?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_length| KeyTextError::Length {
        expected: N,
        found: bytes.len(),
    })
}

/// Encode bytes as unpadded base64url.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode lowercase or uppercase hex into exactly `N` bytes.
pub(crate) fn hex_array<const N: usize>(text: &str) -> Result<[u8; N], KeyTextError> {
    let text = text.as_bytes();
    let mut bytes = [0_u8; N];
    if text.len() != N.saturating_mul(2) {
        return Err(KeyTextError::Length {
            expected: N,
            found: text.len().div_euclid(2),
        });
    }
    for (byte, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_utf8| KeyTextError::Encoding)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_hex| KeyTextError::Encoding)?;
    }
    Ok(bytes)
}

/// Write bytes as lowercase hex.
pub(crate) fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    bytes.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
}

const KEY_PREFIX: &str = "x25519:";

/// A node's static X25519 public key: the identity a client pins at pairing
/// and checks on every secure handshake.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct NodePublicKey([u8; 32]);

impl NodePublicKey {
    /// Wrap raw key bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for NodePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{KEY_PREFIX}{}", base64url(&self.0))
    }
}

impl fmt::Debug for NodePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodePublicKey({self})")
    }
}

impl FromStr for NodePublicKey {
    type Err = KeyTextError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let encoded = s
            .trim()
            .strip_prefix(KEY_PREFIX)
            .ok_or(KeyTextError::MissingPrefix)?;
        base64url_array(encoded).map(Self)
    }
}

/// Where a relay files a node: derived from the node's public key by
/// `vessel-secure` (truncated SHA-256), so it reveals nothing the key would
/// not, and anyone holding the key can compute it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct RoutingId([u8; 16]);

impl RoutingId {
    /// Wrap raw id bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for RoutingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for RoutingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RoutingId({self})")
    }
}

impl FromStr for RoutingId {
    type Err = KeyTextError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex_array(s.trim()).map(Self)
    }
}

/// Serde through the text form, for types that have `Display` + `FromStr`.
macro_rules! serde_via_text {
    ($($name:ty),* $(,)?) => {$(
        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <::std::borrow::Cow<'de, str> as ::serde::Deserialize>::deserialize(deserializer)?;
                text.parse().map_err(::serde::de::Error::custom)
            }
        }
    )*};
}

pub(crate) use serde_via_text;

/// Declares a fixed-size secret: base64url on the wire, never printed.
macro_rules! secret {
    ($(#[$meta:meta])* $name:ident, $len:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
        pub struct $name([u8; $len]);

        impl $name {
            /// Wrap secret bytes (generated by the node from the OS RNG).
            #[must_use]
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// The secret bytes. Callers compare them in constant time or
            /// hash them; they never log them.
            #[must_use]
            pub const fn expose(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }

        impl ::std::fmt::Display for $name {
            /// The wire text. Only for writing credential files and links;
            /// `Debug` is what logs see.
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&$crate::key::base64url(&self.0))
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::key::KeyTextError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $crate::key::base64url_array(s.trim()).map(Self)
            }
        }

        $crate::key::serde_via_text!($name);
    };
}

pub(crate) use secret;

serde_via_text!(NodePublicKey, RoutingId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_their_prefixed_text() {
        let key = NodePublicKey::from_bytes([7; 32]);
        let text = key.to_string();
        assert!(text.starts_with("x25519:"));
        assert_eq!(text.parse::<NodePublicKey>().unwrap(), key);
        assert_eq!(
            serde_json::from_str::<NodePublicKey>(&serde_json::to_string(&key).unwrap()).unwrap(),
            key
        );
        assert_eq!(
            "AAAA".parse::<NodePublicKey>(),
            Err(KeyTextError::MissingPrefix)
        );
        assert!(matches!(
            "x25519:AAAA".parse::<NodePublicKey>(),
            Err(KeyTextError::Length { expected: 32, .. })
        ));
    }

    #[test]
    fn routing_ids_are_lowercase_hex() {
        let id = RoutingId::from_bytes([0xab; 16]);
        assert_eq!(id.to_string(), "ab".repeat(16));
        assert_eq!(id.to_string().parse::<RoutingId>().unwrap(), id);
        assert!("zz".repeat(16).parse::<RoutingId>().is_err());
        assert!("ab".parse::<RoutingId>().is_err());
    }
}
