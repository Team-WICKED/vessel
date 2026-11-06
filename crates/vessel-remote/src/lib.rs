//! Reaching Vessel nodes over SSH (ADRs 0007 and 0008).
//!
//! - [`ssh`]: runs the system `ssh` with fixed options, builds remote
//!   commands from fixed words and quoted values, keeps the end of stderr
//!   for errors.
//! - [`platform`]: which `vesseld` build a machine needs.
//! - [`install`]: uploading a local build or a release archive, checked on
//!   the remote, run there once, then renamed into place; `cleanup`.
//! - [`release`]: fetching a release archive locally with `gh` and
//!   checking it against the release's `SHA256SUMS`.
//!
//! The crate depends on `vessel-proto`, `vessel-rpc` and `vessel-client`
//! (ADR 0001) and starts `ssh` with `tokio::process` directly: a single
//! process, so no process groups and no `vessel-proc`.

pub mod install;
pub mod platform;
pub mod release;
pub mod ssh;
#[cfg(test)]
#[cfg(unix)]
mod tests;

pub use install::{
    Build, BuildError, InstallError, Installed, Installer, InvalidDigest, Sha256Digest,
};
pub use platform::{Arch, Os, Platform, PlatformError};
pub use release::{ReleaseError, Releases};
pub use ssh::{Failed, Input, RemoteCommand, Ssh, SshError, StderrTail};
