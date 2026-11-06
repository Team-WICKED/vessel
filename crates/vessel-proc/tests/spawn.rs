//! Process-level tests against the `fake-tui` testkit binary.
//!
//! `fake-tui` leaves a sleeping grandchild that shares its stdout. Reading
//! stdout to EOF therefore proves the grandchild is gone too: the tests wait
//! on that event instead of sleeping. `fake-tui` also exits 0 by itself
//! when its stdin ends, so a test that expects it to be stopped must keep
//! stdin open (binding it, not leaving it to `..`, which drops it). Every test runs under a generous
//! timeout so a regression fails instead of hanging CI.
//!
//! On Windows every test runs `fake-tui` through a `.cmd` shim, as npm
//! installs harnesses there: the direct child is `cmd.exe`, the fake is its
//! child and the sleeper its grandchild, and EOF proves all three are gone.

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use vessel_proc::{CommandSpec, Signal, Spawned, spawn};

use support::{drain, fake_tui, read_until, within};

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

    #[cfg(unix)]
    pub(super) fn fake_tui() -> CommandSpec {
        CommandSpec::new(Fake::Tui.path().unwrap())
    }

    /// `fake-tui` behind a `.cmd` shim, written once per test process.
    #[cfg(windows)]
    pub(super) fn fake_tui() -> CommandSpec {
        use std::path::PathBuf;
        use std::sync::OnceLock;

        static SHIM: OnceLock<PathBuf> = OnceLock::new();
        let shim = SHIM.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("vessel-proc-tests-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            vessel_testkit::write_cmd_shim(&dir, "fake-tui", &Fake::Tui.path().unwrap()).unwrap()
        });
        CommandSpec::new(shim)
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

    /// The exit code `wait` reports for a child that was stopped: none on
    /// Unix (a signal), the job's termination code on Windows.
    #[cfg(unix)]
    pub(super) const STOPPED: Option<i32> = None;
    #[cfg(windows)]
    pub(super) const STOPPED: Option<i32> = Some(1);
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
        // The leader exited on its own; the sweep in `wait` removed the
        // grandchild, so stdout reaches EOF.
        drop(drain(lines).await);
    })
    .await;
}

#[tokio::test]
async fn stopping_the_child_stops_its_group() {
    within(async {
        // `stdin` is held open for the whole test: `fake-tui` exits 0 by
        // itself at end of input, and with stdin dropped here (as `..` would
        // do) that exit raced the signal and sometimes won.
        let Spawned {
            mut child,
            stdin: _stdin,
            stdout,
            ..
        } = spawn(&fake_tui()).unwrap();
        let mut lines = BufReader::new(stdout).lines();
        drop(read_until(&mut lines, "ready").await);
        child.signal(Signal::Terminate).unwrap();
        assert_eq!(child.wait().await.unwrap(), support::STOPPED);
        drop(drain(lines).await);
        // Signalling after the group is gone is a harmless no-op.
        child.signal(Signal::Kill).unwrap();
    })
    .await;
}

#[tokio::test]
async fn dropping_an_unwaited_child_kills_its_group() {
    within(async {
        // Stdin stays open, so the child is still running when it is
        // dropped (it would exit by itself at end of input).
        let Spawned {
            child,
            stdin: _stdin,
            stdout,
            ..
        } = spawn(&fake_tui()).unwrap();
        let mut lines = BufReader::new(stdout).lines();
        drop(read_until(&mut lines, "ready").await);
        drop(child);
        drop(drain(lines).await);
    })
    .await;
}

/// A leader that exited but is not reaped yet: macOS refuses to signal its
/// group (`EPERM`: every member is a zombie). That must count as "already
/// gone", and the exit code must survive for `wait`.
///
/// macOS only: Linux signals a group of zombies without complaint, so the
/// test could not fail there. `kqueue`'s `NOTE_EXIT` reports the exit
/// without reaping, so the signal lands on the zombie every time.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn signalling_a_leader_that_exited_unreaped_succeeds() {
    use nix::errno::Errno;
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

    within(async {
        let Spawned {
            mut child, stdout, ..
        } = spawn(&fake_tui().arg("--version")).unwrap();
        let printed = drain(BufReader::new(stdout).lines()).await;
        assert!(printed.contains("fake-tui"), "{printed}");
        let watch = KEvent::new(
            usize::try_from(child.pid()).unwrap(),
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            0,
            0,
        );
        let mut seen = [watch];
        // Blocks until the exit; `ESRCH` if it had exited already.
        match Kqueue::new().unwrap().kevent(&[watch], &mut seen, None) {
            Ok(_) | Err(Errno::ESRCH) => {}
            Err(error) => panic!("cannot watch the child: {error}"),
        }
        child.signal(Signal::Terminate).unwrap();
        assert_eq!(child.wait().await.unwrap(), Some(0));
    })
    .await;
}
