//! Installing by upload: local builds and release archives, checked on the
//! remote, run there, renamed into place; nothing left behind on failure.

use std::path::PathBuf;

use super::*;
use crate::install::{Build, InstallError, Installer, Kind, Sha256Digest};

async fn local_build(remote: &Remote, version: &str, triple: &str, on_remote: OnRemote) -> Build {
    let path = remote
        .local()
        .join(format!("vesseld-{version}-{}", triple.len()));
    fake_vesseld(&path, version, triple, on_remote);
    Build::local(&path).await.unwrap()
}

#[tokio::test]
async fn a_local_build_is_uploaded_run_and_named_by_content() {
    let remote = Remote::new();
    let build = local_build(&remote, "0.2.0", &local_triple(), OnRemote::Works).await;
    assert!(build.is_local());
    assert_eq!(build.version(), "0.2.0");
    assert_eq!(build.target(), local_triple());
    let name = build.binary_name();
    assert!(name.starts_with("vesseld-0.2.0-dev-"), "{name}");

    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let into = Installer::new(&ssh, &data_dir);
    assert_eq!(into.installed(&name).await.unwrap(), None);
    let done = into.install(&build).await.unwrap();
    assert!(done.uploaded);
    assert_eq!(done.version_line, format!("0.2.0 {}", local_triple()));
    assert_eq!(done.binary.as_str(), format!("~/.vessel/bin/{name}"));
    // Only the final file: no temporary upload left over.
    assert_eq!(remote.installed(), vec![name.clone()]);
    let file = remote.bin().join(&name);
    assert_eq!(mode(&file), 0o755);
    assert_eq!(mode(&remote.bin()), 0o700);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(
            remote
                .local()
                .join(format!("vesseld-0.2.0-{}", local_triple().len()))
        )
        .unwrap()
    );

    // The same build again: found, not uploaded.
    let again = into.install(&build).await.unwrap();
    assert!(!again.uploaded);
    assert_eq!(
        into.installed(&name).await.unwrap(),
        Some(format!("0.2.0 {}", local_triple()))
    );
}

#[tokio::test]
async fn a_build_that_does_not_run_on_the_remote_is_deleted() {
    let remote = Remote::new();
    let build = local_build(&remote, "0.2.0", &local_triple(), OnRemote::MissingGlibc).await;
    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let error = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap_err();
    let InstallError::DoesNotRun { output, hint } = &error else {
        panic!("{error:?}");
    };
    assert!(output.contains("GLIBC_2.99"), "{output}");
    if local_triple().contains("-linux-gnu") {
        let hint = hint.unwrap();
        assert!(hint.contains("musl"), "{hint}");
        assert!(error.to_string().contains("musl"), "{error}");
    }
    assert_eq!(remote.installed(), Vec::<String>::new(), "nothing kept");
}

#[tokio::test]
async fn a_build_reporting_another_version_remotely_is_refused() {
    let remote = Remote::new();
    let build = local_build(&remote, "0.2.0", &local_triple(), OnRemote::OtherVersion).await;
    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let error = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, InstallError::WrongVersion { reported, .. } if reported.contains("9.9.9")),
        "{error:?}"
    );
    assert_eq!(remote.installed(), Vec::<String>::new());
}

#[tokio::test]
async fn a_build_for_another_platform_is_refused_before_uploading() {
    let remote = Remote::new();
    let other = if cfg!(target_arch = "aarch64") {
        "x86_64-unknown-linux-gnu"
    } else {
        "aarch64-unknown-linux-gnu"
    };
    let build = local_build(&remote, "0.2.0", other, OnRemote::Works).await;
    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let error = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap_err();
    assert!(
        matches!(error, InstallError::WrongTarget { .. }),
        "{error:?}"
    );
    assert!(!remote.bin().exists());
}

#[tokio::test]
async fn a_corrupted_upload_is_refused_and_removed() {
    let remote = Remote::new();
    let mut build = local_build(&remote, "0.2.0", &local_triple(), OnRemote::Works).await;
    build.sha256 = Sha256Digest::of(b"something else");
    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let error = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap_err();
    assert!(matches!(error, InstallError::ChecksumMismatch), "{error:?}");
    assert_eq!(remote.installed(), Vec::<String>::new());
}

#[tokio::test]
async fn a_release_archive_is_unpacked_and_named_by_version() {
    let remote = Remote::new();
    let staging = remote.local().join("staging");
    std::fs::create_dir(&staging).unwrap();
    let triple = release_triple();
    fake_vesseld(&staging.join("vesseld"), "0.2.0", &triple, OnRemote::Works);
    let archive = remote
        .local()
        .join(format!("vesseld-0.2.0-{triple}.tar.gz"));
    let tar = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg("vesseld")
        .status()
        .unwrap();
    assert!(tar.success());
    let digest = Sha256Digest::of_file(&archive).await.unwrap();
    let build = Build::release(archive, "0.2.0".to_owned(), triple.clone(), digest);
    assert_eq!(build.kind, Kind::Release);

    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let installed = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap();
    assert!(installed.uploaded);
    assert_eq!(installed.version_line, format!("0.2.0 {triple}"));
    assert_eq!(remote.installed(), vec!["vesseld-0.2.0".to_owned()]);
    assert_eq!(mode(&remote.bin().join("vesseld-0.2.0")), 0o755);
    assert_eq!(
        std::fs::read(remote.bin().join("vesseld-0.2.0")).unwrap(),
        std::fs::read(staging.join("vesseld")).unwrap()
    );
}

/// The release target of this machine (what a release archive for it is
/// built for).
fn release_triple() -> String {
    let os = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else {
        "unknown-linux-musl"
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

#[tokio::test]
async fn a_data_dir_outside_home_is_used_as_given() {
    let remote = Remote::new();
    let build = local_build(&remote, "0.2.0", &local_triple(), OnRemote::Works).await;
    let elsewhere = remote.home.path().join("srv").join("vessel");
    let data_dir: RemotePath = elsewhere.to_str().unwrap().parse().unwrap();
    let ssh = remote.ssh();
    let installed = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap();
    assert!(
        installed
            .binary
            .as_str()
            .starts_with(elsewhere.to_str().unwrap())
    );
    let file: PathBuf = elsewhere.join("bin").join(build.binary_name());
    assert!(file.is_file());
}
