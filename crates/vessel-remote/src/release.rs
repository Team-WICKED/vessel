//! Fetching a release of `vesseld` locally (plan decision 1).
//!
//! [Decision Log]
//! - Purpose: install the exact release for this client on a remote,
//!   from the private `Team-WICKED/vessel` repository, without the remote
//!   needing GitHub access or a token.
//! - Constraint: Vessel builds no TLS stack (ADR 0005 amendment) and holds
//!   no GitHub credentials; secrets must not pass through it.
//! - Alternatives: an HTTP client with rustls inside `vessel` (a TLS stack,
//!   and `vessel` would have to find and send a GitHub token itself);
//!   `curl` with a token (the same token problem, and the API's asset
//!   download needs two requests and an `Accept` header per asset).
//! - Choice: the GitHub CLI, `gh release download`, which talks to the
//!   authenticated GitHub API with the login the user already has (`gh
//!   auth login`). `vessel` passes only the tag, the repository and the
//!   asset names. The archive is checked against the release's
//!   `SHA256SUMS` here, and again on the remote after the upload
//!   ([`crate::install`]).
//! - Cost: `gh` must be installed to install a release; without it the
//!   error says so and points to a local build (`--from local`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::install::{Build, InvalidDigest, Sha256Digest};
use crate::platform::Platform;

/// The repository releases come from, by default.
pub const REPOSITORY: &str = "Team-WICKED/vessel";

/// The checksum file published with every release.
pub const CHECKSUMS: &str = "SHA256SUMS";

/// Fetches release archives with `gh`.
#[derive(Debug, Clone)]
pub struct Releases {
    gh: PathBuf,
    repository: String,
}

impl Default for Releases {
    fn default() -> Self {
        Self::new(REPOSITORY)
    }
}

/// Why a release could not be fetched.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// `gh` is not installed.
    #[error(
        "installing a release needs the GitHub CLI (`gh`, then `gh auth login`); or install a \
         local build with `--from local`"
    )]
    NoGh(#[source] std::io::Error),
    /// `gh` failed (not logged in, no such release, no network).
    #[error("`gh release download {tag}` failed: {message}")]
    Download {
        /// The release tag.
        tag: String,
        /// Its last error line.
        message: String,
    },
    /// The release has no checksum for the archive.
    #[error("the release's SHA256SUMS has no entry for {asset}")]
    NoChecksum {
        /// The archive's name.
        asset: String,
    },
    /// An entry in `SHA256SUMS` is not a digest.
    #[error("the release's SHA256SUMS is malformed: {0}")]
    BadChecksum(#[from] InvalidDigest),
    /// The downloaded archive does not match its checksum.
    #[error("the downloaded {asset} does not match the release's SHA256SUMS")]
    Mismatch {
        /// The archive's name.
        asset: String,
    },
    /// A downloaded file could not be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

impl Releases {
    /// Releases of `repository` (`owner/name`), fetched with `gh` from
    /// `PATH`.
    #[must_use]
    pub fn new(repository: impl Into<String>) -> Self {
        Self {
            gh: PathBuf::from("gh"),
            repository: repository.into(),
        }
    }

    /// Run `program` instead of `gh` (tests use a stand-in).
    #[must_use]
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.gh = program.into();
        self
    }

    /// The archive's name for `version` on `platform`.
    #[must_use]
    pub fn asset(version: &str, platform: Platform) -> String {
        format!("vesseld-{version}-{}.tar.gz", platform.release_target())
    }

    /// Download release `v<version>`'s archive for `platform` and its
    /// `SHA256SUMS` into `directory`, check the archive, and describe it
    /// as a [`Build`] to install.
    ///
    /// # Errors
    /// See [`ReleaseError`].
    pub async fn fetch(
        &self,
        version: &str,
        platform: Platform,
        directory: &Path,
    ) -> Result<Build, ReleaseError> {
        let tag = format!("v{version}");
        let asset = Self::asset(version, platform);
        tracing::info!(%tag, %asset, repository = %self.repository, "downloading the release");
        let output = tokio::process::Command::new(&self.gh)
            .args(["release", "download", &tag, "--repo", &self.repository])
            .args([
                "--pattern",
                &asset,
                "--pattern",
                CHECKSUMS,
                "--clobber",
                "--dir",
            ])
            .arg(directory)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(ReleaseError::NoGh)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("no error output")
                .to_owned();
            return Err(ReleaseError::Download { tag, message });
        }
        let sums_path = directory.join(CHECKSUMS);
        let sums = tokio::fs::read_to_string(&sums_path)
            .await
            .map_err(|source| ReleaseError::Io {
                path: sums_path,
                source,
            })?;
        let expected = checksum_for(&sums, &asset)?.ok_or_else(|| ReleaseError::NoChecksum {
            asset: asset.clone(),
        })?;
        let archive = directory.join(&asset);
        let actual = Sha256Digest::of_file(&archive)
            .await
            .map_err(|source| ReleaseError::Io {
                path: archive.clone(),
                source,
            })?;
        if actual != expected {
            return Err(ReleaseError::Mismatch { asset });
        }
        Ok(Build::release(
            archive,
            version.to_owned(),
            platform.release_target().to_owned(),
            expected,
        ))
    }
}

/// The digest `sha256sum`'s output gives for `name`: lines of
/// `<hex>  <name>` (or `<hex> *<name>` in binary mode).
fn checksum_for(sums: &str, name: &str) -> Result<Option<Sha256Digest>, InvalidDigest> {
    for line in sums.lines() {
        let Some((digest, file)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        if file == name {
            return digest.parse().map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_found_by_file_name() {
        let a = Sha256Digest::of(b"a");
        let b = Sha256Digest::of(b"b");
        let sums =
            format!("{a}  vesseld-0.2.0-x86_64-unknown-linux-musl.tar.gz\n{b} *other.tar.gz\n");
        assert_eq!(
            checksum_for(&sums, "vesseld-0.2.0-x86_64-unknown-linux-musl.tar.gz").unwrap(),
            Some(a)
        );
        assert_eq!(checksum_for(&sums, "other.tar.gz").unwrap(), Some(b));
        assert_eq!(checksum_for(&sums, "missing.tar.gz").unwrap(), None);
        assert!(checksum_for("zz  x\n", "x").is_err());
    }

    #[test]
    fn asset_names_follow_the_release_workflow() {
        let platform = Platform::from_uname("Darwin arm64").unwrap();
        assert_eq!(
            Releases::asset("0.2.0", platform),
            "vesseld-0.2.0-aarch64-apple-darwin.tar.gz"
        );
    }
}
