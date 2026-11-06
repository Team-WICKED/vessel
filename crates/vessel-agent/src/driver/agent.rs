//! The agent's traffic: session updates and permission requests, as they
//! come off the inbound queue.

use std::sync::Arc;

use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;
use vessel_proto::{AgentEvent, PermissionRequest, ToolCallId};

use super::Driver;
use crate::mapper::{self, Update};

impl Driver {
    pub(super) async fn update(&mut self, notification: acp::SessionNotification) {
        match mapper::session_update(notification.update) {
            Update::Event(event) => {
                if let AgentEvent::ToolCall(call) = &event {
                    let _ = self.tool_calls.insert(call.id.clone());
                }
                self.emit(event).await;
            }
            Update::Skipped(kind) => tracing::trace!(?kind, "session update skipped"),
        }
    }

    /// Hold a permission request and announce it.
    pub(super) async fn permission(
        &mut self,
        request: acp::RequestPermissionRequest,
        responder: Responder<acp::RequestPermissionResponse>,
    ) {
        let tool_call = ToolCallId::new(Arc::clone(&request.tool_call.tool_call_id.0));
        let known = !self.tool_calls.insert(tool_call.clone());
        if let Some(event) = mapper::permission_tool_call(request.tool_call, known) {
            self.emit(event).await;
        }
        let options = mapper::permission_options(request.options);
        let id = self.permissions.open(responder, &options);
        self.emit(AgentEvent::PermissionRequested(PermissionRequest {
            id,
            tool_call,
            options,
        }))
        .await;
        if self.cancelling {
            // Sent before the agent saw the cancel. ACP has the client
            // answer every permission request of a cancelled turn with
            // `cancelled`, and the agent cannot end the turn until it has
            // that answer. The timeline shows the request and its
            // withdrawal, as for one that was pending at the cancel.
            self.withdraw_permissions().await;
        }
    }
}
