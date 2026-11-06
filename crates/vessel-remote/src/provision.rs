//! Which `vesseld` a remote gets, and getting it there: shared by `vessel
//! node install` and by [`SshConnector`](crate::SshConnector) when it finds
//! nothing installed (exit 127: install, then retry once).
//!
//! - [`Source::Local`]: a build on this machine (a development checkout's
//!   `target/<profile>/vesseld`), installed as
//!   `vesseld-<version>-dev-<hash>`.
//! - [`Source::Release`]: this client's version from GitHub Releases
//!   ([`Releases`]), installed as `vesseld-<version>`.
//! - [`Source::Auto`]: the local build if there is one and it is for that
//!   machine, else the release. Before the first release, a development
//!   checkout reaches a fresh machine without flags.

use std::path::PathBuf;

use vessel_client::RemotePath;

use crate::build::{Build, BuildError};
use crate::install::{InstallError, Installed, Installer};
use crate::platform::{Platform, PlatformError};
use crate::release::{ReleaseError, Releases};
use crate::ssh::Ssh;

/// Where an installed `vesseld` comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// This local build.
    Local(PathBuf),
    /// The release of the provisioner's version.
    Release(Releases),
    /// `local` if it exists and runs on the machine, else the release.
    Auto {
        /// A local build, if there may be one.
        local: Option<PathBuf>,
        /// Where releases come from.
        releases: Releases,
    },
}

/// Why provisioning failed.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The remote platform could not be detected or has no build.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// The local build cannot be used.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// The release could not be fetched.
    #[error(transparent)]
    Release(#[from] ReleaseError),
    /// Installing failed.
    #[error(transparent)]
    Install(#[from] InstallError),
    /// No scratch directory for the download.
    #[error("cannot create a directory for the download")]
    Scratch(#[source] std::io::Error),
}

/// Installs this client's `vesseld` on remote machines.
#[derive(Debug, Clone)]
pub struct Provisioner {
    source: Source,
    version: String,
}

impl Provisioner {
    /// Install from `source`; `version` is the release version to install
    /// (the client's own).
    #[must_use]
    pub fn new(source: Source, version: impl Into<String>) -> Self {
        Self {
            source,
            version: version.into(),
        }
    }

    /// The name of the binary this would install, as far as it is known
    /// without asking the remote: a local build's content name, else the
    /// release name. (`Auto` may still install the release, if the local
    /// build turns out to be for another platform.)
    pub async fn expected_binary(&self) -> String {
        let local = match &self.source {
            Source::Local(path)
            | Source::Auto {
                local: Some(path), ..
            } => Build::local(path).await.ok(),
            Source::Release(_) | Source::Auto { local: None, .. } => None,
        };
        local.map_or_else(|| self.release_name(), |build| build.binary_name())
    }

    /// `vesseld-<version>`.
    #[must_use]
    pub fn release_name(&self) -> String {
        format!("vesseld-{}", self.version)
    }

    /// Choose the build for the machine behind `ssh` and install it into
    /// `data_dir`.
    ///
    /// # Errors
    /// See [`ProvisionError`].
    pub async fn install(
        &self,
        ssh: &Ssh,
        data_dir: &RemotePath,
    ) -> Result<Installed, ProvisionError> {
        let platform = Platform::detect(ssh).await?;
        let downloads = tempfile::tempdir().map_err(ProvisionError::Scratch)?;
        let build = self.choose(platform, downloads.path()).await?;
        Ok(Installer::new(ssh, data_dir)
            .install_on(platform, &build)
            .await?)
    }

    async fn choose(
        &self,
        platform: Platform,
        downloads: &std::path::Path,
    ) -> Result<Build, ProvisionError> {
        match &self.source {
            Source::Local(path) => Ok(Build::local(path).await?),
            Source::Release(releases) => {
                Ok(releases.fetch(&self.version, platform, downloads).await?)
            }
            Source::Auto { local, releases } => {
                if let Some(path) = local
                    && path.is_file()
                {
                    match Build::local(path).await {
                        Ok(build) if build.runs_on(platform) => return Ok(build),
                        Ok(build) => tracing::debug!(
                            target = build.target(),
                            %platform,
                            "the local vesseld is for another platform; using the release"
                        ),
                        Err(error) => tracing::debug!(%error, "no usable local vesseld"),
                    }
                }
                Ok(releases.fetch(&self.version, platform, downloads).await?)
            }
        }
    }
}
