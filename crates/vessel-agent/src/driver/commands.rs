//! The client's commands: prompt, cancel, answer a permission request.

use agent_client_protocol::Agent;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use vessel_proto::{AgentEvent, ContentBlock};

use super::{Driver, Inbound, forward};
use crate::error::InputError;
use crate::input::{Command, Reply};
use crate::mapper;

impl Driver {
    pub(super) async fn command(&mut self, cx: &ConnectionTo<Agent>, command: Command) {
        match command {
            Command::Prompt { content, reply } => {
                let result = self.prompt(cx, content).await;
                answer(reply, result);
            }
            Command::CancelTurn { reply } => {
                let result = self.cancel(cx).await;
                answer(reply, result);
            }
            Command::Answer { id, outcome, reply } => {
                let result = self.permissions.answer(id, &outcome);
                if result.is_ok() {
                    self.emit(AgentEvent::PermissionResolved { id, outcome })
                        .await;
                }
                answer(reply, result);
            }
        }
    }

    async fn prompt(
        &mut self,
        cx: &ConnectionTo<Agent>,
        content: Vec<ContentBlock>,
    ) -> Result<(), InputError> {
        if self.turn {
            return Err(InputError::TurnInProgress);
        }
        let Some(session) = &self.session else {
            return Err(InputError::Closed);
        };
        let blocks = content
            .iter()
            .cloned()
            .map(mapper::prompt_block)
            .collect::<Result<Vec<_>, _>>()
            .map_err(InputError::InvalidContent)?;
        let sent = cx.send_request(acp::PromptRequest::new(session.clone(), blocks));
        let inbound = self.inbound.clone();
        let spawned = cx.spawn(async move {
            let result = sent.block_task().await;
            forward(&inbound, Inbound::TurnDone(result)).await;
            Ok(())
        });
        if spawned.is_err() {
            return Err(InputError::Closed);
        }
        self.turn = true;
        self.cancelling = false;
        self.emit(AgentEvent::TurnStarted { content }).await;
        Ok(())
    }

    async fn cancel(&mut self, cx: &ConnectionTo<Agent>) -> Result<(), InputError> {
        let Some(session) = &self.session else {
            return Err(InputError::Closed);
        };
        if !self.turn {
            return Ok(());
        }
        if cx
            .send_notification(acp::CancelNotification::new(session.clone()))
            .is_err()
        {
            return Err(InputError::Closed);
        }
        // ACP: after `session/cancel` the client answers every pending
        // permission request with `cancelled` (also those still on their
        // way, see `permission`); the agent then ends the turn with the
        // `cancelled` stop reason.
        self.cancelling = true;
        self.withdraw_permissions().await;
        Ok(())
    }
}

fn answer(reply: Reply, result: Result<(), InputError>) {
    // The caller may have stopped waiting; the command was applied anyway.
    if reply.send(result).is_err() {
        tracing::debug!("agent command reply dropped");
    }
}
