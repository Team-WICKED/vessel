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
/// Unix uses a symlink, Windows a copy (symlinks there need privileges).
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
    let target = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    place(fake, &target)?;
    Ok(target)
}

#[cfg(unix)]
fn place(fake: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fake, target)
}

#[cfg(not(unix))]
fn place(fake: &Path, target: &Path) -> io::Result<()> {
    std::fs::copy(fake, target).map(drop)
}
