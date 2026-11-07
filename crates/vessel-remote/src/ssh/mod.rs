//! Running commands on a remote machine with the system `ssh` (ADR 0007).
//!
//! [Decision Log]
//! - Purpose: every remote step (platform detection, upload, `version`,
//!   `cleanup`, `attach`) is one `ssh` command. They must all run with the
//!   same options, quote the same way, and report failures the same way.
//! - Constraint: the remote command is a line of shell text that `sshd`
//!   hands to the user's login shell, which may be `sh`, `bash`, `zsh`,
//!   `fish` or `csh`. User-supplied text must never become shell syntax.
//! - Choice: a [`RemoteCommand`] is built only from fixed words (string
//!   literals in this crate) and quoted values. A value is single-quoted,
//!   with `'` written as `'\''`, which every one of those shells reads
//!   back as the original text. A home-relative path becomes `"$HOME"/'…'`
//!   (single quotes would stop `~` from expanding). Commands stay on one
//!   line: `csh` refuses a newline inside quotes.
//! - Options: `-T` (no terminal: the stream is binary frames),
//!   `BatchMode=yes` (fail instead of prompting for a password: there is
//!   no terminal to prompt on), `ServerAliveInterval=15` with
//!   `ServerAliveCountMax=3` (a dead link is noticed in about 45 s; the
//!   protocol has no ping), and `ConnectTimeout=15` (a black-holed host
//!   fails in seconds, not after the OS's two-minute TCP timeout). The
//!   user's `~/.ssh/config` still applies to everything else.
//! - Connection sharing: on Unix, `ControlMaster=auto` with a
//!   `ControlPath` in a 0700 directory the caller chooses and
//!   `ControlPersist=60`, so the several commands of an install share one
//!   login (one key touch, one 2FA prompt in `ssh-agent`). Windows OpenSSH
//!   has no `ControlMaster`; each command logs in again there.
//! - Errors: `ssh` and the remote command share one stderr. The last 64
//!   lines are kept ([`StderrTail`]) for the error message; `ssh`'s own
//!   failures exit 255.
//!
//! Layout: [`RemoteCommand`] (the remote shell line) is in `command`; what
//! a run produces and how it fails ([`Finished`], [`StderrTail`],
//! [`Failed`], [`SshError`]) in `output`; the child's pipes in `pipes`;
//! running `ssh` here.

mod command;
mod output;
mod pipes;

use std::ffi::OsString;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::{Child, Command};
use vessel_client::SshTarget;

pub use command::RemoteCommand;
pub use output::{Failed, Finished, Input, SSH_FAILED, STDERR_LINES, SshError, StderrTail};
pub use pipes::{Running, StderrHandle};

use pipes::{collect_stderr, feed, read_limited};

/// Most bytes of a command's stdout kept by [`Ssh::run`]; the commands it
/// runs print a line or a short list.
const STDOUT_LIMIT: usize = 1024 * 1024;

/// How long stderr is still read after `ssh` exited.
const STDERR_GRACE: Duration = Duration::from_secs(1);

/// Runs `ssh` to one machine.
#[derive(Debug, Clone)]
pub struct Ssh {
    program: PathBuf,
    target: SshTarget,
    control_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}

impl Ssh {
    /// `ssh` (found on `PATH`) to `target`, without connection sharing.
    #[must_use]
    pub fn new(target: SshTarget) -> Self {
        Self {
            program: PathBuf::from("ssh"),
            target,
            control_dir: None,
            env: Vec::new(),
        }
    }

    /// Run `program` instead of `ssh` (tests use `fake-ssh`).
    #[must_use]
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Share one login between commands through a control socket in
    /// `dir` (Unix; ignored on Windows). The directory is created 0700 if
    /// needed, and must be short: a Unix socket path has room for about 104
    /// bytes, and the socket's name is 40 more (`%C`). A path too long is
    /// skipped and every command logs in again.
    #[must_use]
    pub fn with_control_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.control_dir = Some(dir.into());
        self
    }

    /// Set an environment variable for `ssh` (tests point `fake-ssh` at
    /// its fake remote home this way).
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The machine.
    #[must_use]
    pub const fn target(&self) -> &SshTarget {
        &self.target
    }

    /// The `ssh` command line for `remote`, ready to spawn: stdin, stdout
    /// and stderr piped, killed if dropped.
    #[must_use]
    pub fn command(&self, remote: &RemoteCommand) -> Command {
        let mut command = Command::new(&self.program);
        let _options = command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ConnectTimeout=15",
        ]);
        if let Some(path) = self.control_path() {
            let mut control_path = OsString::from("ControlPath=");
            control_path.push(path);
            let _sharing = command
                .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=60", "-o"])
                .arg(control_path);
        }
        if let Some(port) = self.target.port {
            let _port = command.arg("-p").arg(port.to_string());
        }
        if let Some(user) = &self.target.user {
            let _user = command.arg("-l").arg(user);
        }
        // The host was checked not to start with `-` (`SshTarget`), so it
        // cannot be read as an option; `--` then ends the options before
        // the remote command.
        let _destination = command
            .arg(&self.target.host)
            .arg("--")
            .arg(remote.as_str())
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// `<dir>/%C`, if connection sharing is on, possible here, and the
    /// path is short enough.
    #[cfg_attr(
        not(unix),
        expect(clippy::unused_self, reason = "no sharing on Windows")
    )]
    fn control_path(&self) -> Option<PathBuf> {
        #[cfg(unix)]
        {
            /// `sun_path` is 104 bytes on macOS (108 on Linux), and `%C`
            /// expands to 40 hex digits.
            const ROOM: usize = 104 - 41;
            let dir = self.control_dir.as_ref()?;
            if dir.as_os_str().len() > ROOM {
                tracing::debug!(dir = %dir.display(), "control directory path too long; not sharing logins");
                return None;
            }
            // `ssh` splits an option's value at whitespace and quotes, and
            // expands `%` tokens in `ControlPath`; a path with any of them
            // would name some other place.
            let plain = dir.to_str().is_some_and(|text| {
                !text.contains(|c: char| c.is_whitespace() || matches!(c, '%' | '"' | '\''))
            });
            if !plain {
                tracing::debug!(dir = %dir.display(), "control directory path not plain; not sharing logins");
                return None;
            }
            if let Err(error) = private_dir(dir) {
                tracing::debug!(dir = %dir.display(), %error, "no control directory; not sharing logins");
                return None;
            }
            Some(dir.join("%C"))
        }
        #[cfg(not(unix))]
        None
    }

    /// Run `remote` to the end, feeding it `input`.
    ///
    /// # Errors
    /// `ssh` could not be started, or its output not read. A command that
    /// ran and failed is not an error here: see [`Finished::check`].
    pub async fn run(
        &self,
        remote: &RemoteCommand,
        input: Input<'_>,
    ) -> Result<Finished, SshError> {
        tracing::debug!(host = %self.target.host, command = %remote, "ssh");
        let mut child = self.spawn_command(remote)?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(SshError::Io(std::io::Error::other(
                "ssh's stdio is not piped",
            )));
        };
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        let mut collecting = tokio::spawn(collect_stderr(stderr, Arc::clone(&tail)));
        let (fed, stdout, status) =
            tokio::join!(feed(stdin, input), read_limited(stdout), child.wait());
        // `ssh` has exited. Its stderr normally closes with it, but a
        // `ControlPersist` master started by this command may hold the
        // pipe for as long as it lingers (some OpenSSH versions do); what
        // arrived until then is enough for an error message.
        if tokio::time::timeout(STDERR_GRACE, &mut collecting)
            .await
            .is_err()
        {
            collecting.abort();
        }
        let status = status.map_err(SshError::Io)?;
        let stdout = stdout.map_err(SshError::Io)?;
        fed?;
        let stderr = tail.lock().map(|tail| tail.clone()).unwrap_or_default();
        Ok(Finished {
            code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Start `remote` with its stdio piped and stderr collected into a
    /// [`StderrTail`], for a long-running command (`attach`).
    ///
    /// # Errors
    /// `ssh` could not be started.
    pub fn spawn(&self, remote: &RemoteCommand) -> Result<Running, SshError> {
        tracing::debug!(host = %self.target.host, command = %remote, "ssh");
        let mut child = self.spawn_command(remote)?;
        let stderr = Arc::new(Mutex::new(StderrTail::default()));
        if let Some(pipe) = child.stderr.take() {
            drop(tokio::spawn(collect_stderr(pipe, Arc::clone(&stderr))));
        }
        Ok(Running { child, stderr })
    }

    fn spawn_command(&self, remote: &RemoteCommand) -> Result<Child, SshError> {
        self.command(remote)
            .spawn()
            .map_err(|source| SshError::Spawn {
                program: self.program.clone(),
                source,
            })
    }
}

/// Make `dir` exist with mode 0700 (only this user can reach the control
/// socket, which is a logged-in session).
#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::metadata(dir)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_has_the_options_then_the_host_then_the_command() {
        let mut target: SshTarget = "ssh://me@box:2222".parse().unwrap();
        target.data_dir = None;
        let ssh = Ssh::new(target);
        let command = ssh.command(&RemoteCommand::program("uname").arg("-sm"));
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=15",
                "-p",
                "2222",
                "-l",
                "me",
                "box",
                "--",
                "uname -sm",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn logins_are_shared_through_a_private_control_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("ssh");
        let ssh = Ssh::new("box".parse().unwrap()).with_control_dir(&control);
        let command = ssh.command(&RemoteCommand::program("true"));
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let expected = format!("ControlPath={}/%C", control.display());
        assert!(args.contains(&"ControlMaster=auto".to_owned()), "{args:?}");
        assert!(args.contains(&expected), "{args:?}");
        let mode = std::fs::metadata(&control).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        for odd in ["with space", "per%cent", "quo\"te"] {
            let ssh = Ssh::new("box".parse().unwrap()).with_control_dir(dir.path().join(odd));
            let command = ssh.command(&RemoteCommand::program("true"));
            assert!(
                !command
                    .as_std()
                    .get_args()
                    .any(|arg| arg.to_string_lossy().starts_with("ControlPath")),
                "{odd}"
            );
        }

        let long = dir.path().join("x".repeat(80));
        let ssh = Ssh::new("box".parse().unwrap()).with_control_dir(long);
        let command = ssh.command(&RemoteCommand::program("true"));
        assert!(
            !command
                .as_std()
                .get_args()
                .any(|arg| arg.to_string_lossy().starts_with("ControlPath")),
            "a path too long for a socket is skipped"
        );
    }
}
