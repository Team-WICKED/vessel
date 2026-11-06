//! Paired devices (ADR 0010): invitations, pairing, authentication,
//! listing and revocation.
//!
//! - [`Devices`] is the authority: `device.*` calls and the secure
//!   channel's preamble both go through it.
//! - `invitations`: pending pairing invitations, in memory.
//! - `connected`: each device's open connections, so revoking closes them.
//! - `preamble`: the first frames on a secure channel, which decide who the
//!   peer is.
//!
//! What is stored is in `vessel-store` (`devices` table): every device with
//! the hash of its credential, revoked ones kept as tombstones.

mod connected;
mod invitations;
pub(crate) mod preamble;

use std::sync::{Arc, Mutex, PoisonError};

use chrono::{TimeDelta, Utc};
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    DeviceId, DeviceInfo, DeviceSecret, Invitation, InvitationSecret, InviteDevice, NodeId,
    NodePublicKey, PairingLink, PairingOffer, RefusalReason, RouteUrl, Scope,
};
use vessel_store::{CredentialHash, Store};

use crate::error::ServiceError;
use connected::Connected;
pub(crate) use connected::Registration;
use invitations::{Full, Invitations};

/// Longest an invitation may be valid (ADR 0010).
pub(crate) const MAX_INVITATION_TTL: TimeDelta = TimeDelta::hours(24);
/// Longest device label kept, in characters.
const MAX_LABEL: usize = 64;

/// Where a pairing link says the node can be reached.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Routes {
    /// Relays the node registers with (`--relay`), in preference order.
    pub(crate) relays: Vec<RouteUrl>,
    /// Addresses it was told to advertise (`--public`).
    pub(crate) direct: Vec<RouteUrl>,
}

/// The device authority of one node.
#[derive(Debug)]
pub(crate) struct Devices {
    store: Store,
    node: NodeId,
    /// The node's public key, which pairing links carry.
    key: NodePublicKey,
    routes: Routes,
    invitations: Mutex<Invitations>,
    connected: Arc<Connected>,
}

impl Devices {
    pub(crate) fn new(store: Store, node: NodeId, key: NodePublicKey, routes: Routes) -> Self {
        Self {
            store,
            node,
            key,
            routes,
            invitations: Mutex::new(Invitations::default()),
            connected: Arc::new(Connected::default()),
        }
    }

    /// `device.invite`: a single-use pairing link valid for `ttl_secs`
    /// (capped at a day).
    pub(crate) fn invite(&self, request: &InviteDevice) -> Result<Invitation, ServiceError> {
        let key = self.key;
        if request.ttl_secs == 0 {
            return Err(ServiceError::InvalidTtl);
        }
        let ttl = TimeDelta::seconds(i64::from(request.ttl_secs)).min(MAX_INVITATION_TTL);
        let now = Utc::now();
        let expires = now
            .checked_add_signed(ttl)
            .ok_or(ServiceError::InvalidTtl)?;
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(ServiceError::Random)?;
        let invite = InvitationSecret::from_bytes(bytes);
        let offer = PairingOffer {
            node: self.node,
            key,
            relays: self.routes.relays.clone(),
            direct: self.routes.direct.clone(),
            invite: invite.clone(),
            expires,
        };
        let link = PairingLink::encode(&offer).map_err(ServiceError::Link)?;
        self.invitations()
            .add(&invite, request.label.as_deref().map(label), expires, now)
            .map_err(|Full| ServiceError::TooManyInvitations)?;
        tracing::info!(%expires, "made a pairing invitation");
        Ok(Invitation { link, expires })
    }

    /// `device.list`: every device, revoked ones included.
    pub(crate) async fn list(&self) -> Result<Vec<DeviceInfo>, ServiceError> {
        Ok(self.store.devices().await?)
    }

    /// `device.revoke`: revoke the device and close its open connections.
    /// Revoking a revoked device succeeds and changes nothing.
    pub(crate) async fn revoke(&self, device: DeviceId) -> Result<(), ServiceError> {
        self.store.revoke_device(device, Utc::now()).await?;
        let closed = self.connected.close(device);
        tracing::info!(%device, closed, "revoked a device");
        Ok(())
    }

    /// Check a device's credential; its scopes if it may connect.
    ///
    /// A wrong secret and an unknown device give the same answer, and the
    /// credential is hashed either way, so neither the reason nor the time
    /// taken tells which ids exist. A revoked device is told so only when its
    /// secret matches.
    pub(crate) async fn authenticate(
        &self,
        device: DeviceId,
        secret: &DeviceSecret,
    ) -> Result<Vec<Scope>, RefusalReason> {
        let stored = self.store.device(device).await.map_err(|error| {
            tracing::warn!(%error, "cannot read the devices table");
            RefusalReason::Unavailable
        })?;
        let Some(stored) = stored else {
            let _same_work = CredentialHash::of(secret);
            return Err(RefusalReason::UnknownDevice);
        };
        if !stored.credential.matches(secret) {
            return Err(RefusalReason::UnknownDevice);
        }
        if stored.info.revoked_at.is_some() {
            return Err(RefusalReason::Revoked);
        }
        if let Err(error) = self.store.device_seen(device, Utc::now()).await {
            tracing::debug!(%device, %error, "could not record when the device was seen");
        }
        Ok(stored.info.scopes)
    }

    /// Redeem an invitation: a new device named by the invitation's label,
    /// or else by the name the device gave itself. Returns its id, its new
    /// credential and its scopes (M3 grants every scope, ADR 0010).
    pub(crate) async fn pair(
        &self,
        invite: &InvitationSecret,
        device_name: &str,
    ) -> Result<(DeviceId, DeviceSecret, Vec<Scope>), RefusalReason> {
        let now = Utc::now();
        let redeemed = self
            .invitations()
            .redeem(invite, now)
            .ok_or(RefusalReason::InvalidInvitation)?;
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| {
            tracing::warn!(%error, "no randomness for a device credential");
            RefusalReason::Unavailable
        })?;
        let secret = DeviceSecret::from_bytes(bytes);
        let info = DeviceInfo {
            id: DeviceId::random(),
            label: redeemed.label.unwrap_or_else(|| label(device_name)),
            created_at: now,
            last_seen: Some(now),
            scopes: Scope::ALL.to_vec(),
            revoked_at: None,
        };
        let id = info.id;
        let scopes = info.scopes.clone();
        // The invitation is spent even if storing fails: the user makes a
        // new one, and nothing half-paired is left behind.
        self.store
            .add_device(info, CredentialHash::of(&secret))
            .await
            .map_err(|error| {
                tracing::warn!(%error, "could not store a new device");
                RefusalReason::Unavailable
            })?;
        tracing::info!(device = %id, "paired a device");
        Ok((id, secret, scopes))
    }

    /// Register a connection of `device` (see `connected`).
    pub(crate) fn connect(&self, device: DeviceId, parent: &CancellationToken) -> Registration {
        self.connected.register(device, parent)
    }

    fn invitations(&self) -> std::sync::MutexGuard<'_, Invitations> {
        // Held only for a map operation, never across an `.await`; a poisoned
        // map is still a valid map.
        self.invitations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A label as stored: trimmed, without control characters, at most
/// [`MAX_LABEL`] characters, and never empty.
fn label(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_LABEL)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "device".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests;
