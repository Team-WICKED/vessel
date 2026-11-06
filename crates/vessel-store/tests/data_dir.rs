//! The data directory's identity files.

use std::io::ErrorKind;

use vessel_store::{DataDir, StoreError};

#[test]
fn identity_is_created_once_and_then_stable() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path().join("nested").join("vessel"));
    dir.prepare().unwrap();

    let id = dir.load_or_create_node_id().unwrap();
    assert_eq!(dir.load_or_create_node_id().unwrap(), id);

    let token = dir.load_or_create_token().unwrap();
    assert!(token.verify(dir.load_or_create_token().unwrap().expose()));
    assert!(token.verify(dir.read_token().unwrap().expose()));
    assert!(token.verify(token.expose()));
    assert!(!token.verify("not the token"));
}

#[test]
fn clients_do_not_create_a_token() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    let Err(StoreError::Io { source, .. }) = dir.read_token() else {
        panic!("expected a missing token file");
    };
    assert_eq!(source.kind(), ErrorKind::NotFound);
    assert!(!dir.token_path().exists());
}

#[test]
fn a_damaged_token_file_is_an_error_not_a_new_token() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    std::fs::write(dir.token_path(), "garbage").unwrap();
    assert!(matches!(
        dir.load_or_create_token(),
        Err(StoreError::InvalidToken { .. })
    ));
}

#[cfg(unix)]
#[test]
fn secrets_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path().join("vessel"));
    dir.prepare().unwrap();
    let _token = dir.load_or_create_token().unwrap();
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.root()), 0o700);
    assert_eq!(mode(&dir.token_path()), 0o600);
    let _id = dir.load_or_create_node_id().unwrap();
    assert_eq!(mode(&dir.root().join("node.id")), 0o600);
    let _key = dir.load_or_create_node_key().unwrap();
    assert_eq!(mode(&dir.node_key_path()), 0o600);
}

#[test]
fn the_node_key_is_created_once_and_then_stable() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    dir.prepare().unwrap();
    let key = dir.load_or_create_node_key().unwrap();
    assert_eq!(std::fs::read(dir.node_key_path()).unwrap(), key.expose());
    assert_eq!(
        dir.load_or_create_node_key().unwrap().expose(),
        key.expose()
    );
    assert_eq!(dir.read_node_key().unwrap().expose(), key.expose());
    assert_eq!(format!("{key:?}"), "NodeKeySecret(<redacted>)");
}

/// A key of the wrong size is reported, never replaced: a new key would
/// unpair every device without anyone deciding to.
#[test]
fn a_damaged_node_key_is_an_error_not_a_new_key() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    std::fs::write(dir.node_key_path(), [7_u8; 31]).unwrap();
    assert!(matches!(
        dir.load_or_create_node_key(),
        Err(StoreError::InvalidNodeKey { len: 31, .. })
    ));
    assert_eq!(std::fs::read(dir.node_key_path()).unwrap(), [7_u8; 31]);
}

/// A data directory that already exists with group or other access (one
/// made by hand, or left 0775 by an earlier version) is made owner-only.
#[cfg(unix)]
#[test]
fn an_existing_open_directory_is_made_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("vessel");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o775)).unwrap();
    DataDir::new(&root).prepare().unwrap();
    let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

/// The event log and `SQLite`'s `-wal` and `-shm` files are owner-only,
/// whether new or left readable by an earlier version.
#[cfg(unix)]
#[tokio::test]
async fn the_event_log_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    let opened = vessel_store::Store::open(&dir, vessel_store::StoreConfig::default())
        .await
        .unwrap();
    // SQLite creates the WAL sidecars on the first transaction, not on open;
    // a read makes sure they exist before their modes are checked.
    assert!(opened.store.sessions().await.unwrap().is_empty());
    let database = dir.database_path();
    let sidecars = ["vessel.db-wal", "vessel.db-shm"].map(|name| temp.path().join(name));
    assert_eq!(mode(&database), 0o600);
    for sidecar in &sidecars {
        assert_eq!(mode(sidecar), 0o600, "{}", sidecar.display());
    }
    opened.store.close().await;
    drop(opened);

    // As an earlier version left them.
    for path in std::iter::once(&database).chain(&sidecars) {
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
    }
    let reopened = vessel_store::Store::open(&dir, vessel_store::StoreConfig::default())
        .await
        .unwrap();
    for path in std::iter::once(&database).chain(&sidecars) {
        if path.exists() {
            assert_eq!(mode(path), 0o600, "{}", path.display());
        }
    }
    reopened.store.close().await;
}

fn owner(pid: u32) -> vessel_store::NodeOwner {
    vessel_store::NodeOwner {
        pid,
        executable: std::path::PathBuf::from(format!("/bin/vesseld-{pid}")),
        version: "0.1.0".to_owned(),
        started_at: chrono::DateTime::UNIX_EPOCH,
    }
}

/// One node per data directory: a second lock fails and names the holder,
/// and the lock is free again once its holder lets go.
#[test]
fn the_node_lock_admits_one_holder_and_names_it() {
    use vessel_store::NodeLock;

    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    dir.prepare().unwrap();
    assert_eq!(NodeLock::holder(&dir).unwrap(), None);

    let held = NodeLock::acquire(&dir, &owner(41)).unwrap();
    assert_eq!(NodeLock::holder(&dir).unwrap(), Some(owner(41)));
    let Err(StoreError::Locked { holder, .. }) = NodeLock::acquire(&dir, &owner(42)) else {
        panic!("a second node must not get the lock");
    };
    assert_eq!(holder.as_deref(), Some(&owner(41)));

    drop(held);
    // The record is left behind, but without the lock it names nobody.
    assert!(dir.owner_path().exists());
    assert_eq!(NodeLock::holder(&dir).unwrap(), None);
    let _again = NodeLock::acquire(&dir, &owner(43)).unwrap();
    assert_eq!(NodeLock::holder(&dir).unwrap(), Some(owner(43)));
}

#[test]
fn profiles_are_read_as_written_and_flagged_when_shared() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    dir.prepare().unwrap();
    assert_eq!(dir.read_profiles().unwrap(), None);

    let text = "[profiles.alt]\nharness = \"claude\"\n";
    std::fs::write(dir.profiles_path(), text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.profiles_path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let read = dir.read_profiles().unwrap().unwrap();
        assert_eq!(read.text, text);
        assert!(!read.readable_by_others);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(dir.read_profiles().unwrap().unwrap().readable_by_others);
    }
    let read = dir.read_profiles().unwrap().unwrap();
    assert_eq!(read.text, text);
    assert!(!format!("{read:?}").contains("harness"));
}

#[test]
fn the_mcp_directory_is_owner_only_and_starts_empty() {
    let temp = tempfile::tempdir().unwrap();
    let dir = DataDir::new(temp.path());
    dir.prepare().unwrap();
    let mcp = dir.prepare_mcp_dir().unwrap();
    assert_eq!(mcp, dir.mcp_dir());
    std::fs::write(mcp.join("vessel-mcp-stale.json"), "{}").unwrap();
    assert_eq!(dir.prepare_mcp_dir().unwrap(), mcp);
    assert_eq!(std::fs::read_dir(&mcp).unwrap().count(), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&mcp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
