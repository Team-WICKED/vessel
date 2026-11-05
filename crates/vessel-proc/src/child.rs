//! [`spawn`] and [`Child`]: a piped child that is stopped as a group.

use std::process::Stdio;

use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

use crate::error::{SignalError, SpawnError, Stream, WaitError};
#[cfg(unix)]
use crate::group::ProcessGroup;
use crate::signal::Signal;
use crate::spec::CommandSpec;

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

/// Start `spec` with piped stdio, as the leader of a new process group.
///
/// # Errors
/// Fails if the program cannot be started.
pub fn spawn(spec: &CommandSpec) -> Result<Spawned, SpawnError> {
    let mut command = tokio::process::Command::from(spec.to_command());
    let _ = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Backstop for paths that drop a `Child` without waiting; on Windows
        // it is the only cleanup (see `Child::signal`).
        .kill_on_drop(true);
    #[cfg(unix)]
    let _ = command.process_group(0);

    let program = || spec.program.clone();
    let mut inner = command.spawn().map_err(|source| SpawnError::Start {
        program: program(),
        source,
    })?;
    let missing = |stream| SpawnError::MissingPipe {
        program: program(),
        stream,
    };
    let stdin = inner.stdin.take().ok_or_else(|| missing(Stream::Stdin))?;
    let stdout = inner.stdout.take().ok_or_else(|| missing(Stream::Stdout))?;
    let stderr = inner.stderr.take().ok_or_else(|| missing(Stream::Stderr))?;
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

/// A running child that leads its own process group.
///
/// Dropping a `Child` that has not been waited for kills its whole group, so
/// an early return or a cancelled task cannot leak processes.
#[derive(Debug)]
pub struct Child {
    inner: tokio::process::Child,
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
    /// On Unix this signals every process in the group. On Windows every
    /// signal terminates the direct child only; processes it started may
    /// survive (no job objects yet, see the crate docs). After the child has
    /// been reaped this does nothing: [`wait`](Self::wait) already swept the
    /// group.
    ///
    /// # Errors
    /// Fails if the OS refuses to deliver the signal.
    pub fn signal(&mut self, signal: Signal) -> Result<(), SignalError> {
        if let State::Reaped(_) = self.state {
            return Ok(());
        }
        #[cfg(unix)]
        {
            self.group.signal(signal)
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            self.inner.start_kill().map_err(SignalError::from)
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
        let status = self.inner.wait().await?;
        let code = status.code();
        self.state = State::Reaped(code);
        #[cfg(unix)]
        self.sweep();
        Ok(code)
    }

    /// Kill leftovers of the group after the leader exited, so a harness's
    /// own children (MCP servers, shells) never outlive it.
    #[cfg(unix)]
    fn sweep(&self) {
        if let Err(error) = self.group.signal(Signal::Kill) {
            tracing::warn!(pid = self.pid, %error, "cannot sweep process group");
        }
    }
}

/// Unix: kill the whole group of a child dropped before it was reaped; the
/// runtime's orphan queue then reaps the leader. (Windows relies on
/// `kill_on_drop`, which terminates the direct child.)
#[cfg(unix)]
impl Drop for Child {
    fn drop(&mut self) {
        if self.state == State::Running
            && let Err(error) = self.group.signal(Signal::Kill)
        {
            tracing::warn!(pid = self.pid, %error, "cannot kill dropped process group");
        }
    }
}
