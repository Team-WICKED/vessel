//! Install testkit fakes under real harness binary names.
//!
//! Enabled by the `fakes` feature, for tests in other crates (the
//! session runtime, the node's end-to-end tests). Placing a fake under the
//! name a harness is looked up by lets those tests run the real lookup and
//! launch logic, while the knowledge of which name that is stays in this
//! crate.

use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::HarnessKind;

use crate::harnesses::{Structured, descriptor};

/// Which program of a harness a fake stands in for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The binary run in terminal mode.
    Terminal,
    /// The program run in structured mode. For harnesses that speak ACP
    /// themselves this is the same file as [`Role::Terminal`].
    Structured,
}

/// Make `fake` reachable inside `dir` as `kind`'s program for `role`;
/// returns the path created.
///
/// Unix uses a symlink. Windows writes `<name>.cmd`, a shim that runs the
/// fake, because that is how npm installs `claude`, `codex`,
/// `claude-agent-acp` and `codex-acp` there: the lookup then resolves the
/// name through `PATHEXT` and the session's direct child is `cmd.exe`, with
/// the fake as its child, as in production.
///
/// # Errors
/// Fails if the link or copy cannot be created (for example the name is
/// already taken in `dir`).
pub fn install(dir: &Path, kind: HarnessKind, role: Role, fake: &Path) -> io::Result<PathBuf> {
    let descriptor = descriptor(kind);
    let name = match (role, &descriptor.structured) {
        (Role::Terminal, _) | (Role::Structured, Structured::Native { .. }) => descriptor.binary,
        (Role::Structured, Structured::Adapter { binary, .. }) => binary,
    };
    place(fake, dir, name)
}

#[cfg(unix)]
fn place(fake: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    let target = dir.join(name);
    std::os::unix::fs::symlink(fake, &target)?;
    Ok(target)
}

/// The shim runs the fake with every argument and returns its exit code.
/// (The same shape as `vessel_testkit::write_cmd_shim`, which this crate
/// cannot use: the testkit is a dev-dependency only.)
#[cfg(windows)]
fn place(fake: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    use std::io::Write as _;

    let target = dir.join(format!("{name}.cmd"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)?;
    write!(
        file,
        "@echo off\r\n\"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
        fake.display()
    )?;
    Ok(target)
}
