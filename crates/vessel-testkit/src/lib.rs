//! Fake harnesses for Vessel's process-level tests.
//!
//! Tests must never run the real `claude`/`codex` or touch the network
//! (`AGENTS.md` section 8). This crate builds small stand-in programs that
//! behave like the parts of a harness the runtime depends on, and tells tests
//! where the built binaries are.
//!
//! - `fake-tui`: a line-oriented "TUI" that echoes input, reports its window
//!   size, and leaves a sleeping grandchild behind so group kills can be
//!   checked. See `src/bin/fake_tui.rs` for its exact behaviour.
//! - `fake-acp`: a scripted ACP v1 agent on stdio (message, thought, tool
//!   call, plan, permission request, cancel, `session/load`, a model config
//!   option). See `src/bin/fake_acp.rs`.
//! - `fake-ssh`: the OpenSSH client's command line, running the "remote"
//!   command on this machine under a fake home, with an injectable
//!   connection drop (ADR 0007 tests). See `src/bin/fake_ssh.rs`.
//! - `fake-tool`: an installer or service manager that only logs its calls
//!   and answers "is it enabled" from that log (`vessel init` tests). See
//!   `src/bin/fake_tool.rs`.
//! - `fake-provider`: an OpenAI-compatible model provider on loopback for
//!   the model routing smoke's `--fake` run (milestone 5). See
//!   `src/bin/fake_provider.rs`.
//!
//! Both harness fakes answer `report <path>` by writing their arguments,
//! environment and (ACP) MCP servers to that file ([`write_report`]), so a
//! test sees values without them entering the session's log, and `model
//! <text>` by making one model call where a routed session's harness is
//! pointed ([`model::call`], milestone 5).
//!
//! It also locates a freshly built `vesseld` ([`vesseld()`]) for tests
//! outside `vessel-node` that need the real node binary (installing it on a
//! fake remote, attaching to it over `fake-ssh`).
//!
//! With the `relay` feature, [`relay::MisbehavingRelay`] is a relay that
//! drops, replays, reorders, corrupts or delays messages on command
//! (milestone 3).
//!
//! With the `upstream` feature, [`upstream::FakeUpstream`] is a model
//! provider on loopback that answers with scripted streams, error statuses,
//! drops, silences and hangs, and records what it was asked (milestone 5).
//!
//! This crate is a dev-dependency only; nothing in `vesseld` links it.
//!
//! [Decision Log]
//! - Purpose: crates such as `vessel-proc` and `vessel-pty` need the path of
//!   a fake binary at test time, but Cargo only exposes `CARGO_BIN_EXE_*` to
//!   the integration tests of the package that owns the binary, and builds a
//!   package's binaries only when that package's own tests run.
//! - Alternatives: artifact dependencies (unstable Cargo); requiring
//!   `cargo test --workspace` and guessing the path (stale or missing
//!   binaries after `cargo test -p vessel-pty`, or after editing a fake);
//!   copying fake logic into every crate's tests (duplication).
//! - Choice: [`Fake::path`] runs `cargo build -p vessel-testkit --bins` once
//!   per test process (Cargo releases its build lock while tests run) and
//!   returns the binary next to the test executable.
//! - Constraint: the nested build uses the same target directory and profile
//!   as the test executable, derived from its own path, so it never rebuilds
//!   the world under different settings.
//! - Cost (measured 2026-11-06): the nested build is *not* a no-op after
//!   `cargo test --workspace`. Cargo unifies dependency features across
//!   every package it builds, so this package built alone resolves different
//!   features than the workspace build did, and the first nested build of a
//!   run recompiles about 30 crates (tokio, serde, the ACP SDK and their
//!   dependents): 22 s on the Linux CI runner, about 60 s on the Windows
//!   one. Later test binaries of the same run find it fresh (well under a
//!   second); so does every run once the target directory holds both
//!   variants.
//! - Consequence: the first call can take a minute and blocks its thread.
//!   It must never run inside a test's deadline. [`within`] builds first and
//!   only then starts the clock; tests with a deadline use it. Before this,
//!   the first two agent tests on Windows CI spent their whole 30 s safety
//!   net (and their runtime thread) waiting for the build and timed out.
//! - Alternatives for the cost itself: declare every feature the workspace
//!   enables on this package's dependencies (brittle: any other crate
//!   adding a feature reopens the gap); build into a separate target
//!   directory (the first build there compiles everything, which is slower);
//!   CI builds `-p vessel-testkit --bins` in a step of its own before the
//!   tests, which keeps test timings clean and puts the cost where it shows.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::time::error::Elapsed;

pub mod model;
#[cfg(feature = "relay")]
pub mod relay;
mod report;
#[cfg(feature = "upstream")]
pub mod upstream;
mod vesseld;

pub use report::write_report;

pub use vesseld::vesseld;

/// A fake harness binary built from this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fake {
    /// `fake-tui`: echoes stdin, prints `size COLSxROWS` on start and on
    /// `SIGWINCH` (Unix), runs `exit [code]`, `env NAME` and `cwd` commands,
    /// and keeps a sleeping grandchild in its process group.
    Tui,
    /// `fake-acp`: a scripted ACP agent; its prompt text picks a scenario
    /// (`plain`, `wait`, or the full tool-call-and-permission turn).
    Acp,
    /// `fake-ssh`: runs the "remote" command locally under
    /// `$FAKE_SSH_HOME`, and can drop the connection
    /// (`FAKE_SSH_DROP_AFTER`). Unix only. See `src/bin/fake_ssh.rs`.
    Ssh,
    /// `fake-tool`: stands in for `npm`, `systemctl` or `launchctl`,
    /// logging each call to `$FAKE_TOOL_LOG`. See `src/bin/fake_tool.rs`.
    Tool,
}

impl Fake {
    /// The binary's name, without the platform's executable suffix.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tui => "fake-tui",
            Self::Acp => "fake-acp",
            Self::Ssh => "fake-ssh",
            Self::Tool => "fake-tool",
        }
    }

    /// Path of the freshly built binary.
    ///
    /// The first call in a process runs the nested build, which can take a
    /// minute and blocks the calling thread (see the crate documentation).
    /// Inside a test with a deadline, call it only after [`within`] started
    /// the test, never before the deadline is set up in some other way.
    ///
    /// # Errors
    /// Fails if the test executable's location cannot be determined, the
    /// nested `cargo build` fails, or the binary is still missing afterwards.
    pub fn path(self) -> Result<PathBuf, LocateError> {
        let dir = built_dir()?;
        let path = dir.join(format!("{}{}", self.name(), std::env::consts::EXE_SUFFIX));
        if path.is_file() {
            Ok(path)
        } else {
            Err(LocateError::Missing { path })
        }
    }
}

/// Build the fakes now (once per process; later calls return at once).
///
/// Blocks the calling thread for as long as the nested build takes.
///
/// # Errors
/// As [`Fake::path`].
pub fn prepare() -> Result<(), LocateError> {
    built_dir().map(drop)
}

/// Run `test` with a deadline of `limit`, counted from *after* the fakes are
/// built.
///
/// This is the safety net integration tests put around themselves so a hang
/// becomes a failure. The one-time nested build (up to a minute on a cold CI
/// runner) runs first, on the calling thread, before the clock starts:
/// charged to the deadline it made the first tests of a run time out, and
/// since it blocks the runtime thread the deadline cannot even fire until it
/// is over. A failed build is not reported here; the [`Fake::path`] call
/// that needs the binary reports it.
///
/// # Errors
/// [`Elapsed`] when `test` did not finish within `limit`.
pub async fn within<F: Future>(limit: Duration, test: F) -> Result<F::Output, Elapsed> {
    // A build error is cached; the `Fake::path` call that needs the binary
    // returns it.
    timed(|| drop(prepare()), limit, test).await
}

/// `setup`, then `test` under a deadline that starts once `setup` returned.
async fn timed<F: Future>(
    setup: impl FnOnce(),
    limit: Duration,
    test: F,
) -> Result<F::Output, Elapsed> {
    setup();
    tokio::time::timeout(limit, test).await
}

/// Write `<dir>/<name>.cmd`, a shim that runs `program` with every argument
/// and returns its exit code; returns the shim's path.
///
/// npm installs Windows command-line tools this way, so a test that runs a
/// fake through a shim gets `cmd.exe` as the direct child and the fake as
/// its child, the shape that stopping a session must handle there.
///
/// # Errors
/// The file exists already or cannot be written.
#[cfg(windows)]
pub fn write_cmd_shim(dir: &Path, name: &str, program: &Path) -> std::io::Result<PathBuf> {
    use std::io::Write as _;

    let shim = dir.join(format!("{name}.cmd"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&shim)?;
    write!(
        file,
        "@echo off\r\n\"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
        program.display()
    )?;
    Ok(shim)
}

/// Why a fake binary could not be provided.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LocateError {
    /// The running test executable has no usable path.
    #[error("cannot locate the test executable: {message}")]
    CurrentExe {
        /// What went wrong (kept as text so the error can be cached and cloned).
        message: String,
    },
    /// The nested `cargo build` failed.
    #[error("building the test binaries failed:\n{stderr}")]
    Build {
        /// Cargo's error output.
        stderr: String,
    },
    /// The build succeeded but the binary is not where it should be.
    #[error("test binary not found at {}", path.display())]
    Missing {
        /// Where it was expected.
        path: PathBuf,
    },
}

/// The profile directory (`target/<profile>`) holding the built fakes.
///
/// Built once per process; every later call returns the cached outcome.
fn built_dir() -> Result<PathBuf, LocateError> {
    static BUILT: OnceLock<Result<PathBuf, LocateError>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let profile_dir = profile_dir()?;
            build(&profile_dir, &["-p", "vessel-testkit", "--bins"])?;
            Ok(profile_dir)
        })
        .clone()
}

/// `target/<profile>` of the running test executable.
///
/// Test executables live in `target/<profile>/deps/`; a binary run directly
/// lives in `target/<profile>/`.
fn profile_dir() -> Result<PathBuf, LocateError> {
    let exe = std::env::current_exe().map_err(|error| LocateError::CurrentExe {
        message: error.to_string(),
    })?;
    let parent = exe.parent().ok_or_else(|| LocateError::CurrentExe {
        message: format!("{} has no parent directory", exe.display()),
    })?;
    let dir = if parent.file_name().is_some_and(|name| name == "deps") {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    };
    Ok(dir.to_path_buf())
}

/// Run the nested build of `what` (`-p <package> --bins` or `--bin`), if
/// Cargo is driving this process.
///
/// Without `CARGO` (a test executable run by hand) the existing binaries are
/// used as they are.
fn build(profile_dir: &Path, what: &[&str]) -> Result<(), LocateError> {
    let Some(cargo) = std::env::var_os("CARGO") else {
        return Ok(());
    };
    let (Some(target_dir), Some(profile)) = (profile_dir.parent(), profile_dir.file_name()) else {
        return Err(LocateError::CurrentExe {
            message: format!("{} is not inside a target directory", profile_dir.display()),
        });
    };
    // The `dev` profile builds into `debug/`; every other profile into a
    // directory of its own name.
    let profile = if profile == "debug" {
        OsString::from("dev")
    } else {
        profile.to_os_string()
    };
    let output = Command::new(cargo)
        .args(["build", "--quiet"])
        .args(what)
        .arg("--profile")
        .arg(profile)
        .arg("--target-dir")
        .arg(target_dir)
        .output()
        .map_err(|error| LocateError::Build {
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(LocateError::Build {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A setup that blocks the thread (as the nested build does), then a
    /// test that has to wait for something afterwards (as every process
    /// test does).
    ///
    /// This is the one test here that is about time, so it uses real time
    /// with wide margins: the setup outlasts the limit by 100 ms, and the
    /// test needs 10 ms of the limit's 200 ms.
    const SETUP: Duration = Duration::from_millis(300);
    const LIMIT: Duration = Duration::from_millis(200);

    fn slow_setup() {
        std::thread::sleep(SETUP);
    }

    async fn short_test() -> u8 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        7
    }

    #[tokio::test]
    async fn the_deadline_starts_after_a_slow_setup() {
        assert_eq!(timed(slow_setup, LIMIT, short_test()).await, Ok(7));
    }

    /// The shape that failed on Windows CI: the setup inside the deadline.
    /// Kept to show the test above would catch a regression.
    #[tokio::test]
    async fn a_setup_inside_the_deadline_uses_it_up() {
        let charged = tokio::time::timeout(LIMIT, async {
            slow_setup();
            short_test().await
        })
        .await;
        assert!(charged.is_err(), "{charged:?}");
    }
}
