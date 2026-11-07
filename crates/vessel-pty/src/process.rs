//! [`PtyProcess`]: a program in a PTY, from spawn to exit.
//!
//! Owns the program's lifecycle: opening the PTY, starting the program and
//! its I/O threads, stopping it, and reporting its exit. The byte streams
//! are in `streams`, the threads in `threads`, the command translation in
//! `command`.

use bytes::Bytes;
use portable_pty::native_pty_system;
use tokio::sync::{mpsc, oneshot};
use vessel_proc::{CommandSpec, Signal, SignalError};

use crate::command::command_builder;
use crate::error::{PtyClosed, PtyError};
use crate::kill::Killer;
use crate::size::WindowSize;
use crate::streams::{PtyInput, PtyOutput};
use crate::threads::{self, Control};

/// A program running in a pseudo-terminal.
///
/// Owned by one task (the session actor). Input can also be sent through
/// cloneable [`PtyInput`] handles; output arrives on the [`PtyOutput`]
/// returned by [`spawn`](Self::spawn). Dropping a `PtyProcess` whose program
/// is still running kills it (and on Unix its whole group).
#[derive(Debug)]
pub struct PtyProcess {
    pid: u32,
    killer: Killer,
    /// The only strong sender; dropping it ends the control thread, which
    /// closes the PTY. `None` once the program has exited.
    control: Option<mpsc::Sender<Control>>,
    /// Handed to [`PtyInput`]s so they cannot keep the PTY open.
    input: mpsc::WeakSender<Control>,
    exit: oneshot::Receiver<Option<i32>>,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    /// Exited with this code (`None` when ended by a signal).
    Exited(Option<i32>),
}

impl PtyProcess {
    /// Start `spec` in a new PTY of the given size.
    ///
    /// Makes blocking system calls (open, fork/exec) and starts three
    /// threads; call it from a blocking context.
    ///
    /// # Errors
    /// Fails if the PTY cannot be created, the program cannot be started, or
    /// an I/O thread cannot be started. The program is stopped before an
    /// error is returned.
    pub fn spawn(spec: &CommandSpec, size: WindowSize) -> Result<(Self, PtyOutput), PtyError> {
        let program = || spec.program.display().to_string();
        let pair = native_pty_system()
            .openpty(size.to_pty())
            .map_err(|error| PtyError::Open {
                message: error.to_string(),
            })?;
        let child = pair
            .slave
            .spawn_command(command_builder(spec))
            .map_err(|error| PtyError::Spawn {
                program: program(),
                message: error.to_string(),
            })?;
        // The node must not hold the slave end, or the output would never
        // end: the PTY stays open while any process has it.
        drop(pair.slave);

        let Some(pid) = child.process_id() else {
            stop_and_reap(child);
            return Err(PtyError::NoPid { program: program() });
        };
        let mut killer = match Killer::new(&*child, pid) {
            Ok(killer) => killer,
            Err(source) => {
                stop_and_reap(child);
                return Err(PtyError::Group {
                    program: program(),
                    source,
                });
            }
        };

        // From here on, a failure must stop the program before returning.
        let started = threads::start(pair.master, child, killer.reaper(pid));
        match started {
            Ok(threads) => {
                tracing::debug!(pid, program = %spec.program.display(), "pty spawned");
                Ok((
                    Self {
                        pid,
                        killer,
                        input: threads.control.downgrade(),
                        control: Some(threads.control),
                        exit: threads.exit,
                        state: State::Running,
                    },
                    PtyOutput::new(threads.output),
                ))
            }
            Err(error) => {
                if let Err(kill_error) = killer.signal(Signal::Kill) {
                    tracing::warn!(pid, error = %kill_error, "cannot stop program after a failed PTY setup");
                }
                Err(error)
            }
        }
    }

    /// The program's process id (on Unix also its process group id).
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// A cloneable handle for writing input and resizing.
    #[must_use]
    pub fn input(&self) -> PtyInput {
        PtyInput::new(self.input.clone())
    }

    /// Write bytes to the program's terminal input.
    ///
    /// # Errors
    /// [`PtyClosed`] once the program has exited.
    pub async fn write(&self, data: Bytes) -> Result<(), PtyClosed> {
        self.input().write(data).await
    }

    /// Change the window size; the program receives `SIGWINCH` (Unix) or a
    /// console resize event (Windows).
    ///
    /// # Errors
    /// [`PtyClosed`] once the program has exited.
    pub async fn resize(&self, size: WindowSize) -> Result<(), PtyClosed> {
        self.input().resize(size).await
    }

    /// Ask the program to stop: on Unix by signalling its whole process
    /// group, on Windows by terminating it (any signal). [`Signal::Hangup`]
    /// is the polite choice for a terminal program. Does nothing once the
    /// program has exited.
    ///
    /// # Errors
    /// Fails if the OS refuses to deliver the signal.
    pub fn signal(&mut self, signal: Signal) -> Result<(), SignalError> {
        match self.state {
            State::Exited(_) => Ok(()),
            State::Running => self.killer.signal(signal),
        }
    }

    /// Kill the program (and on Unix its group) without a grace period.
    ///
    /// # Errors
    /// Fails if the OS refuses to deliver the signal.
    pub fn kill(&mut self) -> Result<(), SignalError> {
        self.signal(Signal::Kill)
    }

    /// Wait for the program to exit; returns its exit code, or `None` if it
    /// was ended by a signal or its status is unknown.
    ///
    /// When the program exits, the rest of its group is killed (Unix) and
    /// the PTY's input side is closed, so the output stream ends once the
    /// last buffered bytes are read. Cancel-safe; after the first exit it
    /// returns the same code immediately.
    pub async fn wait(&mut self) -> Option<i32> {
        if let State::Exited(code) = self.state {
            return code;
        }
        let code = match (&mut self.exit).await {
            Ok(code) => code,
            Err(_waiter_gone) => {
                tracing::warn!(pid = self.pid, "pty waiter thread ended without a status");
                None
            }
        };
        self.state = State::Exited(code);
        // Ends the control thread, which drops the PTY master: on Windows
        // this closes the pseudo console and so ends the output stream.
        self.control = None;
        code
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.state == State::Running
            && let Err(error) = self.killer.signal(Signal::Kill)
        {
            tracing::warn!(pid = self.pid, %error, "cannot kill dropped PTY program");
        }
    }
}

/// Stop a child that never got its waiter thread, and reap it, so a failed
/// spawn leaves no zombie behind. Blocks until the child is gone; `spawn`
/// runs in a blocking context.
pub(crate) fn stop_and_reap(mut child: Box<dyn portable_pty::Child + Send + Sync>) {
    if let Err(error) = child.kill() {
        tracing::warn!(%error, "cannot stop PTY program");
    }
    if let Err(error) = child.wait() {
        tracing::warn!(%error, "cannot reap PTY program");
    }
}
