//! Fetching a release through a stand-in `gh`, then installing it.

use super::*;
use crate::install::{Installer, Sha256Digest};
use crate::platform::Platform;
use crate::release::{CHECKSUMS, ReleaseError, Releases};

/// A stand-in `gh` that "downloads" every file of `published` into the
/// `--dir` it is given, and records its arguments in `<published>/args`.
fn gh(remote: &Remote, published: &Path) -> PathBuf {
    let path = remote.local().join("gh");
    executable(
        &path,
        &format!(
            "#!/bin/sh\n\
             echo \"$*\" > '{published}/args'\n\
             [ -f '{published}/fail' ] && {{ echo 'HTTP 404: Not Found' >&2; exit 1; }}\n\
             dir=''; previous=''\n\
             for word in \"$@\"; do [ \"$previous\" = --dir ] && dir=$word; previous=$word; done\n\
             for file in '{published}'/*.tar.gz '{published}/{CHECKSUMS}'; do [ -f \"$file\" ] && cp \"$file\" \"$dir\"/; done\n\
             exit 0\n",
            published = published.display(),
        ),
    );
    path
}

/// Publish a release `0.2.0` for this machine into a directory.
fn publish(remote: &Remote, corrupt: bool) -> (PathBuf, String) {
    let published = remote.local().join("published");
    let staging = remote.local().join("staging");
    std::fs::create_dir_all(&published).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    let platform = local_platform();
    fake_vesseld(
        &staging.join("vesseld"),
        "0.2.0",
        platform.release_target(),
        OnRemote::Works,
    );
    let asset = Releases::asset("0.2.0", platform);
    let archive = published.join(&asset);
    let tar = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg("vesseld")
        .status()
        .unwrap();
    assert!(tar.success());
    let bytes = std::fs::read(&archive).unwrap();
    let digest = if corrupt {
        Sha256Digest::of(b"not the archive")
    } else {
        Sha256Digest::of(&bytes)
    };
    std::fs::write(
        published.join(CHECKSUMS),
        format!(
            "{digest}  {asset}\n{}  other.tar.gz\n",
            Sha256Digest::of(b"x")
        ),
    )
    .unwrap();
    (published, asset)
}

fn local_platform() -> Platform {
    Platform::from_uname(&format!(
        "{} {}",
        if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        },
        std::env::consts::ARCH
    ))
    .unwrap()
}

#[tokio::test]
async fn a_release_is_fetched_checked_and_installed() {
    let remote = Remote::new();
    let (published, asset) = publish(&remote, false);
    let releases = Releases::new("Team-WICKED/vessel").with_program(gh(&remote, &published));
    let downloads = tempfile::tempdir().unwrap();
    let build = releases
        .fetch("0.2.0", local_platform(), downloads.path())
        .await
        .unwrap();
    assert!(!build.is_local());
    assert_eq!(build.binary_name(), "vesseld-0.2.0");
    let args = std::fs::read_to_string(published.join("args")).unwrap();
    assert!(
        args.starts_with(&format!(
            "release download v0.2.0 --repo Team-WICKED/vessel --pattern {asset} --pattern SHA256SUMS"
        )),
        "{args}"
    );

    let ssh = remote.ssh();
    let data_dir = Remote::data_dir();
    let installed = Installer::new(&ssh, &data_dir)
        .install(&build)
        .await
        .unwrap();
    assert!(installed.uploaded);
    assert_eq!(remote.installed(), vec!["vesseld-0.2.0".to_owned()]);
}

#[tokio::test]
async fn a_release_that_does_not_match_its_checksums_is_refused() {
    let remote = Remote::new();
    let (published, asset) = publish(&remote, true);
    let releases = Releases::default().with_program(gh(&remote, &published));
    let downloads = tempfile::tempdir().unwrap();
    let error = releases
        .fetch("0.2.0", local_platform(), downloads.path())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ReleaseError::Mismatch { asset: named } if *named == asset),
        "{error:?}"
    );
}

#[tokio::test]
async fn gh_failures_and_absence_are_explained() {
    let remote = Remote::new();
    let (published, _asset) = publish(&remote, false);
    std::fs::write(published.join("fail"), "").unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let failing = Releases::default().with_program(gh(&remote, &published));
    let error = failing
        .fetch("0.2.0", local_platform(), downloads.path())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ReleaseError::Download { message, .. } if message.contains("404")),
        "{error:?}"
    );

    let absent = Releases::default().with_program("/nonexistent/gh");
    let error = absent
        .fetch("0.2.0", local_platform(), downloads.path())
        .await
        .unwrap_err();
    assert!(matches!(error, ReleaseError::NoGh(_)), "{error:?}");
    assert!(error.to_string().contains("--from local"));

    // A release without this platform's archive.
    std::fs::remove_file(published.join("fail")).unwrap();
    let other = Platform::from_uname(if cfg!(target_os = "macos") {
        "Linux x86_64"
    } else {
        "Darwin arm64"
    })
    .unwrap();
    let error = failing
        .fetch("0.2.0", other, downloads.path())
        .await
        .unwrap_err();
    assert!(
        matches!(error, ReleaseError::NoChecksum { .. }),
        "{error:?}"
    );
}
