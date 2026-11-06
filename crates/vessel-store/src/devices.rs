//! Paired devices (ADR 0010): their rows, and the hash that stands in for
//! each device's credential.
//!
//! [Decision Log]
//! - Purpose: a device authenticates with a 256-bit secret issued at
//!   pairing. The node must check it, and a copy of the database must not
//!   let anyone authenticate.
//! - Choice: store SHA-256 of the secret, compared in constant time. The
//!   secret is 256 random bits, so a slow password hash (Argon2, bcrypt)
//!   would add nothing: there is no small space of guesses to slow down.
//! - Revoked devices stay as rows with `revoked_at` set (a tombstone, plan
//!   P1), so a revoked device that still holds its secret hears `Revoked`
//!   rather than `UnknownDevice`. Someone without the secret learns nothing
//!   either way: the node reports `UnknownDevice` unless the secret matches.
//! - Scopes are stored as their JSON names (`["read","operate","admin"]`), so
//!   a scope added later needs no migration.

use std::fmt;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension as _, Row, params};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use vessel_proto::{AgentSecret, DeviceId, DeviceInfo, DeviceSecret, Scope};

use crate::codec::{encode, time_from_sql, time_to_sql};
use crate::error::StoreError;

/// SHA-256 of a credential: what the node keeps instead of the secret (a
/// device's, stored; an agent's, in memory only, ADR 0013). There is no
/// `PartialEq`; the `matches` methods compare in constant time.
#[derive(Clone)]
pub struct CredentialHash([u8; 32]);

impl CredentialHash {
    /// The hash of `secret`.
    #[must_use]
    pub fn of(secret: &DeviceSecret) -> Self {
        Self::of_bytes(secret.expose())
    }

    /// Whether `secret` is the credential this hash was made from. Takes the
    /// same time wherever the first difference is.
    #[must_use]
    pub fn matches(&self, secret: &DeviceSecret) -> bool {
        self.matches_bytes(secret.expose())
    }

    /// The hash of an agent's credential.
    #[must_use]
    pub fn of_agent(secret: &AgentSecret) -> Self {
        Self::of_bytes(secret.expose())
    }

    /// Whether `secret` is the agent credential this hash was made from,
    /// in constant time.
    #[must_use]
    pub fn matches_agent(&self, secret: &AgentSecret) -> bool {
        self.matches_bytes(secret.expose())
    }

    fn of_bytes(secret: &[u8]) -> Self {
        Self(Sha256::digest(secret).into())
    }

    fn matches_bytes(&self, secret: &[u8]) -> bool {
        Self::of_bytes(secret).0.ct_eq(&self.0).into()
    }
}

impl fmt::Debug for CredentialHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialHash(..)")
    }
}

/// A device as stored: what `device.list` shows, and its credential hash.
#[derive(Debug, Clone)]
pub struct StoredDevice {
    /// The device.
    pub info: DeviceInfo,
    /// Its credential's hash.
    pub credential: CredentialHash,
}

/// The columns [`device_from_row`] expects, in order.
pub(crate) const DEVICE_COLUMNS: &str =
    "id, label, secret_hash, scopes, created_at, last_seen, revoked_at";

pub(crate) fn device_from_row(row: &Row<'_>) -> Result<StoredDevice, StoreError> {
    let text: String = row.get(0)?;
    let corrupt = |what| StoreError::CorruptDevice {
        id: text.clone(),
        what,
    };
    let id: DeviceId = text.parse().map_err(|_invalid| corrupt("id"))?;
    let hash: Vec<u8> = row.get(2)?;
    let credential = <[u8; 32]>::try_from(hash.as_slice())
        .map(CredentialHash)
        .map_err(|_length| corrupt("credential hash"))?;
    let scopes: Vec<Scope> =
        serde_json::from_str(&row.get::<_, String>(3)?).map_err(|_invalid| corrupt("scopes"))?;
    Ok(StoredDevice {
        info: DeviceInfo {
            id,
            label: row.get(1)?,
            created_at: time_from_sql(row.get(4)?),
            last_seen: row.get::<_, Option<i64>>(5)?.map(time_from_sql),
            scopes,
            revoked_at: row.get::<_, Option<i64>>(6)?.map(time_from_sql),
        },
        credential,
    })
}

/// Insert a new device. Writer side.
pub(crate) fn insert(
    connection: &Connection,
    device: &DeviceInfo,
    credential: &CredentialHash,
) -> Result<(), StoreError> {
    let exists = connection
        .prepare_cached("SELECT 1 FROM devices WHERE id = ?1")?
        .query_row([device.id.to_string()], |_row| Ok(()))
        .optional()?;
    if exists.is_some() {
        return Err(StoreError::DeviceExists(device.id));
    }
    let _rows = connection
        .prepare_cached(
            "INSERT INTO devices (id, label, secret_hash, scopes, created_at, last_seen, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![
            device.id.to_string(),
            device.label,
            credential.0.as_slice(),
            encode(&device.scopes)?,
            time_to_sql(device.created_at)?,
            device.last_seen.map(time_to_sql).transpose()?,
            device.revoked_at.map(time_to_sql).transpose()?,
        ])?;
    Ok(())
}

/// Record that a device authenticated at `at`. An unknown device is
/// ignored: it can only be one revoked and deleted by hand. Writer side.
pub(crate) fn seen(
    connection: &Connection,
    device: DeviceId,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let _rows = connection
        .prepare_cached("UPDATE devices SET last_seen = ?2 WHERE id = ?1")?
        .execute(params![device.to_string(), time_to_sql(at)?])?;
    Ok(())
}

/// Revoke a device at `at`. Revoking a revoked device keeps the first
/// time. Writer side.
pub(crate) fn revoke(
    connection: &Connection,
    device: DeviceId,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let id = device.to_string();
    let found = connection
        .prepare_cached("SELECT revoked_at FROM devices WHERE id = ?1")?
        .query_row([&id], |row| row.get::<_, Option<i64>>(0))
        .optional()?;
    match found {
        None => Err(StoreError::UnknownDevice(device)),
        Some(Some(_already)) => Ok(()),
        Some(None) => {
            let _rows = connection
                .prepare_cached("UPDATE devices SET revoked_at = ?2 WHERE id = ?1")?
                .execute(params![id, time_to_sql(at)?])?;
            Ok(())
        }
    }
}

/// Every device, oldest first. Reader side.
pub(crate) fn list(connection: &Connection) -> Result<Vec<DeviceInfo>, StoreError> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {DEVICE_COLUMNS} FROM devices ORDER BY created_at, id"
    ))?;
    let mut rows = statement.query([])?;
    let mut devices = Vec::new();
    while let Some(row) = rows.next()? {
        devices.push(device_from_row(row)?.info);
    }
    Ok(devices)
}

/// One device, with its credential hash. Reader side.
pub(crate) fn find(
    connection: &Connection,
    device: DeviceId,
) -> Result<Option<StoredDevice>, StoreError> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {DEVICE_COLUMNS} FROM devices WHERE id = ?1"
    ))?;
    let mut rows = statement.query([device.to_string()])?;
    rows.next()?.map(device_from_row).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_hash_matches_only_its_secret() {
        let secret = DeviceSecret::from_bytes([5; 32]);
        let hash = CredentialHash::of(&secret);
        assert!(hash.matches(&secret));
        assert!(!hash.matches(&DeviceSecret::from_bytes([6; 32])));
        assert_eq!(format!("{hash:?}"), "CredentialHash(..)");
    }

    #[test]
    fn an_agent_credential_hash_matches_only_its_secret() {
        let secret = AgentSecret::from_bytes([5; 32]);
        let hash = CredentialHash::of_agent(&secret);
        assert!(hash.matches_agent(&secret));
        assert!(!hash.matches_agent(&AgentSecret::from_bytes([6; 32])));
    }
}
