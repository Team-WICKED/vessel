//! Reaching Vessel nodes over SSH (ADRs 0007 and 0008).
//!
//! - [`ssh`]: runs the system `ssh` with fixed options, builds remote
//!   commands from fixed words and quoted values, keeps the end of stderr
//!   for errors.
//! - [`platform`]: which `vesseld` build a machine needs.
//! - [`build`] and [`digest`]: what is installed (a local build or a
//!   release archive) and its SHA-256.
//! - [`install`]: uploading a local build or a release archive, checked on
//!   the remote, run there once, then renamed into place; `cleanup`.
//! - [`release`]: fetching a release archive locally with `gh` and
//!   checking it against the release's `SHA256SUMS`.
//! - [`provision`]: which build a machine gets (a local one, or the
//!   release), shared by `vessel node install` and the connector.
//! - [`connector`]: [`SshConnector`], a `vessel_client::Connector` over
//!   `ssh … vesseld attach`.
//!
//! The crate depends on `vessel-proto`, `vessel-rpc` and `vessel-client`
//! (ADR 0001) and starts `ssh` with `tokio::process` directly: a single
//! process, so no process groups and no `vessel-proc`.

pub mod build;
pub mod connector;
pub mod digest;
pub mod install;
pub mod platform;
pub mod provision;
pub mod release;
pub mod ssh;
#[cfg(test)]
#[cfg(unix)]
mod tests;

pub use build::{Build, BuildError};
pub use connector::{SshConnectError, SshConnector};
pub use digest::{InvalidDigest, Sha256Digest};
pub use install::{InstallError, Installed, Installer};
pub use platform::{Arch, Os, Platform, PlatformError};
pub use provision::{ProvisionError, Provisioner, Source};
pub use release::{ReleaseError, Releases};
pub use ssh::{Failed, Input, RemoteCommand, Ssh, SshError, StderrTail};
