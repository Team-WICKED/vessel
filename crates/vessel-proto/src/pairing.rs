//! Pairing links and the routes they carry (ADR 0010, protocol 4).

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::device::InvitationSecret;
use crate::id::NodeId;
use crate::key::{NodePublicKey, base64url, serde_via_text};

/// A route to a node: a `ws://` or `wss://` URL, for a relay or a direct
/// address. Only the scheme is checked here; reachability is the client's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteUrl(String);

/// A route URL with a scheme other than `ws://` or `wss://`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRoute(pub String);

impl fmt::Display for InvalidRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is not a ws:// or wss:// URL", self.0)
    }
}

impl std::error::Error for InvalidRoute {}

impl RouteUrl {
    /// The URL text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether it needs TLS.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.0.starts_with("wss://")
    }
}

impl fmt::Display for RouteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RouteUrl {
    type Err = InvalidRoute;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        let rest = text
            .strip_prefix("wss://")
            .or_else(|| text.strip_prefix("ws://"));
        match rest {
            Some(host) if !host.is_empty() => Ok(Self(text.to_owned())),
            Some(_) | None => Err(InvalidRoute(text.to_owned())),
        }
    }
}

serde_via_text!(RouteUrl);

/// The pairing-link format version. Only version 1 exists; a link from a
/// newer format is refused with a clear error rather than half-understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct OfferVersion;

impl OfferVersion {
    /// The number on the wire.
    pub const NUMBER: u8 = 1;
}

impl Serialize for OfferVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(Self::NUMBER)
    }
}

impl<'de> Deserialize<'de> for OfferVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = u8::deserialize(deserializer)?;
        if number == Self::NUMBER {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "pairing link format {number} is newer than this vessel understands (1); update vessel"
            )))
        }
    }
}

/// Everything a client needs to pair with a node, carried by a pairing link.
///
/// Unknown fields are refused (ADR 0010): an offer is security-relevant, so a
/// field this version does not understand must not be silently ignored;
/// format changes bump `v` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingOffer {
    /// Format version (always 1).
    pub v: OfferVersion,
    /// The node's id (pinned like an SSH node's).
    pub node: NodeId,
    /// The node's static key (pinned; the handshake checks it).
    pub key: NodePublicKey,
    /// Relays the node is registered with, in preference order.
    #[serde(default)]
    pub relays: Vec<RouteUrl>,
    /// Direct addresses the node was told to advertise (`--public`).
    #[serde(default)]
    pub direct: Vec<RouteUrl>,
    /// The single-use invitation.
    pub invite: InvitationSecret,
    /// When the invitation expires.
    pub expires: DateTime<Utc>,
}

const LINK_PREFIX: &str = "vessel://pair#";

/// A pairing link: `vessel://pair#` followed by the base64url JSON of a
/// [`PairingOffer`]. The offer sits after `#` so that, if a link is ever
/// opened through a web page, the fragment never reaches a server.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingLink(String);

/// Why a pairing link did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// It does not start with `vessel://pair#`.
    Prefix,
    /// The fragment is not base64url.
    Encoding,
    /// The decoded offer is not valid.
    Offer(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prefix => f.write_str("a pairing link starts with `vessel://pair#`"),
            Self::Encoding => f.write_str("the pairing link is damaged (not base64url)"),
            Self::Offer(reason) => write!(f, "the pairing link is damaged: {reason}"),
        }
    }
}

impl std::error::Error for LinkError {}

impl PairingLink {
    /// Encode an offer.
    ///
    /// # Errors
    /// Fails only if the offer cannot be serialised, which its types rule out
    /// in practice.
    pub fn encode(offer: &PairingOffer) -> Result<Self, serde_json::Error> {
        let json = serde_json::to_vec(offer)?;
        Ok(Self(format!("{LINK_PREFIX}{}", base64url(&json))))
    }

    /// Decode the offer.
    ///
    /// # Errors
    /// Fails if the text is not a pairing link or its offer is damaged.
    pub fn decode(&self) -> Result<PairingOffer, LinkError> {
        use base64::Engine as _;
        let encoded = self.0.strip_prefix(LINK_PREFIX).ok_or(LinkError::Prefix)?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_decode| LinkError::Encoding)?;
        serde_json::from_slice(&json).map_err(|error| LinkError::Offer(error.to_string()))
    }

    /// The link text, for printing (it contains a secret: print it only to
    /// the person pairing, never to logs).
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PairingLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingLink(<redacted>)")
    }
}

impl FromStr for PairingLink {
    type Err = LinkError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let link = Self(s.trim().to_owned());
        link.decode().map(|_offer| link)
    }
}

impl Serialize for PairingLink {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PairingLink {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceSecret;

    fn offer() -> PairingOffer {
        PairingOffer {
            v: OfferVersion,
            node: NodeId::random(),
            key: NodePublicKey::from_bytes([9; 32]),
            relays: vec!["wss://relay.example/vessel".parse().unwrap()],
            direct: vec!["ws://203.0.113.5:7077".parse().unwrap()],
            invite: InvitationSecret::from_bytes([3; 16]),
            expires: DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn pairing_links_round_trip_and_reject_damage() {
        let offer = offer();
        let link = PairingLink::encode(&offer).unwrap();
        assert!(link.expose().starts_with("vessel://pair#"));
        assert_eq!(link.decode().unwrap(), offer);
        assert_eq!(link.expose().parse::<PairingLink>().unwrap(), link);
        assert!(matches!(
            "https://x#abc".parse::<PairingLink>(),
            Err(LinkError::Prefix)
        ));
        assert!(matches!(
            "vessel://pair#!!".parse::<PairingLink>(),
            Err(LinkError::Encoding)
        ));
    }

    #[test]
    fn secrets_never_print_through_debug() {
        let secret = DeviceSecret::from_bytes([1; 32]);
        assert_eq!(format!("{secret:?}"), "DeviceSecret(<redacted>)");
        let link = PairingLink::encode(&offer()).unwrap();
        assert_eq!(format!("{link:?}"), "PairingLink(<redacted>)");
        assert_eq!(secret.to_string().parse::<DeviceSecret>().unwrap(), secret);
    }

    #[test]
    fn offers_refuse_other_versions_and_unknown_fields() {
        let json = serde_json::to_string(&offer()).unwrap();
        assert!(json.starts_with(r#"{"v":1,"#));
        let newer = json.replacen(r#""v":1"#, r#""v":2"#, 1);
        assert!(serde_json::from_str::<PairingOffer>(&newer).is_err());
        let extra = json.replacen('{', r#"{"scopes":["admin"],"#, 1);
        assert!(serde_json::from_str::<PairingOffer>(&extra).is_err());
    }

    #[test]
    fn routes_must_be_websocket_urls() {
        assert!("wss://relay.example".parse::<RouteUrl>().unwrap().is_tls());
        assert!(!"ws://10.0.0.1:7077".parse::<RouteUrl>().unwrap().is_tls());
        assert!("https://relay.example".parse::<RouteUrl>().is_err());
        assert!("ws://".parse::<RouteUrl>().is_err());
    }
}
