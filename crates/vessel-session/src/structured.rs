//! The structured-mode session actor: an ACP agent's outputs become events.

use tokio::sync::{mpsc, watch};
use vessel_agent::{AgentInput, AgentOutput, AgentOutputs, AgentProcess, Disconnect};
use vessel_proto::{AgentEvent, ProfileName, SessionEvent, SessionStatus};

use crate::lifecycle::{Lifecycle, drain_deadline};
use crate::moving::{Control, Launch};

/// Owns an ACP agent process for the life of a structured session.
///
/// Status follows the handshake: `Starting` until the agent's `Ready`
/// event, then `Running`. A session that never became ready ends `Failed`
/// with the reason (the handshake error, or that the agent exited first),
/// unless it was killed while starting, which ends `Exited` as a killed
/// terminal session does; one that became ready ends `Exited` with the
/// exit code. If the ACP connection
/// ends while the process lives on, the process is stopped: it can no
/// longer be driven.
pub(crate) struct StructuredActor {
    pub(crate) agent: AgentProcess,
    pub(crate) outputs: AgentOutputs,
    pub(crate) outputs_open: bool,
    /// The current agent's input, and where handles find it (`None` while
    /// the session moves to another profile).
    pub(crate) input: AgentInput,
    pub(crate) route: watch::Sender<Option<AgentInput>>,
    /// Moves to another profile, from the runtime.
    pub(crate) control: mpsc::Receiver<Control>,
    /// How the current agent was started.
    pub(crate) launch: Launch,
    /// The profile it runs with; `None` is the harness's default.
    pub(crate) profile: Option<ProfileName>,
    pub(crate) life: Lifecycle,
    pub(crate) ready: bool,
    pub(crate) disconnect: Option<Disconnect>,
    /// The handshake or the protocol failed before anyone killed the
    /// session (a failure after a kill is the kill's doing).
    pub(crate) failed_before_kill: bool,
    /// Where a move to another profile stands.
    pub(crate) move_state: MoveState,
}

/// A structured session's move state (see `moving`).
#[derive(Debug, Default)]
pub(crate) struct MoveState {
    /// A move is under way: the agent's end is not the session's.
    pub(crate) active: bool,
    /// A restarted agent replays the conversation before it is ready; the
    /// log has it already, so it is not published again.
    pub(crate) replaying: bool,
    /// A turn ended (for a forced move, which waits for its cancel).
    pub(crate) turn_over: bool,
    /// How an agent's connection ended during the move.
    pub(crate) disconnect: Option<Disconnect>,
}

impl StructuredActor {
    /// Run until the agent has exited and the final status is published.
    pub(crate) async fn run(mut self) {
        self.life
            .status(&mut self.agent, SessionStatus::Starting)
            .await;

        let mut control_open = true;
        let code = loop {
            tokio::select! {
                biased;
                step = self.life.stop.next() => self.life.apply(&mut self.agent, step),
                code = self.agent.wait() => break code,
                output = self.outputs.next(), if self.outputs_open => match output {
                    Some(output) => self.output(output).await,
                    None => self.outputs_open = false,
                },
                control = self.control.recv(), if control_open => match control {
                    Some(Control::Move(order)) => self.move_profile(order).await,
                    None => control_open = false,
                },
            }
        };
        if self.outputs_open {
            self.drain().await;
        }

        let status = if self.life.stop.interrupted() {
            SessionStatus::Interrupted
        } else if self.ready || (self.life.stop.killed() && !self.failed_before_kill) {
            // Killed while it was still starting ends as a killed terminal
            // session does: exited, with whatever code the stop left. (The
            // kill also fails the pending handshake; that failure is the
            // kill's doing.) A handshake that had failed before the kill
            // stays the more useful answer.
            SessionStatus::Exited { code }
        } else {
            SessionStatus::Failed {
                message: start_failure(self.disconnect.as_ref(), code),
            }
        };
        self.life.status(&mut self.agent, status).await;
    }

    pub(crate) async fn output(&mut self, output: AgentOutput) {
        match output {
            AgentOutput::Event(event) if self.move_state.replaying => {
                // A restarted agent's replay: the log has it already. Its
                // stderr still goes in; `Ready` ends the replay.
                match event {
                    AgentEvent::Ready { .. } => self.move_state.replaying = false,
                    AgentEvent::HarnessLog { .. } => {
                        self.life
                            .publish(&mut self.agent, SessionEvent::Agent { event })
                            .await;
                    }
                    AgentEvent::TurnStarted { .. }
                    | AgentEvent::UserMessageChunk { .. }
                    | AgentEvent::MessageChunk { .. }
                    | AgentEvent::ThoughtChunk { .. }
                    | AgentEvent::ToolCall(_)
                    | AgentEvent::ToolCallUpdate(_)
                    | AgentEvent::Plan { .. }
                    | AgentEvent::PermissionRequested(_)
                    | AgentEvent::PermissionResolved { .. }
                    | AgentEvent::TurnEnded { .. }
                    | AgentEvent::TurnFailed { .. } => {}
                }
            }
            AgentOutput::Disconnected(disconnect) if self.move_state.active => {
                // The old agent being stopped, or the new one failing to
                // start: the move decides what follows, not the session.
                tracing::debug!(%disconnect, "agent connection ended during a move");
                self.move_state.disconnect = Some(disconnect);
            }
            AgentOutput::Event(event) => {
                if matches!(
                    event,
                    AgentEvent::TurnEnded { .. } | AgentEvent::TurnFailed { .. }
                ) {
                    self.move_state.turn_over = true;
                }
                let became_ready = !self.ready && matches!(event, AgentEvent::Ready { .. });
                self.life
                    .publish(&mut self.agent, SessionEvent::Agent { event })
                    .await;
                if became_ready {
                    self.ready = true;
                    self.life
                        .status(&mut self.agent, SessionStatus::Running)
                        .await;
                }
            }
            AgentOutput::Disconnected(disconnect) => {
                tracing::debug!(%disconnect, "agent connection ended");
                self.failed_before_kill = !self.life.stop.killed()
                    && matches!(
                        disconnect,
                        Disconnect::Handshake { .. } | Disconnect::Protocol { .. }
                    );
                self.disconnect = Some(disconnect);
                self.life.begin_stop(&mut self.agent);
            }
        }
    }

    /// Publish what is still buffered after the exit, for a bounded time.
    async fn drain(&mut self) {
        let deadline = drain_deadline();
        loop {
            tokio::select! {
                output = self.outputs.next() => match output {
                    Some(output) => self.output(output).await,
                    None => return,
                },
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        pid = self.agent.pid(),
                        "agent output still open after exit; a process escaped the group"
                    );
                    return;
                }
            }
        }
    }
}

/// Why a session that never became ready failed.
fn start_failure(disconnect: Option<&Disconnect>, code: Option<i32>) -> String {
    match disconnect {
        Some(failure @ (Disconnect::Handshake { .. } | Disconnect::Protocol { .. })) => {
            failure.to_string()
        }
        Some(Disconnect::Closed) | None => match code {
            Some(code) => format!(
                "the agent exited with code {code} before the ACP handshake completed; see its log"
            ),
            None => String::from("the agent was stopped before the ACP handshake completed"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_failures_say_what_happened() {
        let handshake = Disconnect::Handshake {
            message: "the agent does not offer model `x`".to_owned(),
        };
        assert_eq!(
            start_failure(Some(&handshake), Some(0)),
            "the ACP handshake failed: the agent does not offer model `x`"
        );
        assert!(start_failure(Some(&Disconnect::Closed), Some(3)).contains("code 3"));
        assert!(start_failure(None, None).contains("stopped"));
    }
}
