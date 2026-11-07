//! Which `vesseld` a remote machine needs (the plan's C3).
//!
//! `uname -sm` is sent to `sh -s` on stdin rather than run as the command
//! itself: the user's login shell may be anything, and `sh -s` behaves the
//! same everywhere. Only the last non-empty line of the output is read, so
//! a banner printed by a login script does not break detection.
//!
//! Release builds exist for four targets: Linux `x86_64` and `aarch64`,
//! statically linked against musl so any distribution runs them (plan
//! decision 4), and macOS `x86_64` and `aarch64`. Anything else is
//! [`PlatformError::Unsupported`], with what `uname` said.

use std::fmt;

use crate::ssh::{Failed, Input, RemoteCommand, Ssh, SshError};

/// An operating system with `vesseld` builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    /// Linux (any distribution; the release binary is static).
    Linux,
    /// macOS.
    MacOs,
}

/// A processor architecture with `vesseld` builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    /// 64-bit x86.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
}

/// A machine's OS and architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Platform {
    /// The OS.
    pub os: Os,
    /// The architecture.
    pub arch: Arch,
}

/// Why a machine's platform is not one `vesseld` is built for.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// `uname` named an OS or architecture without a build.
    #[error("no vesseld build for this machine (`uname -sm` says `{0}`)")]
    Unsupported(String),
    /// `ssh` could not run.
    #[error(transparent)]
    Ssh(#[from] SshError),
    /// The detection command failed.
    #[error("could not detect the remote platform: {0}")]
    Failed(#[from] Failed),
}

impl Platform {
    /// The release target triple for this platform.
    #[must_use]
    pub const fn release_target(self) -> &'static str {
        match (self.os, self.arch) {
            (Os::Linux, Arch::X86_64) => "x86_64-unknown-linux-musl",
            (Os::Linux, Arch::Aarch64) => "aarch64-unknown-linux-musl",
            (Os::MacOs, Arch::X86_64) => "x86_64-apple-darwin",
            (Os::MacOs, Arch::Aarch64) => "aarch64-apple-darwin",
        }
    }

    /// Whether a binary built for `triple` is for this OS and architecture.
    ///
    /// Any C library counts: a local `-linux-gnu` development build is
    /// accepted for a Linux machine of the same architecture. Whether its
    /// glibc is new enough is only known by running it there, which the
    /// installer does before giving it its final name (ADR 0008).
    #[must_use]
    pub fn runs(self, triple: &str) -> bool {
        let mut parts = triple.split('-');
        let arch = parts.next();
        let rest: Vec<&str> = parts.collect();
        let arch_ok = match self.arch {
            Arch::X86_64 => arch == Some("x86_64"),
            Arch::Aarch64 => arch == Some("aarch64"),
        };
        let os_ok = match self.os {
            Os::Linux => rest.contains(&"linux"),
            Os::MacOs => rest.contains(&"darwin"),
        };
        arch_ok && os_ok
    }

    /// The platform named by `uname -sm` output: its last non-empty line.
    ///
    /// # Errors
    /// [`PlatformError::Unsupported`] for anything without a build.
    pub fn from_uname(output: &str) -> Result<Self, PlatformError> {
        let line = output
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        let unsupported = || PlatformError::Unsupported(line.to_owned());
        let mut words = line.split_whitespace();
        let os = match words.next() {
            Some("Linux") => Os::Linux,
            Some("Darwin") => Os::MacOs,
            _ => return Err(unsupported()),
        };
        let arch = match words.next() {
            Some("x86_64" | "amd64") => Arch::X86_64,
            Some("aarch64" | "arm64") => Arch::Aarch64,
            _ => return Err(unsupported()),
        };
        if words.next().is_some() {
            return Err(unsupported());
        }
        Ok(Self { os, arch })
    }

    /// Ask the machine behind `ssh`.
    ///
    /// # Errors
    /// `ssh` failed, or the platform has no build.
    pub async fn detect(ssh: &Ssh) -> Result<Self, PlatformError> {
        let command = RemoteCommand::program("sh").arg("-s");
        let finished = ssh
            .run(&command, Input::Bytes(b"uname -sm\n"))
            .await?
            .check()?;
        Self::from_uname(&finished.stdout_text())
    }
}

impl fmt::Display for Platform {
    /// `Linux x86_64`, `macOS aarch64`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let os = match self.os {
            Os::Linux => "Linux",
            Os::MacOs => "macOS",
        };
        let arch = match self.arch {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        };
        write!(formatter, "{os} {arch}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_output_maps_to_release_targets() {
        let cases = [
            ("Linux x86_64\n", "x86_64-unknown-linux-musl"),
            ("Linux aarch64", "aarch64-unknown-linux-musl"),
            ("Darwin arm64\n", "aarch64-apple-darwin"),
            ("Darwin x86_64", "x86_64-apple-darwin"),
            (
                "Welcome to gpu-box!\nLast login: yesterday\n\nLinux x86_64\n\n",
                "x86_64-unknown-linux-musl",
            ),
        ];
        for (output, target) in cases {
            let platform = Platform::from_uname(output).unwrap();
            assert_eq!(platform.release_target(), target, "{output:?}");
        }
    }

    #[test]
    fn other_machines_are_unsupported_with_what_uname_said() {
        for output in [
            "Linux armv7l",
            "FreeBSD amd64",
            "MINGW64_NT-10.0 x86_64",
            "",
            "Linux",
        ] {
            let error = Platform::from_uname(output).unwrap_err();
            let PlatformError::Unsupported(said) = &error else {
                panic!("{error:?}");
            };
            assert_eq!(said, output.trim(), "{output:?}");
        }
    }

    #[test]
    fn local_builds_match_by_os_and_architecture() {
        let linux = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        assert!(linux.runs("x86_64-unknown-linux-musl"));
        assert!(linux.runs("x86_64-unknown-linux-gnu"));
        assert!(!linux.runs("aarch64-unknown-linux-gnu"));
        assert!(!linux.runs("x86_64-apple-darwin"));
        assert!(!linux.runs("x86_64-pc-windows-msvc"));
        let mac = Platform {
            os: Os::MacOs,
            arch: Arch::Aarch64,
        };
        assert!(mac.runs("aarch64-apple-darwin"));
        assert!(!mac.runs("aarch64-unknown-linux-musl"));
    }
}
