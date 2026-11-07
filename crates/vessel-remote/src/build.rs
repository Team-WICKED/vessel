//! [`Build`]: a `vesseld` to install (a local build or a release
//! archive), its version, target and digest, and the name it is installed
//! under (ADR 0008).

use std::path::{Path, PathBuf};

use crate::digest::Sha256Digest;
use crate::platform::Platform;

/// What is uploaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A `vesseld` binary built here.
    Local,
    /// A release archive (`vesseld-<version>-<target>.tar.gz`) holding
    /// `vesseld`.
    Release,
}

/// A `vesseld` to install: a local build or a release archive, with its
/// version, target and digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub(crate) kind: Kind,
    pub(crate) file: PathBuf,
    pub(crate) version: String,
    pub(crate) target: String,
    pub(crate) sha256: Sha256Digest,
}

/// Why a local build cannot be installed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// It could not be run or read.
    #[error("cannot use {} as a vesseld build", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// `version` failed or printed something else than `<version> <target>`.
    #[error("{} is not a vesseld build: `version` says `{said}`", path.display())]
    NotVesseld {
        /// The file.
        path: PathBuf,
        /// What it printed (or its error output).
        said: String,
    },
}

impl Build {
    /// A local `vesseld` build: its `version` line gives the version and
    /// target, its content the digest. Installed as
    /// `vesseld-<version>-dev-<16 hex>`.
    ///
    /// # Errors
    /// The file does not run, is not `vesseld`, or cannot be read.
    pub async fn local(path: &Path) -> Result<Self, BuildError> {
        let io = |source| BuildError::Io {
            path: path.to_owned(),
            source,
        };
        let output = tokio::process::Command::new(path)
            .arg("version")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(io)?;
        let said = String::from_utf8_lossy(if output.status.success() {
            &output.stdout
        } else {
            &output.stderr
        })
        .trim()
        .to_owned();
        let parsed = output
            .status
            .success()
            .then(|| parse_version_line(&said))
            .flatten();
        let Some((version, target)) = parsed else {
            return Err(BuildError::NotVesseld {
                path: path.to_owned(),
                said,
            });
        };
        let sha256 = Sha256Digest::of_file(path).await.map_err(io)?;
        Ok(Self {
            kind: Kind::Local,
            file: path.to_owned(),
            version,
            target,
            sha256,
        })
    }

    /// A release archive for `target`, whose digest `sha256` comes from
    /// the release's `SHA256SUMS`. Installed as `vesseld-<version>`.
    #[must_use]
    pub fn release(
        archive: PathBuf,
        version: String,
        target: String,
        sha256: Sha256Digest,
    ) -> Self {
        Self {
            kind: Kind::Release,
            file: archive,
            version,
            target,
            sha256,
        }
    }

    /// The installed file's name: `vesseld-<version>` for a release,
    /// `vesseld-<version>-dev-<first 16 hex of SHA-256>` for a local build.
    #[must_use]
    pub fn binary_name(&self) -> String {
        match self.kind {
            Kind::Release => format!("vesseld-{}", self.version),
            Kind::Local => {
                let hex = self.sha256.hex();
                let short = hex.get(..16).unwrap_or(&hex);
                format!("vesseld-{}-dev-{short}", self.version)
            }
        }
    }

    /// The version it reports.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The target triple it was built for.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Whether it is a local build (a `-dev-` name).
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.kind == Kind::Local
    }

    /// Whether it runs on `platform`: a release must be built for exactly
    /// its release target; a local build for its OS and architecture.
    #[must_use]
    pub fn runs_on(&self, platform: Platform) -> bool {
        match self.kind {
            Kind::Release => self.target == platform.release_target(),
            Kind::Local => platform.runs(&self.target),
        }
    }
}

/// `<version> <target>`, the line `vesseld version` prints.
pub(crate) fn parse_version_line(line: &str) -> Option<(String, String)> {
    let mut words = line.split_whitespace();
    let version = words.next()?;
    let target = words.next()?;
    if words.next().is_some() || target.split('-').count() < 3 {
        return None;
    }
    Some((version.to_owned(), target.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lines_have_a_version_and_a_triple() {
        assert_eq!(
            parse_version_line("0.2.0 x86_64-unknown-linux-musl"),
            Some(("0.2.0".to_owned(), "x86_64-unknown-linux-musl".to_owned()))
        );
        assert_eq!(parse_version_line("vesseld 0.1.0"), None);
        assert_eq!(parse_version_line("0.2.0"), None);
        assert_eq!(parse_version_line("0.2.0 a-b-c extra"), None);
    }

    #[test]
    fn local_builds_are_named_by_content_and_releases_by_version() {
        let digest = Sha256Digest::of(b"vesseld");
        let local = Build {
            kind: Kind::Local,
            file: PathBuf::from("/x/vesseld"),
            version: "0.2.0".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            sha256: digest,
        };
        let short = digest.hex().get(..16).unwrap().to_owned();
        assert_eq!(local.binary_name(), format!("vesseld-0.2.0-dev-{short}"));
        let release = Build::release(
            PathBuf::from("/x/a.tar.gz"),
            "0.2.0".to_owned(),
            "x86_64-unknown-linux-musl".to_owned(),
            digest,
        );
        assert_eq!(release.binary_name(), "vesseld-0.2.0");
        let linux: Platform = Platform::from_uname("Linux x86_64").unwrap();
        assert!(
            local.runs_on(linux),
            "gnu builds run on any Linux, if glibc allows"
        );
        assert!(release.runs_on(linux));
        let mac = Platform::from_uname("Darwin arm64").unwrap();
        assert!(!local.runs_on(mac));
        assert!(!release.runs_on(mac));
    }
}
