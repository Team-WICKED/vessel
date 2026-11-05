//! The three blocking I/O threads behind a PTY.
//!
//! `portable-pty`'s reader, writer and `wait` block, so each runs on a
//! dedicated thread and talks to async code over a bounded channel (see the
//! crate docs for the backpressure policy):
//!
//! - waiter: owns the child, reports its exit code, then sweeps the group;
//! - reader: copies output into [`PtyOutput`](crate::PtyOutput)'s channel;
//! - control: owns the writer and the master, and applies [`Control`]
//!   messages in order. The PTY closes when this thread ends.
//!
//! Must not decide policy (when to stop, what to run); `process` does.

use std::io::{ErrorKind, Read, Write};

use bytes::Bytes;
use portable_pty::{Child, MasterPty};
use tokio::sync::{mpsc, oneshot};

use crate::error::PtyError;
use crate::kill::Killer;
use crate::size::WindowSize;

/// Output chunks buffered between the reader thread and the consumer. At up
/// to [`READ_BUFFER`] bytes each this caps buffered output at 1 MiB; when
/// full, the reader blocks (backpressure, never loss).
const OUTPUT_CHUNKS: usize = 64;

/// Input messages (keystrokes, pastes, resizes) queued for the control
/// thread. When full, `PtyInput` calls wait: the program is not reading its
/// input, and the caller is slowed down rather than input dropped.
const CONTROL_MESSAGES: usize = 64;

/// Largest single read from the PTY.
const READ_BUFFER: usize = 16 * 1024;

/// One instruction for the control thread, applied in order.
#[derive(Debug)]
pub(crate) enum Control {
    Write(Bytes),
    Resize(WindowSize),
}

/// The async ends of the three threads' channels.
pub(crate) struct Threads {
    /// The only strong sender to the control thread.
    pub(crate) control: mpsc::Sender<Control>,
    pub(crate) output: mpsc::Receiver<Bytes>,
    pub(crate) exit: oneshot::Receiver<Option<i32>>,
}

/// Start the waiter, reader and control threads.
///
/// The waiter starts first: once it owns the child, the child is always
/// reaped, even if a later thread fails to start. On error the caller must
/// stop the program.
pub(crate) fn start(
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    killer: &Killer,
) -> Result<Threads, PtyError> {
    let attach = |error: &dyn std::fmt::Display| PtyError::Attach {
        message: error.to_string(),
    };
    let reader = master.try_clone_reader().map_err(|error| attach(&error))?;
    let writer = master.take_writer().map_err(|error| attach(&error))?;

    let (exit_sender, exit) = oneshot::channel();
    let sweep = Sweep::new(killer);
    spawn("vessel-pty-wait", move || {
        wait_for_exit(child, &sweep, exit_sender);
    })?;
    let (output_sender, output) = mpsc::channel(OUTPUT_CHUNKS);
    spawn("vessel-pty-read", move || {
        read_output(reader, &output_sender);
    })?;
    let (control, control_receiver) = mpsc::channel(CONTROL_MESSAGES);
    spawn("vessel-pty-control", move || {
        apply_control(control_receiver, writer, master);
    })?;
    Ok(Threads {
        control,
        output,
        exit,
    })
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), PtyError> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map(drop)
        .map_err(PtyError::Thread)
}

/// Waiter thread: block until the program exits, sweep its group, report.
fn wait_for_exit(
    mut child: Box<dyn Child + Send + Sync>,
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
/// the PTY closes when this thread ends (on Windows that closes the pseudo
/// console, which ends the output stream).
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
        if let Err(error) = self.group.signal(vessel_proc::Signal::Kill) {
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

#[cfg(test)]
mod tests {
    use portable_pty::ExitStatus;

    use super::*;

    #[test]
    fn signals_are_not_exit_codes() {
        assert_eq!(exit_code(&ExitStatus::with_exit_code(3)), Some(3));
        assert_eq!(exit_code(&ExitStatus::with_signal("Hangup")), None);
        // NTSTATUS-style codes keep their bits, as `std` reports them.
        assert_eq!(
            exit_code(&ExitStatus::with_exit_code(0xC000_013A)),
            Some(i32::from_ne_bytes(0xC000_013A_u32.to_ne_bytes()))
        );
    }
}
