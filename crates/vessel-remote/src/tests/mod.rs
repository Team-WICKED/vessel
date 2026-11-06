//! The SSH steps against a "remote" that is a temporary home on this
//! machine. Unix only: the remote side of milestone 2 is (Windows remotes
//! are out of scope), and the stand-ins are shell scripts.
//!
//! `ssh` is played by a small stand-in script until `vessel-testkit`'s
//! `fake-ssh` is on this branch; it does what `fake-ssh` does for these
//! tests (drops the options, runs the command under `sh -c` in the fake
//! home with `HOME` set to it).

mod connector;
mod install;
mod release;
mod ssh;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use vessel_client::RemotePath;

use crate::ssh::Ssh;

/// A remote machine: an empty home directory, and the tools to reach it.
pub(crate) struct Remote {
    pub(crate) home: TempDir,
    tools: TempDir,
}

impl Remote {
    pub(crate) fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        executable(
            &tools.path().join("ssh"),
            "#!/bin/sh\n\
             while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             shift\n\
             cd \"$FAKE_SSH_HOME\" || exit 255\n\
             HOME=\"$FAKE_SSH_HOME\" exec sh -c \"$*\"\n",
        );
        Self { home, tools }
    }

    pub(crate) fn ssh(&self) -> Ssh {
        Ssh::new("gpu-box".parse().unwrap())
            .with_program(self.tools.path().join("ssh"))
            .with_env("FAKE_SSH_HOME", self.home.path())
    }

    pub(crate) fn data_dir() -> RemotePath {
        RemotePath::default_data_dir()
    }

    /// `~/.vessel/bin` on the remote, seen from here.
    pub(crate) fn bin(&self) -> PathBuf {
        self.home.path().join(".vessel").join("bin")
    }

    /// The names in `~/.vessel/bin`, sorted.
    pub(crate) fn installed(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.bin()) else {
            return Vec::new();
        };
        let mut names: Vec<_> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A directory for local files (builds, archives).
    pub(crate) fn local(&self) -> &Path {
        self.tools.path()
    }
}

/// What a fake `vesseld` does when run on the remote (`FAKE_SSH_HOME` is
/// set there, not here).
#[derive(Debug, Clone, Copy)]
pub(crate) enum OnRemote {
    Works,
    /// Fails as a `-linux-gnu` build does on an older glibc.
    MissingGlibc,
    /// Reports another version.
    OtherVersion,
}

/// This machine's target triple, as a local build would report it.
pub(crate) fn local_triple() -> String {
    let os = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else {
        "unknown-linux-gnu"
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

/// Write a fake `vesseld` at `path` reporting `version` and `triple`.
pub(crate) fn fake_vesseld(path: &Path, version: &str, triple: &str, remote: OnRemote) {
    let on_remote = match remote {
        OnRemote::Works => "",
        OnRemote::MissingGlibc => {
            "echo \"vesseld: /lib/libc.so.6: version 'GLIBC_2.99' not found\" >&2; exit 1"
        }
        OnRemote::OtherVersion => "echo \"9.9.9 $TRIPLE\"; exit 0",
    };
    let script = format!(
        "#!/bin/sh\n\
         TRIPLE='{triple}'\n\
         if [ -n \"${{FAKE_SSH_HOME:-}}\" ]; then :; {on_remote}\nfi\n\
         case \"$1\" in\n\
         version) echo '{version} {triple}' ;;\n\
         cleanup) ;;\n\
         *) exit 2 ;;\n\
         esac\n"
    );
    executable(path, &script);
}

/// Write an executable script at `path`.
///
/// Not with `std::fs::write`: while this process holds the file open for
/// writing, a test on another thread may fork, and until that child execs
/// it holds the descriptor too; running the script then fails with
/// `ETXTBSY` ("Text file busy"). So the text goes to a side file and `cp`,
/// a process of its own, creates the executable; no descriptor for it is
/// ever open in this process.
pub(crate) fn executable(path: &Path, text: &str) {
    let mut source = path.as_os_str().to_owned();
    source.push(".source");
    std::fs::write(&source, text).unwrap();
    let copied = std::process::Command::new("cp")
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(copied.success());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

pub(crate) fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}
