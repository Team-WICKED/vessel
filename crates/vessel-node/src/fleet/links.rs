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
    pub(crate) async fn reconcile(&self, membership: Option<&Membership>) {
        let granted = match self.granted().await {
            Ok(Some(granted)) => granted,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "cannot read what this node granted its home");
                return;
            }
        };
        if membership.is_some_and(|membership| membership.home == granted.home) {
            return;
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
    /// the credential.
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
        let dialer = WsDialer::for_routes(offer.direct.iter().chain(&offer.relays))
            .map_err(|error| internal(&error))?;
        let (paired, _route) = pair_over(&dialer, &offer, &self.name, RouteTimeouts::default())
            .await
            .map_err(|error| unavailable(format!("cannot reach the member to pair: {error}")))?;
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
        blocking(move || {
            files::create_private_dir(&dir).map_err(|error| FileError::io(&dir, error))?;
            credential
                .write(&credential_path)
                .map_err(|error| FileError::io(&credential_path, std::io::Error::other(error)))?;
            files::write_json(&json, &grant)
        })
        .await?;
        drop(self.members.lock().await.remove(&offer.node));
        tracing::info!(member = %offer.node, "a member granted this home a credential");
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
        let grant = self
            .grants()
            .await
            .into_iter()
            .find(|grant| grant.node == node)
            .ok_or_else(|| {
                WireError::new(
                    ErrorCode::NotFound,
                    format!(
                        "node {node} is not a member of this fleet that granted the home access \
                         (`vessel fleet nodes`)"
                    ),
                )
            })?;
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
