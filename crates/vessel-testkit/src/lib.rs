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
//!   returns the binary next to the test executable. A fresh build is a
//!   no-op check, so the cost is a fraction of a second per test binary.
//! - Constraint: the nested build uses the same target directory and profile
//!   as the test executable, derived from its own path, so it never rebuilds
//!   the world under different settings.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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
}

impl Fake {
    /// The binary's name, without the platform's executable suffix.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tui => "fake-tui",
            Self::Acp => "fake-acp",
        }
    }

    /// Path of the freshly built binary.
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

/// Why a fake binary could not be provided.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LocateError {
    /// The running test executable has no usable path.
    #[error("cannot locate the test executable: {message}")]
    CurrentExe {
        /// What went wrong (kept as text so the error can be cached and cloned).
        message: String,
    },
    /// `cargo build -p vessel-testkit --bins` failed.
    #[error("building the fake harnesses failed:\n{stderr}")]
    Build {
        /// Cargo's error output.
        stderr: String,
    },
    /// The build succeeded but the binary is not where it should be.
    #[error("fake harness binary not found at {}", path.display())]
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
            build(&profile_dir)?;
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

/// Run the nested build, if Cargo is driving this process.
///
/// Without `CARGO` (a test executable run by hand) the existing binaries are
/// used as they are.
fn build(profile_dir: &Path) -> Result<(), LocateError> {
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
        .args([
            "build",
            "--quiet",
            "-p",
            "vessel-testkit",
            "--bins",
            "--profile",
        ])
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
