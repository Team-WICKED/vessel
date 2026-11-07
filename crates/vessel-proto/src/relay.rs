//! The relay's own protocol (ADR 0009): the node's control connection and
//! the close codes a relay uses. Everything a client and node say to each
//! other passes through the relay encrypted and is not in this module.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::key::{
    KeyTextError, NodePublicKey, RoutingId, base64url, hex_array, secret, serde_via_text, write_hex,
};

/// The registration token a relay operator gives to nodes allowed to
/// register there. Clients need none. Never printed.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct RelayToken(String);

/// An empty relay token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyToken;

impl fmt::Display for EmptyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the relay token is empty")
    }
}

impl std::error::Error for EmptyToken {}

impl RelayToken {
    /// Wrap a token read from a file (surrounding whitespace removed).
    ///
    /// # Errors
    /// Fails if nothing is left.
    pub fn new(text: &str) -> Result<Self, EmptyToken> {
        let text = text.trim();
        if text.is_empty() {
            return Err(EmptyToken);
        }
        Ok(Self(text.to_owned()))
    }

    /// The token text, for constant-time comparison by the relay.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RelayToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelayToken(<redacted>)")
    }
}

impl Serialize for RelayToken {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RelayToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Self::new(&text).map_err(serde::de::Error::custom)
    }
}

secret!(
    /// A ticket's HMAC-SHA256 under the relay's token (`vessel_relay`
    /// computes and checks it). Not printed: it is no use without the
    /// ticket's node key, but nothing needs to see it.
    TicketMac,
    32
);

/// A registration ticket (protocol 6, `fleet.callback`): lets the one node
/// whose key it names register with a relay until it expires, without the
/// relay's token. A fleet's home mints one for a member from the token it
/// registers with, and the relay checks it with the same token, so the
/// relay keeps no state for it and rotating the token voids every ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RelayTicket {
    /// The node it registers: the relay takes it only with this key, and
    /// only once the node proved it holds the key.
    pub key: NodePublicKey,
    /// When it stops registering the node, in seconds since the Unix epoch;
    /// a relay also ends a registration made with it then.
    pub expires_at: u64,
    /// The MAC over the key and `expires_at`.
    pub mac: TicketMac,
}

/// One waiting client's connection on a relay: 128 random bits minted by the
/// relay, lowercase hex. The node answers with `GET /v1/accept?conn=<id>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct ConnId([u8; 16]);

impl ConnId {
    /// Wrap raw bytes (the relay draws them from the OS RNG).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConnId({self})")
    }
}

impl FromStr for ConnId {
    type Err = KeyTextError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex_array(s.trim()).map(Self)
    }
}

/// One Noise handshake message, base64url on the wire.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct HandshakeMessage(Vec<u8>);

impl HandshakeMessage {
    /// Largest Noise message (the spec's limit).
    pub const MAX_LEN: usize = 65_535;

    /// Wrap message bytes.
    ///
    /// # Errors
    /// Fails if longer than a Noise message can be.
    pub fn new(bytes: Vec<u8>) -> Result<Self, KeyTextError> {
        if bytes.len() > Self::MAX_LEN {
            return Err(KeyTextError::Length {
                expected: Self::MAX_LEN,
                found: bytes.len(),
            });
        }
        Ok(Self(bytes))
    }

    /// The message bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for HandshakeMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HandshakeMessage({} bytes)", self.0.len())
    }
}

impl fmt::Display for HandshakeMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&base64url(&self.0))
    }
}

impl FromStr for HandshakeMessage {
    type Err = KeyTextError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use base64::Engine as _;
        // Bound the text before decoding: 4 characters per 3 bytes, rounded
        // up, so nothing larger than a Noise message is ever allocated.
        let limit = Self::MAX_LEN.div_ceil(3).saturating_mul(4);
        if s.len() > limit {
            return Err(KeyTextError::Length {
                expected: Self::MAX_LEN,
                found: s.len(),
            });
        }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim())
            .map_err(|_decode| KeyTextError::Encoding)?;
        Self::new(bytes)
    }
}

serde_via_text!(ConnId, HandshakeMessage);

/// A message from a node on its control connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeToRelay {
    /// Ask to be reachable under `routing_id`.
    Register {
        /// The routing id derived from `key`.
        routing_id: RoutingId,
        /// The node's static key; the relay checks possession with a Noise
        /// handshake to it.
        key: NodePublicKey,
        /// The relay's registration token (an operator's node), or
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<RelayToken>,
        /// a ticket for `key` (a fleet member, protocol 6): exactly one of
        /// the two.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<RelayTicket>,
    },
    /// The node's response to `Prove`.
    Proof {
        /// The second Noise NK message.
        message: HandshakeMessage,
    },
}

/// Why a relay refused a registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RegistrationRefusal {
    /// The registration token is wrong, or none and no ticket was given.
    Token,
    /// The ticket is not for this key, has expired, reaches too far ahead,
    /// or was not made with the relay's token.
    Ticket,
    /// The routing id does not belong to the key, or the node could not
    /// prove it holds the key.
    Proof,
    /// The message was malformed or out of order.
    Protocol,
    /// The relay holds as many nodes as it is configured to; another may
    /// register once one leaves (try again later). Relays before this
    /// variant never send it; a node that does not know it reads the
    /// message as malformed and retries.
    Full,
}

/// A message from a relay on a node's control connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayToNode {
    /// Prove you hold the key: the first Noise NK message to it.
    Prove {
        /// The handshake message.
        message: HandshakeMessage,
    },
    /// Registered; clients can now reach the node.
    Registered,
    /// Refused; the relay closes after sending this.
    Rejected {
        /// Why.
        reason: RegistrationRefusal,
    },
    /// A client is waiting: open `/v1/accept?conn=<conn>`.
    Incoming {
        /// The waiting connection.
        conn: ConnId,
    },
}

/// Close codes a relay sends a client (websocket close frames). Refusals by
/// the node itself travel inside the encrypted channel instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayClose {
    /// No node with that routing id is registered (transient: it may be
    /// restarting).
    NoSuchNode,
    /// The node did not accept the connection in time.
    NotAccepted,
    /// A message exceeded the relay's limit.
    TooLarge,
    /// Rate limited.
    RateLimited,
    /// The relay carries as many connections as it is configured to, to
    /// this node or in all (transient: a slot frees when one ends). A
    /// client that does not know the code sees a plain close.
    Full,
}

impl RelayClose {
    /// Every variant, for exhaustive tests.
    pub const ALL: [Self; 5] = [
        Self::NoSuchNode,
        Self::NotAccepted,
        Self::TooLarge,
        Self::RateLimited,
        Self::Full,
    ];

    /// The websocket close code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::NoSuchNode => 4404,
            Self::NotAccepted => 4408,
            Self::TooLarge => 4413,
            Self::RateLimited => 4429,
            Self::Full => 4503,
        }
    }

    /// The variant for a close code, if it is one of the relay's.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            4404 => Some(Self::NoSuchNode),
            4408 => Some(Self::NotAccepted),
            4413 => Some(Self::TooLarge),
            4429 => Some(Self::RateLimited),
            4503 => Some(Self::Full),
            _ => None,
        }
    }

    /// Whether a client should try again later (every relay close is; a
    /// wrong key or a refused device is not, but those are not relay codes).
    #[must_use]
    pub const fn is_transient(self) -> bool {
        match self {
            Self::NoSuchNode | Self::NotAccepted | Self::RateLimited | Self::Full => true,
            Self::TooLarge => false,
        }
    }
}

/// Close codes a node sends on a secure-channel websocket (`/secure`, or a
/// relay data connection), before any frame could carry an error.
///
/// [Decision Log]
/// - Purpose: with Noise NK only the node can tell that a client pinned the
///   wrong key; the client just sees the connection end, which looks
///   transient, so it would retry a stale key forever.
/// - Choice: the node closes with a distinct code after a failed handshake,
///   and relays forward a node's close code unchanged. Clients treat it as
///   final ("re-pair this node").
/// - Cost: the code is not authenticated. A forged one can only make a
///   client stop retrying, a denial of service any relay or network path can
///   cause anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeClose {
    /// The secure handshake failed: most likely the client pinned another
    /// key (the node was re-keyed or it is a different node).
    HandshakeFailed,
}

impl NodeClose {
    /// Every variant, for exhaustive tests.
    pub const ALL: [Self; 1] = [Self::HandshakeFailed];

    /// The websocket close code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::HandshakeFailed => 4401,
        }
    }

    /// The variant for a close code, if it is one of the node's.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            4401 => Some(Self::HandshakeFailed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_round_trip_in_the_private_range() {
        for close in RelayClose::ALL {
            assert_eq!(RelayClose::from_code(close.code()), Some(close));
            assert!((4000..5000).contains(&close.code()));
        }
        assert_eq!(RelayClose::from_code(1000), None);
        for close in NodeClose::ALL {
            assert_eq!(NodeClose::from_code(close.code()), Some(close));
            assert_eq!(RelayClose::from_code(close.code()), None);
        }
    }

    #[test]
    fn control_messages_are_tagged_and_hide_the_token() {
        let register = NodeToRelay::Register {
            routing_id: RoutingId::from_bytes([1; 16]),
            key: NodePublicKey::from_bytes([2; 32]),
            token: Some(RelayToken::new(" secret ").unwrap()),
            ticket: None,
        };
        let json = serde_json::to_string(&register).unwrap();
        assert!(json.starts_with(r#"{"type":"register""#));
        assert_eq!(
            serde_json::from_str::<NodeToRelay>(&json).unwrap(),
            register
        );
        assert!(!format!("{register:?}").contains("secret"));
        let incoming = RelayToNode::Incoming {
            conn: ConnId::from_bytes([0xcd; 16]),
        };
        assert_eq!(
            serde_json::to_string(&incoming).unwrap(),
            format!(r#"{{"type":"incoming","conn":"{}"}}"#, "cd".repeat(16))
        );
        assert!(RelayToken::new("  ").is_err());
    }

    #[test]
    fn a_registration_names_a_ticket_instead_and_hides_its_mac() {
        let ticket = RelayTicket {
            key: NodePublicKey::from_bytes([2; 32]),
            expires_at: 1_800_000_000,
            mac: TicketMac::from_bytes([9; 32]),
        };
        let register = NodeToRelay::Register {
            routing_id: RoutingId::from_bytes([1; 16]),
            key: NodePublicKey::from_bytes([2; 32]),
            token: None,
            ticket: Some(ticket.clone()),
        };
        let json = serde_json::to_string(&register).unwrap();
        assert!(!json.contains("token"), "{json}");
        assert!(json.contains(r#""expires_at":1800000000"#), "{json}");
        assert_eq!(
            serde_json::from_str::<NodeToRelay>(&json).unwrap(),
            register
        );
        assert!(!format!("{ticket:?}").contains(&TicketMac::from_bytes([9; 32]).to_string()));
    }

    #[test]
    fn handshake_messages_are_bounded_before_decoding() {
        let message = HandshakeMessage::new(vec![5; 48]).unwrap();
        let text = message.to_string();
        assert_eq!(text.parse::<HandshakeMessage>().unwrap(), message);
        assert!(HandshakeMessage::new(vec![0; 65_536]).is_err());
        assert!("A".repeat(100_000).parse::<HandshakeMessage>().is_err());
    }
}
