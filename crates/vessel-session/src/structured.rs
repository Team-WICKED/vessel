//! The structured-mode session actor: an ACP agent's outputs become events.

use vessel_agent::{AgentOutput, AgentOutputs, AgentProcess, Disconnect};
use vessel_proto::{AgentEvent, SessionEvent, SessionStatus};

use crate::lifecycle::{Lifecycle, drain_deadline};

/// Owns an ACP agent process for the life of a structured session.
///
/// Status follows the handshake: `Starting` until the agent's `Ready`
/// event, then `Running`. A session that never became ready ends `Failed`
/// with the reason (the handshake error, or that the agent exited first);
/// one that did ends `Exited` with the exit code. If the ACP connection
/// ends while the process lives on, the process is stopped: it can no
/// longer be driven.
pub(crate) struct StructuredActor {
    pub(crate) agent: AgentProcess,
    pub(crate) outputs: AgentOutputs,
    pub(crate) life: Lifecycle,
    pub(crate) ready: bool,
    pub(crate) disconnect: Option<Disconnect>,
}

impl StructuredActor {
    /// Run until the agent has exited and the final status is published.
    pub(crate) async fn run(mut self) {
        self.life
            .status(&mut self.agent, SessionStatus::Starting)
            .await;

        let mut outputs_open = true;
        let code = loop {
            tokio::select! {
                biased;
                step = self.life.stop.next() => self.life.apply(&mut self.agent, step),
                code = self.agent.wait() => break code,
                output = self.outputs.next(), if outputs_open => match output {
                    Some(output) => self.output(output).await,
                    None => outputs_open = false,
                },
            }
        };
        if outputs_open {
            self.drain().await;
        }

        let status = if self.life.stop.interrupted() {
            SessionStatus::Interrupted
        } else if self.ready {
            SessionStatus::Exited { code }
        } else {
            SessionStatus::Failed {
                message: start_failure(self.disconnect.as_ref(), code),
            }
        };
        self.life.status(&mut self.agent, status).await;
    }

    async fn output(&mut self, output: AgentOutput) {
        match output {
            AgentOutput::Event(event) => {
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
