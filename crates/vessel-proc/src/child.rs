//! [`spawn`] and [`Child`]: a piped child that is stopped as a group.

use std::process::Stdio;

use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

use crate::error::{SignalError, SpawnError, Stream, WaitError};
#[cfg(unix)]
use crate::group::ProcessGroup;
use crate::signal::Signal;
use crate::spec::CommandSpec;

/// How often a refused signal looks again whether the leader has exited
/// (`Child::reaped_within`).
#[cfg(unix)]
const REAP_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// A started child and its three pipes.
#[derive(Debug)]
pub struct Spawned {
    /// The process.
    pub child: Child,
    /// Its standard input.
    pub stdin: ChildStdin,
    /// Its standard output.
    pub stdout: ChildStdout,
    /// Its standard error.
    pub stderr: ChildStderr,
}

/// Start `spec` with piped stdio, as the leader of a new process group (a
/// new job object on Windows).
///
/// # Errors
/// Fails if the program cannot be started (on Windows also if its job
/// object cannot be set up).
pub fn spawn(spec: &CommandSpec) -> Result<Spawned, SpawnError> {
    let mut command = tokio::process::Command::from(spec.to_command());
    let _ = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Backstop for paths that drop a `Child` without waiting.
        .kill_on_drop(true);

    let program = || spec.program.clone();
    let mut inner = os::spawn(command).map_err(|source| SpawnError::Start {
        program: program(),
        source,
    })?;
    let missing = |stream| SpawnError::MissingPipe {
        program: program(),
        stream,
    };
    let (stdin, stdout, stderr) = os::pipes(&mut inner);
    let stdin = stdin.ok_or_else(|| missing(Stream::Stdin))?;
    let stdout = stdout.ok_or_else(|| missing(Stream::Stdout))?;
    let stderr = stderr.ok_or_else(|| missing(Stream::Stderr))?;
    let pid = inner
        .id()
        .ok_or_else(|| SpawnError::NoPid { program: program() })?;
    #[cfg(unix)]
    let group = ProcessGroup::of_leader(pid).map_err(|source| SpawnError::NotLeader {
        program: program(),
        source,
    })?;
    tracing::debug!(pid, program = %spec.program.display(), "spawned");

    Ok(Spawned {
        child: Child {
            inner,
            pid,
            #[cfg(unix)]
            group,
            state: State::Running,
        },
        stdin,
        stdout,
        stderr,
    })
}

/// A running child that leads its own process group (on Windows: the first
/// process of its own job object).
///
/// Dropping a `Child` that has not been waited for kills its whole group, so
/// an early return or a cancelled task cannot leak processes. On Windows
/// dropping it closes the job, which kills every process in it.
#[derive(Debug)]
pub struct Child {
    inner: os::Inner,
    pid: u32,
    #[cfg(unix)]
    group: ProcessGroup,
    state: State,
}

/// Whether the child has been reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    /// Reaped with this exit code (`None` when ended by a signal).
    Reaped(Option<i32>),
}

impl Child {
    /// The child's process id (also its group id on Unix).
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Ask the child's process group to stop.
    ///
    /// On Unix this signals every process in the group. Windows has no
    /// polite signal for a windowless process, so every signal terminates
    /// the child's job: the child and everything it started (through a
    /// `.cmd` shim, the shim's `cmd.exe`, the real harness and its children).
    /// After the child has been reaped this does nothing:
    /// [`wait`](Self::wait) already swept the group.
    ///
    /// # Errors
    /// Fails if the OS refuses to deliver the signal.
    pub fn signal(&mut self, signal: Signal) -> Result<(), SignalError> {
        if let State::Reaped(_) = self.state {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let group = self.group;
            group.signal_with_leader(signal, |grace| self.reaped_within(grace))
        }
        #[cfg(windows)]
        {
            let _ = signal;
            self.inner.start_kill().map_err(SignalError::from)
        }
    }

    /// Whether the leader has exited, looking for up to `grace`: if so it is
    /// reaped now, its code kept for [`wait`](Self::wait) (which would reap
    /// it next anyway). For a refused group signal
    /// ([`ProcessGroup::signal_with_leader`]); blocks, polling `try_wait`,
    /// which never blocks and succeeds only for an exited leader (`waitid`
    /// with `WNOWAIT`, which would not reap, is missing on macOS).
    #[cfg(unix)]
    fn reaped_within(&mut self, grace: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now().checked_add(grace);
        loop {
            match self.inner.try_wait() {
                Ok(Some(status)) => {
                    self.state = State::Reaped(status.code());
                    return true;
                }
                Ok(None) => {}
                Err(_) => return false,
            }
            if deadline.is_none_or(|deadline| std::time::Instant::now() >= deadline) {
                return false;
            }
            std::thread::sleep(REAP_POLL);
        }
    }

    /// Wait for the child to exit, then kill whatever is left of its group.
    ///
    /// Returns the exit code, or `None` if the child was ended by a signal.
    /// Cancel-safe: if the future is dropped early nothing is lost, and a
    /// later call waits again. Once the child has exited, further calls
    /// return the same code immediately.
    ///
    /// # Errors
    /// Fails if the OS cannot report the child's status.
    pub async fn wait(&mut self) -> Result<Option<i32>, WaitError> {
        if let State::Reaped(code) = self.state {
            return Ok(code);
        }
        let status = os::wait_for_leader(&mut self.inner).await?;
        let code = status.code();
        self.state = State::Reaped(code);
        self.sweep();
        Ok(code)
    }

    /// Kill leftovers of the group after the leader exited, so a harness's
    /// own children (MCP servers, shells) never outlive it.
    fn sweep(&mut self) {
        #[cfg(unix)]
        let swept = self.group.signal(Signal::Kill);
        #[cfg(windows)]
        let swept = self.inner.start_kill().map_err(SignalError::from);
        if let Err(error) = swept {
            tracing::warn!(pid = self.pid, %error, "cannot sweep process group");
        }
    }
}

/// Unix: kill the whole group of a child dropped before it was reaped; the
/// runtime's orphan queue then reaps the leader. (On Windows dropping the
/// job handle does the same: the job is kill-on-close.)
#[cfg(unix)]
impl Drop for Child {
    fn drop(&mut self) {
        if self.state == State::Running {
            let group = self.group;
            if let Err(error) =
                group.signal_with_leader(Signal::Kill, |grace| self.reaped_within(grace))
            {
                tracing::warn!(pid = self.pid, %error, "cannot kill dropped process group");
            }
        }
    }
}

/// What differs between Unix process groups and Windows job objects.
///
/// [Decision Log]
/// - Purpose: on Windows, npm installs `claude-agent-acp` and `codex-acp` as
///   `.cmd` shims, so the direct child is `cmd.exe` and the adapter is its
///   child. Terminating the direct child left the adapter running (spending
///   API credit mid-turn) and holding the pipes, so a session never ended.
/// - Alternatives: assign the running child to a job after spawning it
///   (`win32job`; a child that starts its own children within the first
///   milliseconds escapes, and closing that race needs `unsafe` thread
///   resumption); our own Win32 FFI (the workspace forbids `unsafe`).
/// - Choice: `process-wrap`'s `JobObject` and `KillOnDrop`: the child
///   starts suspended, joins a new kill-on-close job, then resumes, so
///   nothing escapes, and closing the job (dropping the [`Child`]) kills
///   all of it.
/// - Constraint: `process-wrap`'s own `wait` also waits for every process
///   left in the job, which could block forever on a lingering grandchild.
///   [`wait_for_leader`] waits for the child itself, as on Unix, and
///   [`Child::sweep`] then terminates the job.
#[cfg(unix)]
mod os {
    use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

    pub(super) type Inner = tokio::process::Child;

    pub(super) fn spawn(mut command: tokio::process::Command) -> std::io::Result<Inner> {
        let _ = command.process_group(0);
        command.spawn()
    }

    pub(super) const fn pipes(
        inner: &mut Inner,
    ) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
        (inner.stdin.take(), inner.stdout.take(), inner.stderr.take())
    }

    pub(super) async fn wait_for_leader(
        inner: &mut Inner,
    ) -> std::io::Result<std::process::ExitStatus> {
        inner.wait().await
    }
}

/// See the Unix `os` module for the Decision Log.
#[cfg(windows)]
mod os {
    use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject, KillOnDrop};
    use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

    pub(super) type Inner = Box<dyn ChildWrapper>;

    pub(super) fn spawn(command: tokio::process::Command) -> std::io::Result<Inner> {
        let mut wrapped = CommandWrap::from(command);
        let _ = wrapped.wrap(JobObject).wrap(KillOnDrop);
        wrapped.spawn()
    }

    pub(super) fn pipes(
        inner: &mut Inner,
    ) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
        (
            inner.stdin().take(),
            inner.stdout().take(),
            inner.stderr().take(),
        )
    }

    /// The child itself, not the whole job: `JobObject` wraps the plain
    /// child directly (`KillOnDrop` only sets a flag), so its inner wrapper
    /// is the process we started.
    pub(super) async fn wait_for_leader(
        inner: &mut Inner,
    ) -> std::io::Result<std::process::ExitStatus> {
        inner.inner_mut().wait().await
    }
}
