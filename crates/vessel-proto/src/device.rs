//! Paired devices: scopes, credentials, the in-channel preamble, and
//! pairing links (ADR 0010, protocol 4).
//!
//! A device is one client machine with its own credential for a node. It is
//! created by an expiring, single-use invitation and can be revoked on its
//! own, unlike the node token, which is shared by everything local.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::DeviceId;
use crate::key::secret;
use crate::node::NodeInfo;
use crate::pairing::PairingLink;

/// What a caller may do. Every RPC method declares the one it needs
/// (`Method::scope`); the node checks it for calls from a paired device.
/// The node token and the local socket hold every scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Look: node info, harnesses, sessions, logs, subscriptions.
    Read,
    /// Act on sessions: create, send input, kill.
    Operate,
    /// Change the node: stop it, manage devices, reload profiles.
    Admin,
    /// Fetch the fleet's bundle and report on applying it (protocol 5, ADR
    /// 0012): what a member node holds on its fleet's home. Independent of
    /// the others: a member needs no access to the home's sessions.
    Sync,
    /// Pass calls on to the fleet's other nodes (`fleet.relay`, protocol
    /// 6, ADR 0013): what a home grants a member whose agents and devices
    /// may act on other nodes. Independent of `Sync`, and granted only when
    /// the home's owner asks (`vessel fleet invite --relay`).
    Relay,
}

impl Scope {
    /// Every scope.
    pub const ALL: [Self; 5] = [
        Self::Read,
        Self::Operate,
        Self::Admin,
        Self::Sync,
        Self::Relay,
    ];

    /// What an invitation grants unless it names scopes: everything a person
    /// at another machine needs, but not `Sync` or `Relay`, which only fleet
    /// members use.
    pub const DEVICE_DEFAULT: [Self; 3] = [Self::Read, Self::Operate, Self::Admin];

    /// The scope's wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Operate => "operate",
            Self::Admin => "admin",
            Self::Sync => "sync",
            Self::Relay => "relay",
        }
    }

    /// The first protocol version that has this scope; a device holding a
    /// newer scope is shown to older clients without it.
    #[must_use]
    pub const fn since(self) -> crate::node::ProtocolVersion {
        match self {
            Self::Read | Self::Operate | Self::Admin => crate::node::ProtocolVersion::new(4),
            Self::Sync => crate::node::ProtocolVersion::new(5),
            Self::Relay => crate::node::ProtocolVersion::new(6),
        }
    }
}

secret! {
    /// A device's credential: 256 random bits, sent only inside a secure
    /// channel, stored on the node as a hash only.
    DeviceSecret, 32
}

secret! {
    /// The single-use secret of a pairing invitation: 128 random bits.
    InvitationSecret, 16
}

/// A paired device as the node reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Its id.
    pub id: DeviceId,
    /// The name given at pairing (the client machine's name by default).
    pub label: String,
    /// When it was paired.
    pub created_at: DateTime<Utc>,
    /// When it last authenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// What it may do.
    pub scopes: Vec<Scope>,
    /// When it was revoked, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The first frame a client sends inside a secure channel, before `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Preamble {
    /// A paired device presents its credential.
    DeviceAuth {
        /// The device.
        device: DeviceId,
        /// Its credential.
        secret: DeviceSecret,
    },
    /// A new device redeems a pairing invitation.
    PairRequest {
        /// The invitation from the pairing link.
        invite: InvitationSecret,
        /// What to call this device on the node.
        device_name: String,
    },
}

/// Why a node refused a preamble. Deliberately coarse: a refusal must not
/// tell an attacker which part was wrong beyond what a legitimate user needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// No such device, or the credential does not match.
    UnknownDevice,
    /// The device was revoked.
    Revoked,
    /// The invitation is unknown, used, or expired.
    InvalidInvitation,
    /// The node cannot authenticate right now (for example it is stopping).
    Unavailable,
}

/// The node's answer to a [`Preamble`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PreambleReply {
    /// The device is authenticated; `hello` follows.
    Authenticated {
        /// The device.
        device: DeviceId,
    },
    /// The invitation was redeemed; the client stores the credential, and
    /// `hello` follows on the same channel.
    Paired {
        /// The new device's id.
        device: DeviceId,
        /// Its credential.
        secret: DeviceSecret,
        /// The node, so the client can pin its id. Boxed: the other replies
        /// are a few bytes, and this one is sent once per pairing.
        node: Box<NodeInfo>,
    },
    /// Refused; the node closes the channel after sending this.
    Refused {
        /// Why.
        reason: RefusalReason,
    },
}

/// `device.invite` (protocol 4): make a pairing link. Needs `Admin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteDevice {
    /// Label for the device that redeems it (it may name itself instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// How long the invitation is valid, in seconds (the node caps it).
    pub ttl_secs: u32,
    /// What the device may do (protocol 5); `None` is
    /// [`Scope::DEVICE_DEFAULT`], what every protocol 4 device got. A fleet
    /// member is invited with `[sync]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<Scope>>,
}

/// Reply to `device.invite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    /// The link to hand to the new device.
    pub link: PairingLink,
    /// When it expires.
    pub expires: DateTime<Utc>,
}

/// `device.list` (protocol 4): every device, revoked ones included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDevices;

/// `device.revoke` (protocol 4): revoke a device and close its connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeDevice {
    /// Which device.
    pub device: DeviceId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_frames_are_tagged() {
        let auth = Preamble::DeviceAuth {
            device: DeviceId::random(),
            secret: DeviceSecret::from_bytes([2; 32]),
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(json.starts_with(r#"{"type":"device_auth""#));
        assert_eq!(serde_json::from_str::<Preamble>(&json).unwrap(), auth);
        let refused = PreambleReply::Refused {
            reason: RefusalReason::Revoked,
        };
        assert_eq!(
            serde_json::to_string(&refused).unwrap(),
            r#"{"type":"refused","reason":"revoked"}"#
        );
    }
}
