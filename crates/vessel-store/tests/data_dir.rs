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
}
