//! Process-level tests against the `fake-tui` testkit binary.
//!
//! `fake-tui` leaves a sleeping grandchild that shares its stdout. Reading
//! stdout to EOF therefore proves the grandchild is gone too: the tests wait
//! on that event instead of sleeping. Every test runs under a generous
//! timeout so a regression fails instead of hanging CI.

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use vessel_proc::{CommandSpec, Signal, Spawned, spawn};

use support::{drain, fake_tui, finish, read_until, within};

/// Helpers live in a `#[cfg(test)]` module so clippy's test allowances
/// (`unwrap`, `panic`) cover them as they cover the tests.
#[cfg(test)]
mod support {
    use std::future::Future;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, BufReader, Lines};
    use tokio::process::ChildStdout;
    use vessel_proc::CommandSpec;
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(30);

    /// The safety net starts after the fakes are built: the first build of
    /// a run can take a minute (see `vessel_testkit::within`).
    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    pub(super) fn fake_tui() -> CommandSpec {
        CommandSpec::new(Fake::Tui.path().unwrap())
    }

    /// Read lines until one contains `needle`; returns every line read.
    pub(super) async fn read_until(
        lines: &mut Lines<BufReader<ChildStdout>>,
        needle: &str,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let done = line.contains(needle);
            seen.push(line);
            if done {
                return seen;
            }
        }
        panic!("stdout ended before {needle:?}; saw {seen:?}");
    }

    /// Read to EOF, which only happens once every holder of the pipe is gone.
    pub(super) async fn drain(lines: Lines<BufReader<ChildStdout>>) -> String {
        let mut rest = String::new();
        let _ = lines.into_inner().read_to_string(&mut rest).await.unwrap();
        rest
    }

    /// After the leader exited on its own: on Unix the sweep in `wait` killed
    /// the grandchild, so stdout reaches EOF.
    #[cfg(unix)]
    pub(super) async fn finish(lines: Lines<BufReader<ChildStdout>>, _seen: &str) {
        drop(drain(lines).await);
    }

    /// On Windows the grandchild survives its parent (documented gap), so it
    /// is removed by pid instead of waiting for EOF.
    #[cfg(windows)]
    pub(super) async fn finish(lines: Lines<BufReader<ChildStdout>>, seen: &str) {
        drop(lines);
        let pid = seen
            .lines()
            .find_map(|line| line.split_once("grandchild ").map(|(_, pid)| pid))
            .unwrap()
            .trim()
            .to_owned();
        let status = tokio::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid])
            .status()
            .await
            .unwrap();
        assert!(status.success());
    }
}

#[tokio::test]
async fn version_probe_reads_stdout_and_exit_code() {
    within(async {
        let Spawned {
            mut child, stdout, ..
        } = spawn(&fake_tui().arg("--version")).unwrap();
        let text = drain(BufReader::new(stdout).lines()).await;
        assert!(text.starts_with("fake-tui "), "{text}");
        assert_eq!(child.wait().await.unwrap(), Some(0));
    })
    .await;
}

#[tokio::test]
async fn missing_program_is_a_spawn_error() {
    let error = spawn(&CommandSpec::new("/definitely/not/a/program")).unwrap_err();
    assert!(
        matches!(error, vessel_proc::SpawnError::Start { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn environment_and_working_directory_are_applied() {
    within(async {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let spec = fake_tui()
            .env("VESSEL_SET", "yes")
            .env("VESSEL_GONE", "1")
            .env_remove("VESSEL_GONE")
            .cwd(&dir);
        let Spawned {
            mut child,
            mut stdin,
            stdout,
            ..
        } = spawn(&spec).unwrap();
        stdin
            .write_all(b"env VESSEL_SET\nenv VESSEL_GONE\ncwd\nexit 3\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let seen = read_until(&mut lines, "cwd ").await.join("\n");
        assert!(seen.contains("env VESSEL_SET=yes"), "{seen}");
        assert!(seen.contains("env VESSEL_GONE unset"), "{seen}");
        let reported = seen
            .lines()
            .find_map(|line| line.split_once("cwd ").map(|(_, dir)| dir))
            .unwrap();
        assert_eq!(std::path::Path::new(reported).canonicalize().unwrap(), dir);
        assert_eq!(child.wait().await.unwrap(), Some(3));
        finish(lines, &seen).await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_the_child_stops_its_group() {
    within(async {
        let Spawned {
            mut child, stdout, ..
        } = spawn(&fake_tui()).unwrap();
        let mut lines = BufReader::new(stdout).lines();
        drop(read_until(&mut lines, "ready").await);
        child.signal(Signal::Terminate).unwrap();
        assert_eq!(child.wait().await.unwrap(), None, "ended by a signal");
        drop(drain(lines).await);
        // Signalling after the group is gone is a harmless no-op.
        child.signal(Signal::Kill).unwrap();
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_an_unwaited_child_kills_its_group() {
    within(async {
        let Spawned { child, stdout, .. } = spawn(&fake_tui()).unwrap();
        let mut lines = BufReader::new(stdout).lines();
        drop(read_until(&mut lines, "ready").await);
        drop(child);
        drop(drain(lines).await);
    })
    .await;
}

#[cfg(windows)]
#[tokio::test]
async fn stopping_the_child_ends_it() {
    within(async {
        let Spawned {
            mut child, stdout, ..
        } = spawn(&fake_tui()).unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let seen = read_until(&mut lines, "ready").await.join("\n");
        child.signal(Signal::Terminate).unwrap();
        let _ = child.wait().await.unwrap();
        finish(lines, &seen).await;
    })
    .await;
}
