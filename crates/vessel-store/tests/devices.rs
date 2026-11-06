//! Paired devices in the store (ADR 0010).

use fixtures::{at, device, store};
use vessel_proto::{DeviceId, DeviceSecret};
use vessel_store::{CredentialHash, Store, StoreConfig, StoreError};

/// Builders. A `cfg(test)` module so the lints' test allowances (`unwrap`
/// and friends) apply to it.
#[cfg(test)]
mod fixtures {
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;
    use vessel_proto::{DeviceId, DeviceInfo, Scope};
    use vessel_store::{DataDir, Store, StoreConfig};

    pub(crate) fn at(second: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000_i64.saturating_add(second), 0).unwrap()
    }

    pub(crate) fn device(label: &str, created: i64) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::random(),
            label: label.to_owned(),
            created_at: at(created),
            last_seen: None,
            scopes: Scope::ALL.to_vec(),
            revoked_at: None,
        }
    }

    pub(crate) async fn store() -> (TempDir, DataDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let dir = DataDir::new(temp.path());
        dir.prepare().unwrap();
        let store = Store::open(&dir, StoreConfig::default())
            .await
            .unwrap()
            .store;
        (temp, dir, store)
    }
}

#[tokio::test]
async fn devices_are_stored_with_their_credential_hash_only() {
    let (_temp, _dir, store) = store().await;
    let secret = DeviceSecret::from_bytes([4; 32]);
    let laptop = device("laptop", 0);
    let phone = device("phone", 1);
    store
        .add_device(laptop.clone(), CredentialHash::of(&secret))
        .await
        .unwrap();
    store
        .add_device(
            phone.clone(),
            CredentialHash::of(&DeviceSecret::from_bytes([5; 32])),
        )
        .await
        .unwrap();

    assert_eq!(store.devices().await.unwrap(), [laptop.clone(), phone]);
    let found = store.device(laptop.id).await.unwrap().unwrap();
    assert_eq!(found.info, laptop);
    assert!(found.credential.matches(&secret));
    assert!(!found.credential.matches(&DeviceSecret::from_bytes([5; 32])));
    assert!(store.device(DeviceId::random()).await.unwrap().is_none());

    assert!(matches!(
        store
            .add_device(laptop.clone(), CredentialHash::of(&secret))
            .await,
        Err(StoreError::DeviceExists(id)) if id == laptop.id
    ));
}

#[tokio::test]
async fn revoking_keeps_a_tombstone_and_its_first_time() {
    let (_temp, dir, store) = store().await;
    let laptop = device("laptop", 0);
    let secret = DeviceSecret::from_bytes([4; 32]);
    store
        .add_device(laptop.clone(), CredentialHash::of(&secret))
        .await
        .unwrap();
    store.device_seen(laptop.id, at(5)).await.unwrap();
    store.revoke_device(laptop.id, at(10)).await.unwrap();
    store.revoke_device(laptop.id, at(20)).await.unwrap();

    let found = store.device(laptop.id).await.unwrap().unwrap();
    assert_eq!(found.info.last_seen, Some(at(5)));
    assert_eq!(found.info.revoked_at, Some(at(10)));
    assert!(
        found.credential.matches(&secret),
        "kept to answer `Revoked`"
    );

    let unknown = DeviceId::random();
    assert!(matches!(
        store.revoke_device(unknown, at(30)).await,
        Err(StoreError::UnknownDevice(id)) if id == unknown
    ));

    // Survives a restart.
    store.close().await;
    drop(store);
    let reopened = Store::open(&dir, StoreConfig::default())
        .await
        .unwrap()
        .store;
    assert_eq!(
        reopened.devices().await.unwrap()[0].revoked_at,
        Some(at(10))
    );
}

/// A database from milestone 2 (schema version 1) gains the devices table
/// on open, and keeps its sessions.
#[tokio::test]
async fn a_version_1_database_gains_the_devices_table() {
    let (_temp, dir, store) = store().await;
    store.close().await;
    drop(store);
    let connection = rusqlite::Connection::open(dir.database_path()).unwrap();
    connection.execute_batch("DROP TABLE devices").unwrap();
    connection.pragma_update(None, "user_version", 1).unwrap();
    drop(connection);

    let store = Store::open(&dir, StoreConfig::default())
        .await
        .unwrap()
        .store;
    assert!(store.devices().await.unwrap().is_empty());
    let laptop = device("laptop", 0);
    store
        .add_device(
            laptop,
            CredentialHash::of(&DeviceSecret::from_bytes([1; 32])),
        )
        .await
        .unwrap();
}
