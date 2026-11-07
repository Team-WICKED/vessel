//! The end of the connection: settling the turn and the permission
//! requests it leaves open.

use vessel_proto::{AgentEvent, PermissionOutcome};

use super::{AFTER_CLOSE, Driver};

impl Driver {
    /// The connection closed: handle what is still queued, then settle
    /// what is still open.
    ///
    /// The SDK fails the requests still waiting before it reports the
    /// close, but a prompt's outcome reaches `inbound` through the prompt's
    /// own task, which may not have run yet. Sweeping only what is queued
    /// would then record a failure for a turn whose outcome (possibly
    /// "ended normally", if the agent answered and then exited) is on its
    /// way. So while a turn is open the loop keeps taking items, each
    /// within [`AFTER_CLOSE`], until the outcome arrives.
    pub(super) async fn settle_after_close(&mut self) {
        while self.turn {
            let Ok(Some(item)) =
                tokio::time::timeout(AFTER_CLOSE, self.inbound_receiver.recv()).await
            else {
                tracing::debug!("no turn outcome after the connection closed");
                break;
            };
            if self.handle(item).await.is_some() {
                break;
            }
        }
        while let Ok(item) = self.inbound_receiver.try_recv() {
            if self.handle(item).await.is_some() {
                break;
            }
        }
        self.abandon_turn().await;
    }

    /// The connection ended: settle what is still open.
    pub(super) async fn abandon_turn(&mut self) {
        self.withdraw_permissions().await;
        self.cancelling = false;
        if self.turn {
            self.turn = false;
            self.emit(AgentEvent::TurnFailed {
                message: String::from("the agent exited during the turn"),
            })
            .await;
        }
    }

    pub(super) async fn withdraw_permissions(&mut self) {
        for id in self.permissions.cancel_all() {
            self.emit(AgentEvent::PermissionResolved {
                id,
                outcome: PermissionOutcome::Cancelled,
            })
            .await;
        }
    }
}
