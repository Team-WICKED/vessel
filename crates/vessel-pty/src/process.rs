//! [`PtyProcess`]: the program in a PTY, its I/O threads and its exit.

use std::io::{ErrorKind, Read, Write};

use bytes::Bytes;
use portable_pty::{CommandBuilder, MasterPty, native_pty_system};
use tokio::sync::{mpsc, oneshot};
use vessel_proc::{CommandSpec, EnvValue, Signal, SignalError};

use crate::error::{PtyClosed, PtyError};
use crate::kill::Killer;
use crate::size::WindowSize;

/// Output chunks buffered between the reader thread and the consumer. At up
/// to [`READ_BUFFER`] bytes each this caps buffered output at 1 MiB; when
/// full, the reader blocks (backpressure, never loss).
const OUTPUT_CHUNKS: usize = 64;

/// Input messages (keystrokes, pastes, resizes) queued for the control
/// thread. When full, [`PtyInput`] calls wait: the program is not reading
/// its input, and the caller is slowed down rather than input dropped.
const CONTROL_MESSAGES: usize = 64;

/// Largest single read from the PTY.
const READ_BUFFER: usize = 16 * 1024;

/// `TERM` given to programs whose spec does not mention it.
///
/// The node's own `TERM` (if it has one at all, which a service usually
/// does not) describes the node's terminal, not the clients'. Every client
/// terminal Vessel targets implements xterm's 256-colour feature set.
const DEFAULT_TERM: &str = "xterm-256color";

/// One instruction for the control thread, applied in order.
#[derive(Debug)]
enum Control {
    Write(Bytes),
    Resize(WindowSize),
}

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
            terminate_unidentified(&*child);
            return Err(PtyError::NoPid { program: program() });
        };
        let mut killer = Killer::new(&*child, pid).map_err(|source| {
            terminate_unidentified(&*child);
            PtyError::Group {
                program: program(),
                source,
            }
        })?;

        // From here on, a failure must stop the program before returning.
        let started = start_threads(pair.master, child, &killer);
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
                    PtyOutput {
                        chunks: threads.output,
                    },
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
        PtyInput {
            control: self.input.clone(),
        }
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

/// Cloneable input side of a [`PtyProcess`].
///
/// Holds only a weak reference, so outstanding handles never keep a PTY
/// open after its program has exited.
#[derive(Debug, Clone)]
pub struct PtyInput {
    control: mpsc::WeakSender<Control>,
}

impl PtyInput {
    /// Write bytes to the program's terminal input. Waits while the input
    /// queue is full.
    ///
    /// # Errors
    /// [`PtyClosed`] once the program has exited.
    pub async fn write(&self, data: Bytes) -> Result<(), PtyClosed> {
        self.send(Control::Write(data)).await
    }

    /// Change the window size. Applied in order with writes.
    ///
    /// # Errors
    /// [`PtyClosed`] once the program has exited.
    pub async fn resize(&self, size: WindowSize) -> Result<(), PtyClosed> {
        self.send(Control::Resize(size)).await
    }

    async fn send(&self, message: Control) -> Result<(), PtyClosed> {
        let control = self.control.upgrade().ok_or(PtyClosed)?;
        if control.send(message).await.is_err() {
            return Err(PtyClosed);
        }
        Ok(())
    }
}

/// Output of a [`PtyProcess`], as raw byte chunks in order.
#[derive(Debug)]
pub struct PtyOutput {
    chunks: mpsc::Receiver<Bytes>,
}

impl PtyOutput {
    /// The next chunk, or `None` once the PTY is closed and drained.
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<Bytes> {
        self.chunks.recv().await
    }
}

/// Channels to the three I/O threads.
struct Threads {
    control: mpsc::Sender<Control>,
    output: mpsc::Receiver<Bytes>,
    exit: oneshot::Receiver<Option<i32>>,
}

/// Start the waiter, reader and control threads.
///
/// The waiter starts first: once it owns the child, the child is always
/// reaped, even if a later thread fails to start.
fn start_threads(
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    killer: &Killer,
) -> Result<Threads, PtyError> {
    let reader = master
        .try_clone_reader()
        .map_err(|error| PtyError::Attach {
            message: error.to_string(),
        })?;
    let writer = master.take_writer().map_err(|error| PtyError::Attach {
        message: error.to_string(),
    })?;

    let (exit_sender, exit) = oneshot::channel();
    let sweep = Sweep::new(killer);
    spawn_thread("vessel-pty-wait", move || {
        wait_for_exit(child, &sweep, exit_sender);
    })?;
    let (output_sender, output) = mpsc::channel(OUTPUT_CHUNKS);
    spawn_thread("vessel-pty-read", move || {
        read_output(reader, &output_sender);
    })?;
    let (control, control_receiver) = mpsc::channel(CONTROL_MESSAGES);
    spawn_thread("vessel-pty-control", move || {
        apply_control(control_receiver, writer, master);
    })?;
    Ok(Threads {
        control,
        output,
        exit,
    })
}

fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), PtyError> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map(drop)
        .map_err(PtyError::Thread)
}

/// Waiter thread: block until the program exits, sweep its group, report.
fn wait_for_exit(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    sweep: &Sweep,
    exit: oneshot::Sender<Option<i32>>,
) {
    let code = match child.wait() {
        Ok(status) => exit_code(&status),
        Err(error) => {
            tracing::warn!(%error, "cannot wait for PTY program");
            None
        }
    };
    sweep.run();
    if exit.send(code).is_err() {
        tracing::debug!("pty exit status had no receiver");
    }
}

/// `portable-pty` reports "ended by a signal" with a placeholder code of 1,
/// which is not an exit code.
fn exit_code(status: &portable_pty::ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        // Windows exit codes are `u32` (NTSTATUS values have the top bit
        // set); reinterpreting them matches what `std` reports.
        Some(status.exit_code().cast_signed())
    }
}

/// Reader thread: copy PTY output into the channel until the PTY closes.
fn read_output(mut reader: Box<dyn Read + Send>, output: &mpsc::Sender<Bytes>) {
    let mut buffer = vec![0_u8; READ_BUFFER];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let Some(chunk) = buffer.get(..read) else {
                    break;
                };
                // One allocation per chunk, sized to what was read.
                if output.blocking_send(Bytes::copy_from_slice(chunk)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // Linux reports EIO once no process holds the PTY: that is EOF.
            Err(error) => {
                tracing::debug!(%error, "pty output ended");
                break;
            }
        }
    }
}

/// Control thread: apply writes and resizes in order. Owns the master, so
/// the PTY closes when this thread ends.
fn apply_control(
    mut control: mpsc::Receiver<Control>,
    mut writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
) {
    while let Some(message) = control.blocking_recv() {
        match message {
            Control::Write(data) => {
                if let Err(error) = writer.write_all(&data).and_then(|()| writer.flush()) {
                    tracing::debug!(%error, "pty input closed");
                    break;
                }
            }
            Control::Resize(size) => {
                if let Err(error) = master.resize(size.to_pty()) {
                    tracing::debug!(%error, "pty resize failed");
                }
            }
        }
    }
    drop(writer);
    drop(master);
}

/// Kills what is left of the program's group after it exits (Unix), so a
/// shell or MCP server it started cannot keep the PTY open.
struct Sweep {
    #[cfg(unix)]
    group: vessel_proc::ProcessGroup,
}

impl Sweep {
    #[cfg(unix)]
    const fn new(killer: &Killer) -> Self {
        Self {
            group: killer.group(),
        }
    }

    #[cfg(not(unix))]
    const fn new(_killer: &Killer) -> Self {
        Self {}
    }

    #[cfg(unix)]
    fn run(&self) {
        if let Err(error) = self.group.signal(Signal::Kill) {
            tracing::warn!(%error, "cannot sweep PTY process group");
        }
    }

    /// Nothing to sweep: the pseudo console's closing ends the rest.
    #[cfg(not(unix))]
    #[expect(
        clippy::unused_self,
        reason = "same signature as the Unix version, which sweeps its group"
    )]
    const fn run(&self) {}
}

/// Best-effort stop for a child whose pid could not be used.
fn terminate_unidentified(child: &(dyn portable_pty::Child + Send + Sync)) {
    if let Err(error) = child.clone_killer().kill() {
        tracing::warn!(%error, "cannot stop PTY program");
    }
}

/// The `portable-pty` command for `spec`.
fn command_builder(spec: &CommandSpec) -> CommandBuilder {
    let mut builder = CommandBuilder::new(&spec.program);
    builder.args(&spec.args);
    for (name, value) in &spec.env {
        match value {
            EnvValue::Set(value) => builder.env(name, value),
            EnvValue::Remove => builder.env_remove(name),
        }
    }
    if spec.env_change("TERM").is_none() {
        builder.env("TERM", DEFAULT_TERM);
    }
    if let Some(cwd) = &spec.cwd {
        builder.cwd(cwd);
    }
    builder
}
