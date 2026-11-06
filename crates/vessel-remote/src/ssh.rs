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

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use vessel_client::{RemotePath, SshTarget};

/// Lines of stderr kept for error messages.
pub const STDERR_LINES: usize = 64;

/// Most bytes of a command's stdout kept by [`Ssh::run`]; the commands it
/// runs print a line or a short list.
const STDOUT_LIMIT: usize = 1024 * 1024;

/// `ssh`'s exit code for its own failures (it could not connect, log in,
/// or lost the connection).
pub const SSH_FAILED: i32 = 255;

/// A line of remote shell text, built from fixed words and quoted values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommand {
    line: String,
}

impl RemoteCommand {
    /// A command whose program is a fixed word (`sh`, `uname`).
    #[must_use]
    pub fn program(word: &'static str) -> Self {
        Self {
            line: word.to_owned(),
        }
    }

    /// A command whose program is a file on the remote machine.
    #[must_use]
    pub fn path_program(path: &RemotePath) -> Self {
        Self {
            line: quote_path(path),
        }
    }

    /// Add a fixed word (an option, a subcommand).
    #[must_use]
    pub fn arg(mut self, word: &'static str) -> Self {
        self.line.push(' ');
        self.line.push_str(word);
        self
    }

    /// Add a value, quoted so the remote shell reads it back unchanged.
    #[must_use]
    pub fn value(mut self, text: &str) -> Self {
        self.line.push(' ');
        self.line.push_str(&quote(text));
        self
    }

    /// Add a remote path, quoted (a home-relative one as `"$HOME"/'…'`).
    #[must_use]
    pub fn path(mut self, path: &RemotePath) -> Self {
        self.line.push(' ');
        self.line.push_str(&quote_path(path));
        self
    }

    /// The line as `ssh` sends it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.line
    }
}

impl fmt::Display for RemoteCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.line)
    }
}

/// `text` in single quotes, each `'` written as `'\''`.
fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len().saturating_add(2));
    quoted.push('\'');
    for c in text.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

fn quote_path(path: &RemotePath) -> String {
    match path.home_relative() {
        Some("") => "\"$HOME\"".to_owned(),
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(path.as_str()),
    }
}

/// The last [`STDERR_LINES`] lines a command wrote to stderr.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StderrTail {
    lines: VecDeque<String>,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        if self.lines.len() >= STDERR_LINES {
            let _oldest = self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The kept lines, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// The last non-empty line, usually the one that says what failed.
    #[must_use]
    pub fn last(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
    }

    /// Whether any kept line contains `text`.
    #[must_use]
    pub fn contains(&self, text: &str) -> bool {
        self.lines.iter().any(|line| line.contains(text))
    }
}

/// Where a run's stdin comes from.
#[derive(Debug, Clone, Copy)]
pub enum Input<'a> {
    /// Nothing (stdin is closed at once).
    Empty,
    /// These bytes.
    Bytes(&'a [u8]),
    /// This local file, streamed.
    File(&'a Path),
}

/// A finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// Its exit code; `None` if `ssh` was killed by a signal.
    pub code: Option<i32>,
    /// What it printed (at most 1 MiB).
    pub stdout: Vec<u8>,
    /// The end of what it, or `ssh`, wrote to stderr.
    pub stderr: StderrTail,
}

impl Finished {
    /// Whether it exited 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Stdout as text (lossy).
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// `Ok` if it exited 0, else why not.
    ///
    /// # Errors
    /// [`Failed`] with the exit code and the stderr tail.
    pub fn check(self) -> Result<Self, Failed> {
        if self.success() {
            Ok(self)
        } else {
            Err(Failed {
                code: self.code,
                stderr: self.stderr,
            })
        }
    }
}

/// A command that did not exit 0.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", describe(*code, stderr))]
pub struct Failed {
    /// The exit code; 255 is `ssh` itself.
    pub code: Option<i32>,
    /// The end of stderr.
    pub stderr: StderrTail,
}

fn describe(code: Option<i32>, stderr: &StderrTail) -> String {
    let what = match code {
        Some(SSH_FAILED) => "ssh failed".to_owned(),
        Some(code) => format!("the remote command exited {code}"),
        None => "ssh was killed".to_owned(),
    };
    match stderr.last() {
        Some(line) => format!("{what}: {line}"),
        None => what,
    }
}

/// Why `ssh` could not be run at all.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// The `ssh` program could not be started.
    #[error("could not run `{}` (is the OpenSSH client installed?)", program.display())]
    Spawn {
        /// What was run.
        program: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
    /// Reading its output, or waiting for it, failed.
    #[error("talking to ssh failed")]
    Io(#[source] std::io::Error),
    /// A file to send could not be read.
    #[error("cannot read {}", path.display())]
    Input {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

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
        let (fed, stdout, (), status) = tokio::join!(
            feed(stdin, input),
            read_limited(stdout),
            collect_stderr(stderr, Arc::clone(&tail)),
            child.wait(),
        );
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

/// A long-running `ssh` command.
#[derive(Debug)]
pub struct Running {
    /// The `ssh` process; its stdin and stdout are piped, stderr is taken.
    pub child: Child,
    stderr: Arc<Mutex<StderrTail>>,
}

impl Running {
    /// The end of stderr so far.
    #[must_use]
    pub fn stderr(&self) -> StderrTail {
        self.stderr
            .lock()
            .map(|tail| tail.clone())
            .unwrap_or_default()
    }

    /// A handle to the stderr tail that outlives this value.
    #[must_use]
    pub fn stderr_handle(&self) -> StderrHandle {
        StderrHandle(Arc::clone(&self.stderr))
    }
}

/// Shared access to a running command's [`StderrTail`].
#[derive(Debug, Clone)]
pub struct StderrHandle(Arc<Mutex<StderrTail>>);

impl StderrHandle {
    /// The end of stderr so far.
    #[must_use]
    pub fn snapshot(&self) -> StderrTail {
        self.0.lock().map(|tail| tail.clone()).unwrap_or_default()
    }
}

/// Write `input` to the command and close its stdin. A command that exits
/// without reading everything closes the pipe; that is its exit code's
/// business, not an error here.
async fn feed(mut stdin: tokio::process::ChildStdin, input: Input<'_>) -> Result<(), SshError> {
    let written = match input {
        Input::Empty => Ok(()),
        Input::Bytes(bytes) => stdin.write_all(bytes).await,
        Input::File(path) => {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|source| SshError::Input {
                    path: path.to_owned(),
                    source,
                })?;
            tokio::io::copy(&mut file, &mut stdin).await.map(drop)
        }
    };
    let flushed = match written {
        Ok(()) => stdin.shutdown().await,
        Err(error) => Err(error),
    };
    match flushed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            tracing::debug!("the remote command closed its stdin early");
            Ok(())
        }
        Err(error) => Err(SshError::Io(error)),
    }
}

/// Read to the end, keeping at most [`STDOUT_LIMIT`] bytes.
async fn read_limited(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(kept);
        }
        let room = STDOUT_LIMIT.saturating_sub(kept.len()).min(read);
        if let Some(chunk) = buffer.get(..room) {
            kept.extend_from_slice(chunk);
        }
    }
}

/// Keep the last lines of `pipe` in `tail`, logging each at debug level.
async fn collect_stderr(pipe: impl AsyncRead + Unpin, tail: Arc<Mutex<StderrTail>>) {
    let mut lines = tokio::io::BufReader::new(pipe).split(b'\n');
    while let Ok(Some(line)) = lines.next_segment().await {
        let line = String::from_utf8_lossy(&line).trim_end().to_owned();
        tracing::debug!(%line, "ssh stderr");
        if let Ok(mut tail) = tail.lock() {
            tail.push(line);
        }
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

    fn path(text: &str) -> RemotePath {
        text.parse().unwrap()
    }

    #[test]
    fn values_are_single_quoted_and_paths_keep_home() {
        let command = RemoteCommand::path_program(&path("~/.vessel/bin/vesseld-0.2.0"))
            .arg("--data-dir")
            .path(&path("~/.vessel"))
            .arg("attach");
        assert_eq!(
            command.as_str(),
            "\"$HOME\"/'.vessel/bin/vesseld-0.2.0' --data-dir \"$HOME\"/'.vessel' attach"
        );
        let absolute = RemoteCommand::program("ls").path(&path("/srv/it's here"));
        assert_eq!(absolute.as_str(), "ls '/srv/it'\\''s here'");
        assert_eq!(
            RemoteCommand::program("cd").path(&path("~")).as_str(),
            "cd \"$HOME\""
        );
        let hostile = RemoteCommand::program("echo").value("$(rm -rf ~); `x` \"y\" 'z'");
        assert_eq!(
            hostile.as_str(),
            "echo '$(rm -rf ~); `x` \"y\" '\\''z'\\'''"
        );
    }

    /// The quoting, read back by a real `sh`, gives the original text.
    #[cfg(unix)]
    #[test]
    fn sh_reads_quoted_values_back_unchanged() {
        let texts = [
            "plain",
            "it's",
            "''",
            "$HOME `id` $(id) \"q\" \\ ; & | > <",
            "a\tb",
        ];
        for text in texts {
            let line = RemoteCommand::program("printf").value("%s").value(text);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(line.as_str())
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(output.stdout).unwrap(), text, "{line}");
        }
        let home = RemoteCommand::program("printf")
            .value("%s")
            .path(&path("~/a b/'c'"));
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(home.as_str())
            .env("HOME", "/home/x y")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "/home/x y/a b/'c'"
        );
    }

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

    #[test]
    fn the_stderr_tail_keeps_the_last_lines() {
        let mut tail = StderrTail::default();
        for index in 0..100 {
            tail.push(format!("line {index}"));
        }
        tail.push(String::new());
        assert_eq!(tail.lines().count(), STDERR_LINES);
        assert_eq!(tail.lines().next(), Some("line 37"));
        assert_eq!(tail.last(), Some("line 99"));
        assert!(tail.contains("line 50"));
        let failed = Failed {
            code: Some(SSH_FAILED),
            stderr: tail,
        };
        assert_eq!(failed.to_string(), "ssh failed: line 99");
    }
}
