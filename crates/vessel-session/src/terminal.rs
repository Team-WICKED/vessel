//! The terminal-mode session actor: a PTY's output becomes events.

use std::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;
use vessel_proto::{SessionEvent, SessionStatus, TerminalBytes, TerminalOutput};
use vessel_pty::{PtyOutput, PtyProcess, Signal};

use crate::publish::Publisher;
use crate::stop::{Step, Stopper};

/// How long output may keep arriving after the program exited.
///
/// Normally the output ends right after the exit (the group is swept and
/// nothing holds the PTY any more). A process that left the group (for
/// example a daemon that called `setsid`) can keep the PTY open forever; the
/// session must still end, so draining gives up after this long.
const DRAIN: Duration = Duration::from_secs(2);

/// Owns a PTY program for the life of a terminal session.
pub(crate) struct TerminalActor {
    pub(crate) pty: PtyProcess,
    pub(crate) output: PtyOutput,
    pub(crate) out: Publisher,
    pub(crate) stop: Stopper,
}

impl TerminalActor {
    /// Run until the program has exited and its final status is published.
    pub(crate) async fn run(mut self) {
        self.publish(status(SessionStatus::Starting)).await;
        self.publish(status(SessionStatus::Running)).await;

        let mut output_open = true;
        let code = loop {
            tokio::select! {
                biased;
                step = self.stop.next() => self.apply(step),
                code = self.pty.wait() => break code,
                chunk = self.output.next(), if output_open => match chunk {
                    Some(bytes) => self.publish(terminal(bytes)).await,
                    None => output_open = false,
                },
            }
        };
        if output_open {
            self.drain().await;
        }

        let status = if self.stop.interrupted() {
            SessionStatus::Interrupted
        } else {
            SessionStatus::Exited { code }
        };
        self.publish(SessionEvent::Status { status }).await;
    }

    /// Publish what the PTY still has after the exit, for at most [`DRAIN`].
    async fn drain(&mut self) {
        let deadline = Instant::now()
            .checked_add(DRAIN)
            .unwrap_or_else(Instant::now);
        loop {
            tokio::select! {
                chunk = self.output.next() => match chunk {
                    Some(bytes) => self.publish(terminal(bytes)).await,
                    None => return,
                },
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        pid = self.pty.pid(),
                        "terminal output still open after exit; a process escaped the group"
                    );
                    return;
                }
            }
        }
    }

    /// Publish one event, staying responsive to `kill` while the durable
    /// channel is full.
    async fn publish(&mut self, event: SessionEvent) {
        let slot = loop {
            tokio::select! {
                biased;
                step = self.stop.next() => self.apply(step),
                slot = self.out.slot() => break slot,
            }
        };
        self.out.commit(slot, event);
    }

    /// Carry out a stop step: `SIGHUP` first (what a closing terminal
    /// sends), then `SIGKILL` for the whole group.
    fn apply(&mut self, step: Step) {
        let signal = match step {
            Step::Ask => Signal::Hangup,
            Step::Force => Signal::Kill,
        };
        if let Err(error) = self.pty.signal(signal) {
            tracing::warn!(pid = self.pty.pid(), %error, ?signal, "cannot signal terminal program");
        }
        self.stop.advance(step);
    }
}

const fn status(status: SessionStatus) -> SessionEvent {
    SessionEvent::Status { status }
}

fn terminal(bytes: Bytes) -> SessionEvent {
    SessionEvent::Terminal(TerminalOutput {
        data: TerminalBytes::new(bytes),
    })
}
