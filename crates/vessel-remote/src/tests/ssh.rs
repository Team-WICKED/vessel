//! The runner and platform detection through the stand-in `ssh`.

use super::*;
use crate::platform::{Arch, Os, Platform};
use crate::ssh::{Input, RemoteCommand, SSH_FAILED};

#[tokio::test]
async fn a_run_feeds_stdin_and_keeps_stdout_stderr_and_the_exit_code() {
    let remote = Remote::new();
    let script = b"echo out; echo \"home=$HOME\"; echo first >&2; echo last >&2; exit 3\n";
    let finished = remote
        .ssh()
        .run(
            &RemoteCommand::program("sh").arg("-s"),
            Input::Bytes(script),
        )
        .await
        .unwrap();
    assert_eq!(finished.code, Some(3));
    let home = remote.home.path().display();
    assert_eq!(finished.stdout_text(), format!("out\nhome={home}\n"));
    assert_eq!(finished.stderr.last(), Some("last"));
    let failed = finished.check().unwrap_err();
    assert_eq!(failed.to_string(), "the remote command exited 3: last");
}

#[tokio::test]
async fn a_command_that_stops_reading_early_is_not_an_error() {
    let remote = Remote::new();
    let big = vec![b'x'; 4 * 1024 * 1024];
    let finished = remote
        .ssh()
        .run(&RemoteCommand::program("true"), Input::Bytes(&big))
        .await
        .unwrap();
    assert!(finished.success(), "{finished:?}");
}

#[tokio::test]
async fn an_ssh_failure_is_reported_with_its_stderr() {
    let remote = Remote::new();
    let ssh = remote.ssh().with_env("FAKE_SSH_HOME", "/nonexistent/home");
    let finished = ssh
        .run(&RemoteCommand::program("true"), Input::Empty)
        .await
        .unwrap();
    assert_eq!(finished.code, Some(SSH_FAILED));
    assert!(
        finished
            .check()
            .unwrap_err()
            .to_string()
            .starts_with("ssh failed")
    );
}

#[tokio::test]
async fn a_missing_ssh_says_so() {
    let ssh = Ssh::new("box".parse().unwrap()).with_program("/nonexistent/ssh");
    let error = ssh
        .run(&RemoteCommand::program("true"), Input::Empty)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("OpenSSH"), "{error}");
}

#[tokio::test]
async fn the_platform_is_this_machines() {
    let remote = Remote::new();
    let platform = Platform::detect(&remote.ssh()).await.unwrap();
    let expected = Platform {
        os: if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        },
        arch: if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        },
    };
    assert_eq!(platform, expected);
}
