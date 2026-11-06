//! The fleet's links for calls across nodes (protocol 6, ADR 0013 "across
//! nodes through the home"): a member's connection to its home, the home's
//! credentials on its members and its connections to them, and the mutual
//! grant that makes those credentials.
//!
//! [Decision Log]
//! - Purpose: `fleet.forward`, `fleet.relay` and `fleet.deliver` need a
//!   connection for each hop: member to home (the pull loop has one) and
//!   home to member (nothing had one: in F2 the home held no credential
//!   on its members).
//! - The mutual grant (ADR 0012): once connected, a member's node makes an
//!   invitation on itself with the scopes its owner chose at join
//!   (`vessel fleet join --grant`, default `read`, `operate`) and hands the
//!   link to its home (`fleet.grant`). The home redeems it like any device
//!   (`pair_over`) and keeps the credential in `fleet/grants/` (owner-only),
//!   named after the member's device on the home, so it also knows which
//!   node that device speaks for. A membership from before this version
//!   grants nothing until joined again.
//! - Which device is the home's: the member's invitation is tracked
//!   (`Devices::invite_tracked`), so the member learns the id of the
//!   device the home's redemption created and records it in
//!   `fleet/granted.json`; it accepts `fleet.deliver` from that device only.
//!   Alternatives: matching the device by its label (a label is not
//!   authenticated); having `fleet.grant` answer the id (the contract
//!   answers `ack`, and the home's word is not needed for what the member
//!   saw itself).
//! - Leaving: the node revokes the home's device and forgets the record
//!   once the membership is gone or names another home (`reconcile`), so
//!   a former home loses its access even if `vessel fleet leave` ran while
//!   the node was stopped.
//! - Who a member device speaks for (the review of 2026-11-06): a node id
//!   is a name the node chose (`node.id`), not something its key proves,
//!   and pairing checks only that the node answering a link claims the
//!   link's id; the key is what the secure channel authenticates. So the
//!   home binds each member device to the `(id, key)` of its first grant
//!   and refuses a grant that would make one node's id or key another's:
//!   a device rebinding to another pair, an id or key another device's
//!   grant holds, the home's own id, a name another member (or the home)
//!   uses. Without this, any `sync` device could grant a link from a stock
//!   node whose `node.id` names another member, and the home's calls to
//!   that member (prompts, input, secrets copied) went to it. The same
//!   pair through another device is the same node joined again (only it
//!   holds the key), and its newer grant replaces the older one. Deriving
//!   the id from the key would remove the question; that is a protocol
//!   change, proposed in the plan.
//! - A grant's link names the routes the home dials: at most
//!   [`MAX_GRANT_ROUTES`], and a failure to pair answers the device in
//!   general terms (the details go to the home's log), so a `sync` device
//!   cannot use the home to probe addresses.
//! - Connections: the member's to its home is the pull loop's, shared here
//!   while it lasts. The home connects to a member on first use and keeps
//!   the connection while it stays open; a call that loses it fails as
//!   `unavailable` (it may or may not have run, so it is not retried), and
//!   the next one connects again.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::FutureExt as _;
use serde::{Deserialize, Serialize};
use vessel_client::secure::{RouteTimeouts, SecureConnector, WsDialer, pair_over};
use vessel_client::{
    CallError, Connector as _, DeviceCredential, NodeClient, Pinned, SecureTarget,
};
use vessel_proto::{
    ClientInfo, DeviceId, ErrorCode, FleetNode, GrantHome, InviteDevice, NodeId, NodePublicKey,
    PROTOCOL_VERSION, PairingLink, ProtocolVersion, RouteUrl, Scope, WireError,
};
use vessel_rpc::Unary;

use super::files::{self, FileError, FleetFiles};
use super::member::Membership;
use crate::devices::Devices;

/// How long a pairing link for the home is valid: the home redeems it at
/// once, so this only bounds a link nobody used.
const GRANT_TTL_SECS: u32 = 600;

/// How long the member waits to hear that its invitation was redeemed
/// after the home said it was.
const REDEEMED_WITHIN: Duration = Duration::from_secs(30);

/// The most routes (relays and direct addresses together) a grant's link
/// may name: a node has a few; more is a device using the home to dial.
const MAX_GRANT_ROUTES: usize = 8;

/// What the device that sent a grant hears when the home could not pair
/// with the node its link names.
const CANNOT_PAIR: &str = "the home could not pair with the node the link names; the home's \
                           log says why";

/// What a member granted its home (`fleet/granted.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Granted {
    /// The home.
    home: NodeId,
    /// The device its redemption created here.
    device: DeviceId,
    /// What that device may do.
    scopes: Vec<Scope>,
    /// When.
    since: DateTime<Utc>,
}

/// A member's grant as its home keeps it (`fleet/grants/<device>.json`, the
/// credential beside it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberGrant {
    /// The member's device on the home.
    pub(crate) member: DeviceId,
    /// The member's node.
    pub(crate) node: NodeId,
    /// Its name.
    pub(crate) name: String,
    /// Its key.
    key: NodePublicKey,
    /// Its relays.
    #[serde(default)]
    relays: Vec<RouteUrl>,
    /// Its direct addresses.
    #[serde(default)]
    direct: Vec<RouteUrl>,
    /// The newest protocol it speaks.
    pub(crate) protocol: ProtocolVersion,
    /// When it granted.
    granted_at: DateTime<Utc>,
}

/// This node's links to the rest of its fleet.
#[derive(Debug)]
pub(crate) struct Links {
    /// This node.
    node: NodeId,
    /// Its name, which the home's device on a member is named after.
    name: String,
    files: FleetFiles,
    devices: Arc<Devices>,
    /// A member's connection to its home, while the pull loop has one.
    home: Mutex<Option<NodeClient>>,
    /// The home's open connections to its members.
    members: tokio::sync::Mutex<HashMap<NodeId, NodeClient>>,
    /// Held while a grant is made or accepted (and while `reconcile`
    /// revokes): two grants cannot both pass the checks against the
    /// grants held, and a revocation never races a grant being recorded.
    granting: tokio::sync::Mutex<()>,
}

/// A grant's claim: the member device that sent it, and the node its link
/// names (the name once the home has paired with it).
#[derive(Debug, Clone, Copy)]
struct Claim<'a> {
    member: DeviceId,
    node: NodeId,
    key: &'a NodePublicKey,
    name: Option<&'a str>,
}

fn conflict(message: String) -> WireError {
    WireError::new(ErrorCode::Conflict, message)
}

/// Whether `claim` may be granted, given the grants of active member
/// devices `held` and the home's own id and name; the member devices whose
/// grants it replaces (the same node, joined again through another
/// device). See the module's Decision Log.
fn check_claim(
    claim: Claim<'_>,
    home: NodeId,
    home_name: &str,
    held: &[MemberGrant],
) -> Result<Vec<DeviceId>, WireError> {
    if claim.node == home {
        return Err(conflict(format!(
            "the link names node {home}, which is this home itself"
        )));
    }
    if claim.name == Some(home_name) {
        return Err(conflict(format!(
            "the node is named `{home_name}`, as this home is; rename one of them"
        )));
    }
    let mut replaced = Vec::new();
    for grant in held {
        let same_node = grant.node == claim.node;
        let same_key = grant.key == *claim.key;
        if grant.member == claim.member {
            if !same_node || !same_key {
                return Err(conflict(format!(
                    "this device speaks for node {} on this home; a device's node never \
                     changes (join again from the other node)",
                    grant.node
                )));
            }
            continue;
        }
        if same_node && same_key {
            replaced.push(grant.member);
            continue;
        }
        if same_node {
            return Err(conflict(format!(
                "node {} has granted this home access already, with another key",
                claim.node
            )));
        }
        if same_key {
            return Err(conflict(format!(
                "the link's key belongs to node {} on this home",
                grant.node
            )));
        }
        if claim.name == Some(grant.name.as_str()) {
            return Err(conflict(format!(
                "another member, node {}, is named `{}`; rename one of them",
                grant.node, grant.name
            )));
        }
    }
    Ok(replaced)
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "vesseld-fleet".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

fn unavailable(message: String) -> WireError {
    WireError::new(ErrorCode::Unavailable, message)
}

fn internal(error: &impl std::fmt::Display) -> WireError {
    WireError::new(ErrorCode::Internal, error.to_string())
}

/// A hop's failure as the caller sees it: the target's own error as it
/// is, the hop's in words that say which node.
fn hop_error(node: NodeId, error: CallError) -> WireError {
    match error {
        CallError::Remote(error) => error,
        CallError::NotInProtocol {
            method, negotiated, ..
        } => WireError::new(
            ErrorCode::Unsupported,
            format!(
                "node {node} speaks protocol {negotiated}; `{}` needs {PROTOCOL_VERSION}",
                method.as_str()
            ),
        ),
        CallError::Disconnected => unavailable(format!(
            "lost the connection to node {node}; the call may or may not have run there"
        )),
        CallError::UnexpectedReply(_) | CallError::RequestIdsExhausted => internal(&error),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, FileError> + Send + 'static,
) -> Result<T, WireError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| internal(&error))?
        .map_err(|error| internal(&error))
}

impl Links {
    pub(crate) fn new(
        node: NodeId,
        name: String,
        files: FleetFiles,
        devices: Arc<Devices>,
    ) -> Self {
        Self {
            node,
            name,
            files,
            devices,
            home: Mutex::new(None),
            members: tokio::sync::Mutex::new(HashMap::new()),
            granting: tokio::sync::Mutex::new(()),
        }
    }

    /// This node's id.
    pub(crate) const fn node(&self) -> NodeId {
        self.node
    }

    /// Whether this node is its fleet's home (it has `fleet.toml`).
    pub(crate) fn is_home(&self) -> bool {
        self.files.sources().exists()
    }

    /// Whether this node is a member (it has `fleet/member.json`).
    pub(crate) fn is_member(&self) -> bool {
        self.files.membership().exists()
    }

    /// The home this node is a member of, if any.
    pub(crate) async fn home(&self) -> Option<NodeId> {
        let files = self.files.clone();
        blocking(move || super::member::membership(&files))
            .await
            .ok()
            .flatten()
            .map(|membership| membership.home)
    }

    // The member's side.

    /// The pull loop is connected to the home through `client`.
    pub(crate) fn connected_home(&self, client: NodeClient) {
        *self.home.lock().unwrap_or_else(PoisonError::into_inner) = Some(client);
    }

    /// The pull loop lost its connection to the home.
    pub(crate) fn lost_home(&self) {
        *self.home.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Call the home over the pull loop's connection.
    pub(crate) async fn call_home<R: Unary>(&self, request: R) -> Result<R::Response, WireError> {
        let client = self
            .home
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                unavailable(
                    "this node is not connected to its fleet's home right now \
                     (`vessel fleet status`)"
                        .to_owned(),
                )
            })?;
        let home = client.node().id;
        client
            .call(request)
            .await
            .map_err(|error| hop_error(home, error))
    }

    /// Give the home of `membership` a credential on this node, once: the
    /// scopes the membership names (none, for a membership from before
    /// grants). Called by the pull loop right after it connects.
    pub(crate) async fn grant(&self, client: &NodeClient, membership: &Membership) {
        if membership.grant.is_empty() {
            return;
        }
        match self.granted().await {
            Ok(Some(granted)) if granted.home == membership.home => {
                if self.device_is_valid(granted.device).await {
                    return;
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "cannot read what this node granted its home");
                return;
            }
        }
        if let Err(error) = self.make_grant(client, membership).await {
            tracing::warn!(home = %membership.home, %error, "could not grant the fleet's home a credential here");
        }
    }

    async fn make_grant(
        &self,
        client: &NodeClient,
        membership: &Membership,
    ) -> Result<(), WireError> {
        let (invitation, redeemed) = self
            .devices
            .invite_tracked(&InviteDevice {
                label: Some(format!("fleet home {}", membership.home)),
                ttl_secs: GRANT_TTL_SECS,
                scopes: Some(membership.grant.clone()),
            })
            .map_err(|error| error.to_wire())?;
        let _ack = client
            .call(GrantHome {
                link: invitation.link,
            })
            .await
            .map_err(|error| hop_error(membership.home, error))?;
        let device = tokio::time::timeout(REDEEMED_WITHIN, redeemed)
            .await
            .map_err(|_elapsed| {
                unavailable("the home said it redeemed the link, but it was not".to_owned())
            })?
            .map_err(|_dropped| unavailable("the link was dropped unredeemed".to_owned()))?;
        let granted = Granted {
            home: membership.home,
            device,
            scopes: membership.grant.clone(),
            since: Utc::now(),
        };
        let path = self.granted_path();
        blocking(move || files::write_json(&path, &granted)).await?;
        tracing::info!(home = %membership.home, %device, "granted the fleet's home a credential here");
        Ok(())
    }

    fn granted_path(&self) -> PathBuf {
        self.files.dir().join("granted.json")
    }

    async fn granted(&self) -> Result<Option<Granted>, WireError> {
        let path = self.granted_path();
        blocking(move || files::read_json(&path)).await
    }

    /// The home's device on this member, if this node granted its current
    /// home one: the only device `fleet.deliver` is accepted from.
    pub(crate) async fn home_device(&self, home: NodeId) -> Option<DeviceId> {
        match self.granted().await {
            Ok(Some(granted)) if granted.home == home => Some(granted.device),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "cannot read what this node granted its home");
                None
            }
        }
    }

    async fn device_is_valid(&self, device: DeviceId) -> bool {
        self.devices.list().await.is_ok_and(|devices| {
            devices
                .iter()
                .any(|info| info.id == device && info.revoked_at.is_none())
        })
    }

    /// A grant to a home this node no longer follows is revoked and
    /// forgotten. Called by the fleet loop on every look.
    ///
    /// [Decision Log]
    /// - Revoking is the one thing here that cannot be undone, so it needs
    ///   to know the membership is gone (or names another home), not merely
    ///   to have failed to read it: a read error keeps the grant.
    /// - The membership is read here, after the grant, not taken from the
    ///   loop's earlier look: a grant is only ever written while a
    ///   membership exists, so reading in this order cannot pair a new
    ///   grant with a membership read from before the join (which revoked
    ///   the grant a join had just made).
    pub(crate) async fn reconcile(&self) {
        let granted = match self.granted().await {
            Ok(Some(granted)) => granted,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "cannot read what this node granted its home");
                return;
            }
        };
        let files = self.files.clone();
        match blocking(move || super::member::membership(&files)).await {
            Ok(Some(membership)) if membership.home == granted.home => return,
            Ok(Some(_) | None) => {}
            Err(error) => {
                tracing::warn!(%error, "cannot read this node's fleet membership; its home's grant stays");
                return;
            }
        }
        if let Err(error) = self.devices.revoke(granted.device).await {
            tracing::warn!(%error, "cannot revoke the former home's device");
            return;
        }
        let path = self.granted_path();
        match blocking(move || files::remove(&path)).await {
            Ok(()) => {
                tracing::info!(home = %granted.home, "revoked the former fleet home's access here");
            }
            Err(error) => tracing::warn!(%error, "cannot forget the former home's grant"),
        }
    }

    // The home's side.

    fn grant_paths(&self, member: DeviceId) -> (PathBuf, PathBuf) {
        let dir = self.files.dir().join("grants");
        (
            dir.join(format!("{member}.json")),
            dir.join(format!("{member}.credential")),
        )
    }

    /// `fleet.grant`: redeem the link member device `member` sent, and keep
    /// the credential, if the node it names may be that device's (see the
    /// module's Decision Log).
    pub(crate) async fn accept_grant(
        &self,
        member: DeviceId,
        link: &PairingLink,
    ) -> Result<(), WireError> {
        if !self.is_home() {
            return Err(WireError::new(
                ErrorCode::Unavailable,
                "this node is not a fleet's home".to_owned(),
            ));
        }
        let offer = link.decode().map_err(|error| {
            WireError::new(ErrorCode::InvalidRequest, format!("the link: {error}"))
        })?;
        let routes = offer.direct.len().saturating_add(offer.relays.len());
        if routes == 0 || routes > MAX_GRANT_ROUTES {
            return Err(WireError::new(
                ErrorCode::InvalidRequest,
                format!("a grant's link names 1 to {MAX_GRANT_ROUTES} routes, not {routes}"),
            ));
        }
        let _granting = self.granting.lock().await;
        let mut claim = Claim {
            member,
            node: offer.node,
            key: &offer.key,
            name: None,
        };
        let _replaced = check_claim(claim, self.node, &self.name, &self.grants().await)?;
        let dialer = WsDialer::for_routes(offer.direct.iter().chain(&offer.relays)).map_err(
            |error| {
                tracing::warn!(%member, %error, "a member's grant link names routes this home cannot dial");
                unavailable(CANNOT_PAIR.to_owned())
            },
        )?;
        let (paired, _route) = pair_over(&dialer, &offer, &self.name, RouteTimeouts::default())
            .await
            .map_err(|error| {
                tracing::warn!(%member, node = %offer.node, %error, "cannot pair with the node a member's grant names");
                unavailable(CANNOT_PAIR.to_owned())
            })?;
        claim.name = Some(&paired.node.name);
        let replaced = check_claim(claim, self.node, &self.name, &self.grants().await)?;
        let grant = MemberGrant {
            member,
            node: offer.node,
            name: paired.node.name.clone(),
            key: offer.key,
            relays: offer.relays.clone(),
            direct: offer.direct.clone(),
            protocol: paired.node.protocol.max,
            granted_at: Utc::now(),
        };
        let (json, credential_path) = self.grant_paths(member);
        let credential = paired.credential;
        let dir = self.files.dir().join("grants");
        let stale: Vec<(PathBuf, PathBuf)> = replaced
            .iter()
            .map(|device| self.grant_paths(*device))
            .collect();
        blocking(move || {
            files::create_private_dir(&dir).map_err(|error| FileError::io(&dir, error))?;
            credential
                .write(&credential_path)
                .map_err(|error| FileError::io(&credential_path, std::io::Error::other(error)))?;
            files::write_json(&json, &grant)?;
            for (json, credential) in &stale {
                files::remove(json)?;
                files::remove(credential)?;
            }
            Ok(())
        })
        .await?;
        drop(self.members.lock().await.remove(&offer.node));
        tracing::info!(member = %offer.node, device = %member, replaced = ?replaced, "a member granted this home a credential");
        Ok(())
    }

    /// The grant of member device `member`, if it made one.
    pub(crate) async fn grant_of(&self, member: DeviceId) -> Option<MemberGrant> {
        let (json, _credential) = self.grant_paths(member);
        blocking(move || files::read_json(&json))
            .await
            .ok()
            .flatten()
    }

    /// Every grant of a member device that may still sync (not revoked).
    async fn grants(&self) -> Vec<MemberGrant> {
        let Ok(devices) = self.devices.list().await else {
            return Vec::new();
        };
        let mut grants = Vec::new();
        for device in devices {
            if device.revoked_at.is_some() || !device.scopes.contains(&Scope::Sync) {
                continue;
            }
            if let Some(grant) = self.grant_of(device.id).await {
                grants.push(grant);
            }
        }
        grants
    }

    /// Call member `node` with the home's credential there.
    pub(crate) async fn call_member<R: Unary>(
        &self,
        node: NodeId,
        request: R,
    ) -> Result<R::Response, WireError> {
        let client = self.member_client(node).await?;
        let called = client.call(request).await;
        if matches!(called, Err(CallError::Disconnected)) {
            drop(self.members.lock().await.remove(&node));
        }
        called.map_err(|error| hop_error(node, error))
    }

    async fn member_client(&self, node: NodeId) -> Result<NodeClient, WireError> {
        let mut members = self.members.lock().await;
        if let Some(client) = members.get(&node)
            && client.closed().now_or_never().is_none()
        {
            return Ok(client.clone());
        }
        // By the member device bound to `node`: exactly one, as grants are
        // checked when made. Two can only come from grants kept before
        // that check; neither is trusted.
        let mut bound = self
            .grants()
            .await
            .into_iter()
            .filter(|grant| grant.node == node);
        let grant = match (bound.next(), bound.next()) {
            (Some(grant), None) => grant,
            (None, _) => {
                return Err(WireError::new(
                    ErrorCode::NotFound,
                    format!(
                        "node {node} is not a member of this fleet that granted the home \
                         access (`vessel fleet nodes`)"
                    ),
                ));
            }
            (Some(first), Some(second)) => {
                return Err(conflict(format!(
                    "member devices {} and {} both claim node {node}; revoke the wrong one \
                     (`vessel fleet members`)",
                    first.member, second.member
                )));
            }
        };
        let (_json, credential_path) = self.grant_paths(grant.member);
        let credential = blocking(move || {
            DeviceCredential::read(&credential_path)
                .map_err(|error| FileError::io(&credential_path, std::io::Error::other(error)))
        })
        .await?;
        let target = SecureTarget {
            key: grant.key,
            relays: grant.relays.clone(),
            direct: grant.direct.clone(),
        };
        let dialer = WsDialer::for_routes(target.routes()).map_err(|error| internal(&error))?;
        let connector = SecureConnector::new(dialer, target, node, credential)
            .map_err(|error| internal(&error))?;
        let client = Pinned::new(connector, Some(node))
            .connect(client_info())
            .await
            .map_err(|error| unavailable(format!("cannot reach node {node}: {error}")))?;
        let _previous = members.insert(node, client.clone());
        Ok(client)
    }

    /// `fleet.peers` on the home: the home itself, and every member that
    /// granted it access, reachable when its device is connected here or
    /// the home holds an open connection to it.
    pub(crate) async fn peers(&self, home_name: &str) -> Vec<FleetNode> {
        let mut nodes = vec![FleetNode {
            node: self.node,
            name: home_name.to_owned(),
            protocol: PROTOCOL_VERSION,
            reachable: true,
        }];
        let open: Vec<NodeId> = self
            .members
            .lock()
            .await
            .iter()
            .filter(|(_node, client)| client.closed().now_or_never().is_none())
            .map(|(node, _client)| *node)
            .collect();
        for grant in self.grants().await {
            nodes.push(FleetNode {
                node: grant.node,
                name: grant.name,
                protocol: grant.protocol.min(PROTOCOL_VERSION),
                reachable: open.contains(&grant.node) || self.devices.is_connected(grant.member),
            });
        }
        nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> NodePublicKey {
        NodePublicKey::from_bytes([byte; 32])
    }

    fn held(member: DeviceId, node: NodeId, key_byte: u8, name: &str) -> MemberGrant {
        MemberGrant {
            member,
            node,
            name: name.to_owned(),
            key: key(key_byte),
            relays: Vec::new(),
            direct: Vec::new(),
            protocol: PROTOCOL_VERSION,
            granted_at: Utc::now(),
        }
    }

    fn refused(result: Result<Vec<DeviceId>, WireError>) -> String {
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict, "{error:?}");
        error.message
    }

    /// The checks of a grant against the grants held (the review of
    /// 2026-11-06: a `sync` device claimed another member's node id).
    #[test]
    fn a_grant_may_not_make_one_nodes_id_or_key_another_s() {
        let home = NodeId::random();
        let (desktop, laptop, newcomer) =
            (DeviceId::random(), DeviceId::random(), DeviceId::random());
        let (desktop_node, laptop_node) = (NodeId::random(), NodeId::random());
        let grants = [
            held(desktop, desktop_node, 1, "desktop"),
            held(laptop, laptop_node, 2, "laptop"),
        ];
        let keys = [key(0), key(1), key(2), key(3)];
        let claim = |member, node, key_byte: usize, name| Claim {
            member,
            node,
            key: &keys[key_byte],
            name,
        };

        // A new node: nothing replaced.
        let new_node = NodeId::random();
        assert_eq!(
            check_claim(
                claim(newcomer, new_node, 3, Some("tablet")),
                home,
                "home",
                &grants
            )
            .unwrap(),
            Vec::<DeviceId>::new()
        );
        // Another member's id with another key: the reported attack.
        let message = refused(check_claim(
            claim(newcomer, desktop_node, 3, None),
            home,
            "home",
            &grants,
        ));
        assert!(message.contains("another key"), "{message}");
        // Another member's key under another id.
        let message = refused(check_claim(
            claim(newcomer, new_node, 1, None),
            home,
            "home",
            &grants,
        ));
        assert!(message.contains("belongs to node"), "{message}");
        // The home's own id, and its name.
        let _own = refused(check_claim(
            claim(newcomer, home, 3, None),
            home,
            "home",
            &grants,
        ));
        let _named = refused(check_claim(
            claim(newcomer, new_node, 3, Some("home")),
            home,
            "home",
            &grants,
        ));
        // A device that granted once keeps its node and its key.
        for (node, key_byte) in [(new_node, 1_usize), (desktop_node, 3), (laptop_node, 2)] {
            let message = refused(check_claim(
                claim(desktop, node, key_byte, None),
                home,
                "home",
                &grants,
            ));
            assert!(message.contains("never changes"), "{message}");
        }
        assert!(
            check_claim(
                claim(desktop, desktop_node, 1, Some("desktop")),
                home,
                "home",
                &grants
            )
            .unwrap()
            .is_empty()
        );
        // Another member's name, once the home knows the node's.
        let message = refused(check_claim(
            claim(newcomer, new_node, 3, Some("laptop")),
            home,
            "home",
            &grants,
        ));
        assert!(message.contains("named `laptop`"), "{message}");
        // The same node joined again through a new device: it replaces the
        // older grant.
        assert_eq!(
            check_claim(
                claim(newcomer, desktop_node, 1, Some("desktop")),
                home,
                "home",
                &grants
            )
            .unwrap(),
            vec![desktop]
        );
    }
}
