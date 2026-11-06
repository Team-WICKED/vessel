//! Calls across a fleet (protocol 6, ADR 0013 "across nodes through the
//! home"): the envelope a call travels in from its caller's node, through
//! the home, to the node that runs it, and what members learn of the fleet.
//!
//! [Decision Log]
//! - Purpose: an agent (or a person's CLI) reaches only its own node, yet
//!   may need to act on a session on another node of its fleet, with no
//!   more authority there than it has at home.
//! - Constraint: only the home holds credentials on every node; a member
//!   holds one on the home only. A target must be able to tell who a call
//!   speaks for, and the guard is generated from the method table.
//! - Alternatives: a `node` field on every request (every type changes, and
//!   the guard would rediscover the intersection per method); relaying
//!   subscriptions (a stream per waiting agent on each hop, with lag and
//!   resume on both); a mesh of direct links (ADR 0013 option 1, later).
//! - Choice: three rows, each with a real scope for its own hop:
//!   `fleet.forward` (caller to its node), `fleet.relay` (member to home,
//!   scope `relay`, which the home's owner grants per member) and
//!   `fleet.deliver` (home to target, the last hop: never passed on, so at
//!   most two hops and no loops). Each carries an [`Origin`] its first node
//!   vouches for; the target runs the inner call with its delivering
//!   device's grant intersected with `origin.scopes`, and applies ownership
//!   and limits to the origin.
//! - Trade-off: a call to another member takes two hops until a direct
//!   link replaces the home's; waiting across nodes is a long poll
//!   (`session.read` with `wait`), not a stream.

use serde::{Deserialize, Serialize};

use crate::device::Scope;
use crate::id::{DeviceId, NodeId, SessionId};
use crate::node::ProtocolVersion;
use crate::pairing::PairingLink;
use crate::rpc::{Call, Method, Reply};

/// Who a forwarded call speaks for, as the node it started on vouches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The node the call started on.
    pub node: NodeId,
    /// The principal there.
    pub by: OriginBy,
    /// What that principal holds on its own node. The target never grants
    /// more than these, whatever its own grant to the delivering device.
    pub scopes: Vec<Scope>,
}

/// The principal a call started with, on its own node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginBy {
    /// That node's token or local socket.
    Owner,
    /// A device paired with that node.
    Device {
        /// Its id on that node.
        id: DeviceId,
    },
    /// An agent, through its session's credential on that node.
    Agent {
        /// The agent's session.
        session: SessionId,
        /// How many agents deep that session is (the owner's own sessions
        /// are 0), so the target can refuse a session past the depth limit.
        depth: u8,
    },
}

/// `fleet.forward` (protocol 6): run `call` on fleet node `node`. Sent by a
/// client to its own node, which checks `call` against the caller as if it
/// were local, then passes it on with the caller's [`Origin`]. Needs `read`
/// here; the inner call's own scope is checked on every hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    /// The node that runs the call; this node itself runs it locally.
    pub node: NodeId,
    /// The call, which must be [forwardable](Method::forwardable).
    pub call: Box<Call>,
}

/// `fleet.relay` (protocol 6): a member asks its home to pass a call on.
/// The home refuses an `origin.node` other than the member's own (a member
/// speaks only for its own principals). Needs `relay`, which a member gets
/// only when the home's owner asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayCall {
    /// The node that runs the call; the home runs it itself when it is
    /// the target.
    pub node: NodeId,
    /// The call.
    pub call: Box<Call>,
    /// Who it speaks for.
    pub origin: Origin,
}

/// `fleet.deliver` (protocol 6): the home asks a member to run a call for
/// `origin`. The last hop: a member never passes a delivered call on, and
/// accepts one only from its home's device (the one `fleet.grant` made).
/// Needs `read`; the inner call runs with the delivering device's grant
/// intersected with `origin.scopes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliver {
    /// The call.
    pub call: Box<Call>,
    /// Who it speaks for.
    pub origin: Origin,
}

/// Reply to `fleet.forward`, `fleet.relay` and `fleet.deliver`: the inner
/// call's reply. An inner call that fails fails the envelope with its error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forwarded {
    /// What the target answered.
    pub reply: Box<Reply>,
}

/// `fleet.grant` (protocol 6): a member gives its home a credential on
/// itself, as a single-use pairing link made with its own `device.invite`
/// and the scopes its owner chose. The home redeems it like any device and
/// keeps the credential; revoking it on the member cuts the home off.
/// Needs `sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantHome {
    /// The link, made on the member.
    pub link: PairingLink,
}

/// `fleet.peers` (protocol 6): the home's view of its fleet, for its
/// members. Needs `sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPeers;

/// `fleet.nodes` (protocol 6): what this node knows of its fleet (a member
/// asks its home with `fleet.peers`). Needs `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListNodes;

/// A node of a fleet, the home included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNode {
    /// Its id.
    pub node: NodeId,
    /// Its name.
    pub name: String,
    /// The protocol its home speaks with it (a protocol 5 member gets the
    /// bundle but no delivered calls).
    pub protocol: ProtocolVersion,
    /// Whether the home can reach it now.
    pub reachable: bool,
}

impl Method {
    /// Whether a call of this method can travel in `fleet.forward`,
    /// `fleet.relay` and `fleet.deliver`. Deny by default: the `match` names
    /// every method, so a new row does not compile until someone decides.
    /// A forwarded call still runs with the target's grant to its home
    /// intersected with the origin's scopes.
    ///
    /// - Forwardable, the work agents and people do on another node: node
    ///   and harness info, sessions (list, read, create, send, kill, move
    ///   between profiles), profiles to choose from, and the transfers F4
    ///   pulls through the home (export, import, read, close), with
    ///   `secret.set`, which `secret.copy` sends to its target.
    /// - Not forwardable, administration of a node done there: stopping it,
    ///   its devices, re-reading its profiles, listing, deleting or copying
    ///   its secrets.
    /// - Not forwardable, the fleet's own plumbing: an envelope is never
    ///   wrapped in another (at most two hops, no loops), and the bundle,
    ///   grant and membership calls belong to the hop between a member and
    ///   its home.
    /// - Not forwardable, streams: a subscription's events would need a
    ///   stream across hops (`session.read` with `wait` serves instead).
    #[must_use]
    pub const fn forwardable(self) -> bool {
        match self {
            Self::NodeInfo
            | Self::ListHarnesses
            | Self::ListSessions
            | Self::GetSession
            | Self::CreateSession
            | Self::SendInput
            | Self::KillSession
            | Self::ReadEvents
            | Self::MoveProfile
            | Self::ListProfiles
            | Self::ExportSession
            | Self::ImportSession
            | Self::ExportMemory
            | Self::ImportMemory
            | Self::ReadTransfer
            | Self::CloseTransfer
            | Self::SetSecret => true,
            Self::StopNode
            | Self::InviteDevice
            | Self::ListDevices
            | Self::RevokeDevice
            | Self::ReloadProfiles
            | Self::ListSecrets
            | Self::DeleteSecret
            | Self::CopySecret
            | Self::Forward
            | Self::Relay
            | Self::Deliver
            | Self::GrantHome
            | Self::ListPeers
            | Self::ListNodes
            | Self::GetManifest
            | Self::ReadBlob
            | Self::WatchFleet
            | Self::ReportApplied
            | Self::ListMembers
            | Self::Subscribe
            | Self::Unsubscribe => false,
        }
    }
}
