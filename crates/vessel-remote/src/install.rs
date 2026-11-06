//! Installing `vesseld` on a remote machine by upload (ADR 0008, plan
//! decision 1, the plan's C4).
//!
//! [Decision Log]
//! - Purpose: `ssh` to a fresh machine and get a working node, with the
//!   exact `vesseld` for this client, side by side with any other version
//!   and never half-installed.
//! - Constraint (plan decision 1): no download on the remote in milestone
//!   2. The release is private, so the client fetches it locally (see
//!   [`crate::release`]) or uses a local build, and sends the bytes through
//!   `ssh` stdin.
//! - Choice: one remote command per install, a fixed `sh -c` script with
//!   the paths and the expected digest as quoted arguments. It writes a
//!   temporary file in `bin/` (`.vesseld-<version>.<random>.tmp`, the name
//!   `vesseld cleanup` recognises as a leftover), checks its SHA-256 with
//!   `sha256sum` or `shasum -a 256`, unpacks a release archive with
//!   `tar -xzO`, runs the result's `version`, and only then renames it
//!   onto the final name, in the same directory (atomic). A trap removes
//!   the temporary files on every way out.
//! - Running before renaming: a local `-linux-gnu` build matches a Linux
//!   machine by OS and architecture, but needs a glibc at least as new as
//!   the one it was linked against. It is run where it will live before it
//!   gets a name the client would run; one that does not start is deleted
//!   and the error suggests a musl build or a release (coordinator,
//!   2026-11-06). No unrunnable binary ever has a final name.
//! - Alternatives: `scp`/`sftp` (another protocol that may be disabled,
//!   and still a separate rename step); several commands (upload, check,
//!   rename: more logins without `ControlMaster`, and a window in which a
//!   failed step leaves files behind).
//! - The script is one line with no newlines, no `!` and no `\\`: it is
//!   passed through the user's login shell first, and `csh` refuses
//!   newlines inside quotes while `fish` reads `\\` inside them.

use std::fmt;
use std::hash::{BuildHasher as _, RandomState};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;
use vessel_client::{InvalidRemotePath, RemotePath};

use crate::platform::{Platform, PlatformError};
use crate::ssh::{Failed, Input, RemoteCommand, Ssh, SshError};

/// The installer script's own exit codes (distinct from the shell's 126
/// and 127 and from `ssh`'s 255).
mod exit {
    pub(super) const NO_CHECKSUM_TOOL: i32 = 90;
    pub(super) const CHECKSUM_MISMATCH: i32 = 91;
    pub(super) const DOES_NOT_RUN: i32 = 92;
    pub(super) const WRONG_VERSION: i32 = 93;
}

/// The install script. Arguments: `$1` the bin directory, `$2` the
/// temporary upload, `$3` the final name, `$4` the expected SHA-256 (hex),
/// `$5` `raw` or `tgz`, `$6` the temporary unpacked binary, `$7` the
/// expected version. Prints the binary's `version` line.
const SCRIPT: &str = concat!(
    "set -eu; umask 077; mkdir -p \"$1\"; ",
    "trap 'rm -f \"$2\" \"$6\"' EXIT; ",
    "cat > \"$2\"; ",
    "if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum < \"$2\"); ",
    "elif command -v shasum >/dev/null 2>&1; then sum=$(shasum -a 256 < \"$2\"); ",
    "else echo 'vessel: the remote has neither sha256sum nor shasum' >&2; exit 90; fi; ",
    "sum=${sum%% *}; ",
    "if [ \"$sum\" = \"$4\" ]; then :; ",
    "else echo \"vessel: checksum mismatch: the upload hashes to $sum\" >&2; exit 91; fi; ",
    "if [ \"$5\" = tgz ]; then tar -xzOf \"$2\" vesseld > \"$6\"; bin=$6; else bin=$2; fi; ",
    "chmod 755 \"$bin\"; ",
    "if out=$(\"$bin\" version 2>&1); then :; ",
    "else printf 'vessel: the uploaded vesseld does not run here: %s' \"$out\" >&2; exit 92; fi; ",
    "case \"$out\" in \"$7 \"*) ;; ",
    "*) printf 'vessel: the uploaded vesseld reports %s' \"$out\" >&2; exit 93;; esac; ",
    "mv -f \"$bin\" \"$3\"; ",
    "printf '%s' \"$out\"",
);

/// A SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The digest of the file at `path`.
    ///
    /// # Errors
    /// The file cannot be read.
    pub async fn of_file(path: &Path) -> std::io::Result<Self> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            if let Some(chunk) = buffer.get(..read) {
                hasher.update(chunk);
            }
        }
        Ok(Self(hasher.finalize().into()))
    }

    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Lowercase hex, as `sha256sum` prints it.
    #[must_use]
    pub fn hex(&self) -> String {
        use fmt::Write as _;

        self.0
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                let _infallible = write!(text, "{byte:02x}");
                text
            })
    }
}

impl std::str::FromStr for Sha256Digest {
    type Err = InvalidDigest;

    /// 64 hex digits, either case.
    fn from_str(text: &str) -> Result<Self, InvalidDigest> {
        let invalid = || InvalidDigest(text.to_owned());
        if text.len() != 64 || !text.is_ascii() {
            return Err(invalid());
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let start = index.checked_mul(2).ok_or_else(invalid)?;
            let pair = text
                .get(start..start.saturating_add(2))
                .ok_or_else(invalid)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_digit| invalid())?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Sha256Digest({})", self.hex())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

/// Text that is not a SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a SHA-256 digest (64 hex digits)")]
pub struct InvalidDigest(String);

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
    file: PathBuf,
    version: String,
    target: String,
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
fn parse_version_line(line: &str) -> Option<(String, String)> {
    let mut words = line.split_whitespace();
    let version = words.next()?;
    let target = words.next()?;
    if words.next().is_some() || target.split('-').count() < 3 {
        return None;
    }
    Some((version.to_owned(), target.to_owned()))
}

/// A finished install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The binary, on the remote.
    pub binary: RemotePath,
    /// What it reports (`<version> <target>`).
    pub version_line: String,
    /// Whether it was uploaded now (`false`: it was already there).
    pub uploaded: bool,
    /// What `cleanup` deleted.
    pub removed: Vec<String>,
}

/// Why an install failed.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The machine's platform is unknown or unsupported.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// The build is for another platform.
    #[error("the vesseld build is for {build}, but this machine is {platform}")]
    WrongTarget {
        /// The build's target.
        build: String,
        /// The machine.
        platform: Platform,
    },
    /// `ssh` could not run.
    #[error(transparent)]
    Ssh(#[from] SshError),
    /// The remote has no SHA-256 tool.
    #[error("the remote has neither `sha256sum` nor `shasum`; install coreutils or perl")]
    NoChecksumTool,
    /// The uploaded bytes hash differently from the expected digest.
    #[error("the upload arrived corrupted (its SHA-256 differs); nothing was installed")]
    ChecksumMismatch,
    /// The uploaded binary does not start on the remote (for example a
    /// glibc too old for a `-linux-gnu` build).
    #[error("the uploaded vesseld does not run on the remote: {output}{}", hint.unwrap_or_default())]
    DoesNotRun {
        /// Its error output.
        output: String,
        /// What to do instead.
        hint: Option<&'static str>,
    },
    /// The uploaded binary reports another version.
    #[error("the uploaded vesseld reports `{reported}`, not version {expected}")]
    WrongVersion {
        /// The expected version.
        expected: String,
        /// What it said.
        reported: String,
    },
    /// Any other remote failure.
    #[error(transparent)]
    Remote(#[from] Failed),
    /// A path could not be built (a bug: the names are fixed).
    #[error(transparent)]
    Path(#[from] InvalidRemotePath),
}

/// The hint for a local build that does not run on a Linux remote.
const MUSL_HINT: &str = "; a `-linux-gnu` build needs the remote's glibc to be at least as new \
    as the build machine's. Build vesseld for `x86_64-unknown-linux-musl` (or \
    `aarch64-unknown-linux-musl`) with `cargo build --release --target ...`, or install a \
    release";

/// Installs into one remote data directory over `ssh`.
#[derive(Debug, Clone, Copy)]
pub struct Installer<'a> {
    ssh: &'a Ssh,
    data_dir: &'a RemotePath,
}

impl<'a> Installer<'a> {
    /// Install into `data_dir` on the machine behind `ssh`.
    #[must_use]
    pub const fn new(ssh: &'a Ssh, data_dir: &'a RemotePath) -> Self {
        Self { ssh, data_dir }
    }

    /// `<data_dir>/bin`.
    ///
    /// # Errors
    /// Never for the fixed names used here.
    pub fn bin_dir(&self) -> Result<RemotePath, InvalidRemotePath> {
        self.data_dir.join("bin")
    }

    /// `<data_dir>/bin/<name>`.
    ///
    /// # Errors
    /// `name` is not a plain file name.
    pub fn binary(&self, name: &str) -> Result<RemotePath, InvalidRemotePath> {
        self.bin_dir()?.join(name)
    }

    /// What the installed `name` reports (`<version> <target>`), or `None`
    /// if it is missing or does not run.
    ///
    /// # Errors
    /// `ssh` itself failed.
    pub async fn installed(&self, name: &str) -> Result<Option<String>, InstallError> {
        let binary = self.binary(name)?;
        let command = RemoteCommand::path_program(&binary).arg("version");
        let finished = self.ssh.run(&command, Input::Empty).await?;
        match finished.code {
            Some(0) => Ok(Some(finished.stdout_text().trim().to_owned())),
            Some(crate::ssh::SSH_FAILED) => Err(InstallError::Remote(Failed {
                code: finished.code,
                stderr: finished.stderr,
            })),
            _ => Ok(None),
        }
    }

    /// Install `build` unless the same file is already there, then run
    /// `cleanup` with it.
    ///
    /// # Errors
    /// See [`InstallError`]. A failed install leaves no file behind and
    /// does not touch what was installed before.
    pub async fn install(&self, build: &Build) -> Result<Installed, InstallError> {
        let platform = Platform::detect(self.ssh).await?;
        if !build.runs_on(platform) {
            return Err(InstallError::WrongTarget {
                build: build.target.clone(),
                platform,
            });
        }
        let name = build.binary_name();
        let binary = self.binary(&name)?;
        let present = self.installed(&name).await?.filter(|line| {
            parse_version_line(line).is_some_and(|(version, _)| version == build.version)
        });
        let (version_line, uploaded) = match present {
            Some(line) => (line, false),
            None => (self.upload(build, &name).await?, true),
        };
        let removed = self.cleanup(&binary).await?;
        Ok(Installed {
            binary,
            version_line,
            uploaded,
            removed,
        })
    }

    async fn upload(&self, build: &Build, name: &str) -> Result<String, InstallError> {
        let nonce = format!("{:016x}", RandomState::new().hash_one(name));
        let temporary = self.binary(&format!(".{name}.{nonce}.tmp"))?;
        let unpacked = self.binary(&format!(".{name}.{nonce}.bin.tmp"))?;
        let kind = match build.kind {
            Kind::Local => "raw",
            Kind::Release => "tgz",
        };
        let command = RemoteCommand::program("sh")
            .arg("-c")
            .value(SCRIPT)
            .arg("vessel-install")
            .path(&self.bin_dir()?)
            .path(&temporary)
            .path(&self.binary(name)?)
            .value(&build.sha256.hex())
            .arg(kind)
            .path(&unpacked)
            .value(&build.version);
        tracing::info!(host = %self.ssh.target().host, binary = name, "uploading vesseld");
        let finished = self.ssh.run(&command, Input::File(&build.file)).await?;
        let output = || finished.stderr.last().unwrap_or_default().to_owned();
        match finished.code {
            Some(0) => Ok(finished.stdout_text().trim().to_owned()),
            Some(exit::NO_CHECKSUM_TOOL) => Err(InstallError::NoChecksumTool),
            Some(exit::CHECKSUM_MISMATCH) => Err(InstallError::ChecksumMismatch),
            Some(exit::DOES_NOT_RUN) => Err(InstallError::DoesNotRun {
                output: output(),
                hint: build.target.contains("-linux-gnu").then_some(MUSL_HINT),
            }),
            Some(exit::WRONG_VERSION) => Err(InstallError::WrongVersion {
                expected: build.version.clone(),
                reported: output(),
            }),
            _ => Err(InstallError::Remote(Failed {
                code: finished.code,
                stderr: finished.stderr,
            })),
        }
    }

    /// `<binary> cleanup`: delete installed binaries nothing needs (ADR
    /// 0008); returns what it deleted.
    ///
    /// # Errors
    /// `ssh` failed or cleanup reported an error.
    pub async fn cleanup(&self, binary: &RemotePath) -> Result<Vec<String>, InstallError> {
        let command = RemoteCommand::path_program(binary)
            .arg("cleanup")
            .arg("--data-dir")
            .path(self.data_dir);
        let finished = self.ssh.run(&command, Input::Empty).await?.check()?;
        Ok(finished
            .stdout_text()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_print_and_parse_as_sha256sum_does() {
        let digest = Sha256Digest::of(b"abc");
        assert_eq!(
            digest.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.hex().parse::<Sha256Digest>().unwrap(), digest);
        assert_eq!(
            digest.hex().to_uppercase().parse::<Sha256Digest>().unwrap(),
            digest
        );
        for bad in ["", "abc", &"g".repeat(64), &"é".repeat(32)] {
            assert!(bad.parse::<Sha256Digest>().is_err(), "{bad}");
        }
    }

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

    /// The script survives the login shells `sshd` may hand it to.
    #[test]
    fn the_script_is_one_line_without_shell_traps() {
        assert!(!SCRIPT.contains('\n'));
        assert!(!SCRIPT.contains('!'));
        assert!(!SCRIPT.contains("\\\\"));
        for code in [
            exit::NO_CHECKSUM_TOOL,
            exit::CHECKSUM_MISMATCH,
            exit::DOES_NOT_RUN,
            exit::WRONG_VERSION,
        ] {
            assert!(SCRIPT.contains(&format!("exit {code}")), "{code}");
        }
    }
}
