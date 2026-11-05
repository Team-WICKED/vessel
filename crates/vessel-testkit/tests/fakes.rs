//! Smoke tests for the fakes themselves.
//!
//! Having an integration test also makes `cargo test --workspace` build the
//! fake binaries before any test runs (Cargo builds a package's binaries only
//! for its integration tests).

use std::io::Write as _;
use std::process::{Command, Stdio};

use vessel_testkit::Fake;

#[test]
fn locator_finds_the_cargo_built_binaries() {
    for (fake, built) in [
        (Fake::Tui, env!("CARGO_BIN_EXE_fake-tui")),
        (Fake::Acp, env!("CARGO_BIN_EXE_fake-acp")),
    ] {
        let located = fake.path().unwrap();
        let built = std::path::PathBuf::from(built);
        assert_eq!(
            located.canonicalize().unwrap(),
            built.canonicalize().unwrap()
        );
    }
}

#[test]
fn fake_acp_answers_the_version_probe_and_can_fail_at_start() {
    let output = Command::new(env!("CARGO_BIN_EXE_fake-acp"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("fake-acp ")
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fake-acp"))
        .env("FAKE_ACP_EXIT_EARLY", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "fake-acp: not logged in"
    );
}

#[test]
fn fake_tui_answers_the_version_probe() {
    let output = Command::new(env!("CARGO_BIN_EXE_fake-tui"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.trim(),
        format!("fake-tui {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn fake_tui_runs_commands_and_exits_with_the_requested_code() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fake-tui"))
        .env("FAKE_TUI_PROBE", "present")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"hello\nenv FAKE_TUI_PROBE\nexit 7\n")
        .unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(7));
    // The grandchild still holds stdout open; the group is not killed here,
    // so do not read to EOF. Kill it through its reported pid instead.
    let stdout = child.stdout.take().unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
    let mut seen = Vec::new();
    for line in lines.by_ref() {
        let line = line.unwrap();
        let done = line.contains("env FAKE_TUI_PROBE=");
        seen.push(line);
        if done {
            break;
        }
    }
    let text = seen.join("\n");
    assert!(text.contains("echo: hello"), "{text}");
    assert!(text.contains("env FAKE_TUI_PROBE=present"), "{text}");
    let grandchild = seen
        .iter()
        .find_map(|line| line.strip_prefix("grandchild "))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(kill(grandchild).unwrap().success());
}

#[cfg(unix)]
fn kill(pid: u32) -> std::io::Result<std::process::ExitStatus> {
    Command::new("kill").args(["-9", &pid.to_string()]).status()
}

#[cfg(windows)]
fn kill(pid: u32) -> std::io::Result<std::process::ExitStatus> {
    Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()
}
