//! Pending permission requests: the first answer wins.

use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;
use vessel_proto::{PermissionId, PermissionOption, PermissionOptionId, PermissionOutcome};

use crate::error::InputError;
use crate::mapper;
use crate::recent::Recent;

/// The permission requests of one session.
///
/// Each request gets a node-minted [`PermissionId`] so many clients can
/// answer it; the agent's responder is held until the first answer, a turn
/// cancellation, the end of the turn, or the end of the connection. The
/// most recently settled ids are remembered so a late answer is told
/// "already settled", not "never existed".
#[derive(Debug, Default)]
pub(crate) struct Permissions {
    /// In arrival order, so cancellation resolves them in a stable order.
    /// A handful at most, so a list beats a map.
    pending: Vec<Pending>,
    /// Bounded: a long session must not keep every id it ever settled.
    settled: Recent<PermissionId>,
}

#[derive(Debug)]
struct Pending {
    id: PermissionId,
    options: Vec<PermissionOptionId>,
    responder: Responder<acp::RequestPermissionResponse>,
}

impl Permissions {
    /// Hold a new request; returns its id.
    pub(crate) fn open(
        &mut self,
        responder: Responder<acp::RequestPermissionResponse>,
        options: &[PermissionOption],
    ) -> PermissionId {
        let id = PermissionId::random();
        self.pending.push(Pending {
            id,
            options: options.iter().map(|option| option.id.clone()).collect(),
            responder,
        });
        id
    }

    /// Answer request `id` with `outcome`.
    pub(crate) fn answer(
        &mut self,
        id: PermissionId,
        outcome: &PermissionOutcome,
    ) -> Result<(), InputError> {
        let Some(index) = self.pending.iter().position(|pending| pending.id == id) else {
            return Err(if self.settled.contains(&id) {
                InputError::PermissionSettled(id)
            } else {
                InputError::PermissionNotFound(id)
            });
        };
        if let PermissionOutcome::Selected { option } = outcome
            && self
                .pending
                .get(index)
                .is_some_and(|pending| !pending.options.contains(option))
        {
            return Err(InputError::UnknownOption {
                id,
                option: option.clone(),
            });
        }
        let pending = self.pending.remove(index);
        respond(pending.responder, outcome);
        let _ = self.settled.insert(id);
        Ok(())
    }

    /// Withdraw every pending request (as `Cancelled`); returns their ids in
    /// arrival order.
    pub(crate) fn cancel_all(&mut self) -> Vec<PermissionId> {
        let ids = self
            .pending
            .drain(..)
            .map(|pending| {
                respond(pending.responder, &PermissionOutcome::Cancelled);
                pending.id
            })
            .collect::<Vec<_>>();
        for id in &ids {
            let _ = self.settled.insert(*id);
        }
        ids
    }
}

fn respond(responder: Responder<acp::RequestPermissionResponse>, outcome: &PermissionOutcome) {
    let response = acp::RequestPermissionResponse::new(mapper::permission_outcome(outcome));
    // Fails only when the connection is already gone; the turn ends anyway.
    if let Err(error) = responder.respond(response) {
        tracing::debug!(%error, "permission answer not delivered");
    }
}
