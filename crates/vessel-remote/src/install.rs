//! Installing `vesseld` on a remote machine by upload (ADR 0008, plan
//! decision 1, the plan's C4). What is installed is a [`Build`]
//! (`crate::build`), checked by its [`Sha256Digest`](crate::Sha256Digest).
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

use std::hash::{BuildHasher as _, RandomState};

use vessel_client::{InvalidRemotePath, RemotePath};

use crate::build::{Build, Kind, parse_version_line};
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
        self.install_on(platform, build).await
    }

    /// [`install`](Self::install) on a machine whose platform is already
    /// known (detected by the caller, to choose the build).
    ///
    /// # Errors
    /// As [`install`](Self::install).
    pub async fn install_on(
        &self,
        platform: Platform,
        build: &Build,
    ) -> Result<Installed, InstallError> {
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
