//! The terminal-mode session actor: a PTY's output becomes events.

use bytes::Bytes;
use vessel_proto::{SessionEvent, SessionStatus, TerminalBytes, TerminalOutput};
use vessel_pty::{PtyOutput, PtyProcess};

use crate::lifecycle::{Lifecycle, drain_deadline};

/// Owns a PTY program for the life of a terminal session.
pub(crate) struct TerminalActor {
    pub(crate) pty: PtyProcess,
    pub(crate) output: PtyOutput,
    pub(crate) life: Lifecycle,
}

impl TerminalActor {
    /// Run until the program has exited and its final status is published.
    pub(crate) async fn run(mut self) {
        // The program is already running when the actor starts.
        self.life
            .status(&mut self.pty, SessionStatus::Starting)
            .await;
        self.life
            .status(&mut self.pty, SessionStatus::Running)
            .await;

        let mut output_open = true;
        let code = loop {
            tokio::select! {
                biased;
                step = self.life.stop.next() => self.life.apply(&mut self.pty, step),
                code = self.pty.wait() => break code,
                chunk = self.output.next(), if output_open => match chunk {
                    Some(bytes) => self.life.publish(&mut self.pty, terminal(bytes)).await,
                    None => output_open = false,
                },
            }
        };
        if output_open {
            self.drain().await;
        }

        let status = if self.life.stop.interrupted() {
            SessionStatus::Interrupted
        } else {
            SessionStatus::Exited { code }
        };
        self.life.status(&mut self.pty, status).await;
    }

    /// Publish what the PTY still has after the exit, for a bounded time.
    async fn drain(&mut self) {
        let deadline = drain_deadline();
        loop {
            tokio::select! {
                chunk = self.output.next() => match chunk {
                    Some(bytes) => self.life.publish(&mut self.pty, terminal(bytes)).await,
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
}

fn terminal(bytes: Bytes) -> SessionEvent {
    SessionEvent::Terminal(TerminalOutput {
        data: TerminalBytes::new(bytes),
    })
}
