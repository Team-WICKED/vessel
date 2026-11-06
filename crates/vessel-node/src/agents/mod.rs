//! Agent principals (ADR 0013): every session's agent reaches its node
//! through `vessel mcp` with a credential of its own, and may do what the
//! session's creator could, on the sessions it started.
//!
//! [Decision Log]
//! - Purpose: an agent inside a session lists, starts, drives and stops
//!   sessions, and every call says which session's agent made it, so the
//!   node can attribute it, hold it to its creator's grant and limit it.
//! - Choice: one 256-bit credential per session, minted when the session
//!   is created and kept here only as its hash (`CredentialHash`), in
//!   memory: a restart voids it with the sessions it interrupts. The
//!   secret goes only into the `vessel` MCP server's definition (see
//!   `profiles::launch`). `vessel mcp` presents it as the local socket's
//!   first frame (`AgentAuth`).
//! - Grants only shrink: a session's agent gets the scopes of whoever
//!   created the session, at most `read` and `operate` (never `admin`);
//!   a session an agent creates gets that agent's.
//! - Limits, counted here under one lock so two concurrent creations
//!   cannot both take the last place: an agent owns at most
//!   [`MAX_OWNED`] live sessions it created itself, and agents start
//!   agents at most [`MAX_DEPTH`] levels deep (a user's session is level
//!   0). Every live session has an entry, with a credential or without one
//!   (a node without `vessel mcp`), so depth and counts hold either way.
//!   A session frees its place as soon as its end is visible
//!   (`SessionHandle::has_ended`, set before the final status is
//!   published): CI once saw a client watch a session end, then be refused
//!   a new one because `retire` (after the actor finishes) had not run yet.
//! - The credential dies with its session: it is refused once the end is
//!   visible (`has_ended`, as for the limits), and [`Agents::retire`]
//!   removes the entry and closes the agent's open connections, as revoking a device
//!   closes the device's (`devices::Connected`). Checking a credential and
//!   registering its connection happen under the same lock, so a
//!   connection is either refused or found and closed.
//! - A profile move restarts the agent, and with it `vessel mcp`: the move
//!   gets a second credential, valid beside the first until the move ends;
//!   then the one in use stays (the new one, or the old one if the session
//!   went back to its previous agent).
//! - Ownership (operate calls on a session need the session to descend
//!   from the calling agent's) is checked by the service, which can read
//!   ended sessions from the store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio_util::sync::CancellationToken;
use vessel_proto::{AgentAuth, AgentSecret, NodeId, Origin, OriginBy, Scope, SessionId};
use vessel_session::SessionHandle;
use vessel_store::CredentialHash;

use crate::error::ServiceError;
use crate::principal::Principal;

/// Live sessions an agent may own (that it created itself) at once.
pub(crate) const MAX_OWNED: usize = 8;

/// How deep agents may start agents: a user's session is level 0, a
/// session its agent starts level 1, and so on.
pub(crate) const MAX_DEPTH: u8 = 3;

/// What an agent may do at most, whoever created its session.
const AGENT_SCOPES: [Scope; 2] = [Scope::Read, Scope::Operate];

/// Every live session's agent: its credentials, grant and connections.
#[derive(Debug)]
pub(crate) struct Agents {
    node: NodeId,
    next: AtomicU64,
    sessions: Mutex<HashMap<SessionId, Entry>>,
}

#[derive(Debug)]
struct Entry {
    /// Accepted credentials: one, or two while a profile move runs.
    credentials: Vec<CredentialHash>,
    /// What the session's agent may do.
    scopes: Vec<Scope>,
    /// 0 for a session a user created.
    depth: u8,
    /// The agent session that created this one, with its node: this node,
    /// or another node of the fleet whose agent's call was delivered here.
    parent: Option<(NodeId, SessionId)>,
    /// Open connections of the agent, closed when the session ends.
    connections: HashMap<u64, CancellationToken>,
    /// The session, once it exists: an ended one frees its creator's place
    /// at once, before `retire` (which waits for the actor to finish) runs.
    session: Option<SessionHandle>,
}

impl Agents {
    pub(crate) fn new(node: NodeId) -> Self {
        Self {
            node,
            next: AtomicU64::new(0),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Take a place for the session `id` that `principal` is creating:
    /// its agent's scopes and depth, and, `with_credential`, a credential
    /// for it. Refused past the limits.
    ///
    /// # Errors
    /// The calling agent owns [`MAX_OWNED`] live sessions already, its
    /// session is [`MAX_DEPTH`] levels deep, or it is gone; or no random
    /// bytes for the credential.
    pub(crate) fn reserve(
        &self,
        id: SessionId,
        principal: &Principal,
        with_credential: bool,
    ) -> Result<Option<AgentSecret>, ServiceError> {
        let (secret, credentials) = if with_credential {
            let secret = mint()?;
            let hash = CredentialHash::of_agent(&secret);
            (Some(secret), vec![hash])
        } else {
            (None, Vec::new())
        };
        let mut sessions = self.lock();
        let (scopes, depth, parent) = match principal {
            Principal::Owner => (AGENT_SCOPES.to_vec(), 0, None),
            Principal::Agent {
                node,
                session,
                scopes,
            } => {
                let creator = sessions
                    .get(session)
                    .ok_or(ServiceError::AgentGone(*session))?;
                let parent = (*node, *session);
                let depth = deeper(creator.depth)?;
                owns_fewer_than_the_limit(&sessions, parent)?;
                (scopes.clone(), depth, Some(parent))
            }
            // Another node's agent: its depth comes with the call (its own
            // node vouches for it, and the home for that node), and what it
            // owns here is counted here.
            Principal::Delegated {
                scopes,
                origin:
                    Origin {
                        node,
                        by: OriginBy::Agent { session, depth },
                        ..
                    },
                ..
            } => {
                let parent = (*node, *session);
                let depth = deeper(*depth)?;
                owns_fewer_than_the_limit(&sessions, parent)?;
                (within_agent_scopes(scopes), depth, Some(parent))
            }
            Principal::Device { scopes, .. } | Principal::Delegated { scopes, .. } => {
                (within_agent_scopes(scopes), 0, None)
            }
        };
        let previous = sessions.insert(
            id,
            Entry {
                credentials,
                scopes,
                depth,
                parent,
                connections: HashMap::new(),
                session: None,
            },
        );
        if previous.is_some() {
            tracing::error!(session = %id, "an agent place was reserved twice");
        }
        Ok(secret)
    }

    /// How many agents deep a live session is (0 for a user's), for the
    /// `Origin` of a call its agent sends to another node.
    pub(crate) fn depth(&self, id: SessionId) -> Option<u8> {
        self.lock().get(&id).map(|entry| entry.depth)
    }

    /// The session `id` reserved a place for has started as `handle`.
    pub(crate) fn started(&self, id: SessionId, handle: SessionHandle) {
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.session = Some(handle);
        }
    }

    /// Forget a session that did not start.
    pub(crate) fn forget(&self, id: SessionId) {
        drop(self.lock().remove(&id));
    }

    /// The session ended: its credential is refused from now on, and its
    /// agent's open connections close.
    pub(crate) fn retire(&self, id: SessionId) {
        let Some(entry) = self.lock().remove(&id) else {
            return;
        };
        for closing in entry.connections.values() {
            closing.cancel();
        }
        if !entry.connections.is_empty() {
            tracing::info!(session = %id, connections = entry.connections.len(), "the session ended; closed its agent's connections");
        }
    }

    /// Check a presented credential. Admitted, the connection is
    /// registered (its token a child of `parent`, the node's closing) and
    /// closes when the session ends.
    pub(crate) fn admit(
        self: &Arc<Self>,
        auth: &AgentAuth,
        parent: &CancellationToken,
    ) -> Option<(Principal, Registration)> {
        let mut sessions = self.lock();
        let entry = sessions.get_mut(&auth.session)?;
        // Refused as soon as the end is visible, not only after `retire`:
        // a client that saw the session end must not get its agent in
        // (macOS CI admitted one in that gap). A connection admitted just
        // before the end is still closed by `retire`.
        if entry.session.as_ref().is_some_and(SessionHandle::has_ended)
            || !entry
                .credentials
                .iter()
                .any(|hash| hash.matches_agent(&auth.secret))
        {
            return None;
        }
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let closing = parent.child_token();
        let _none = entry.connections.insert(key, closing.clone());
        let principal = Principal::Agent {
            node: self.node,
            session: auth.session,
            scopes: entry.scopes.clone(),
        };
        Some((
            principal,
            Registration {
                agents: Arc::clone(self),
                session: auth.session,
                key,
                closing,
            },
        ))
    }

    /// A profile move of `id` begins: a second credential, for the agent
    /// it starts. `None` when the session has no credential.
    ///
    /// # Errors
    /// Another move of the session is running, or no random bytes.
    pub(crate) fn begin_move(&self, id: SessionId) -> Result<Option<AgentSecret>, ServiceError> {
        let mut sessions = self.lock();
        let Some(entry) = sessions.get_mut(&id) else {
            return Ok(None);
        };
        match entry.credentials.len() {
            0 => Ok(None),
            1 => {
                let secret = mint()?;
                entry.credentials.push(CredentialHash::of_agent(&secret));
                Ok(Some(secret))
            }
            _ => Err(ServiceError::MoveRunning(id)),
        }
    }

    /// The move begun with [`begin_move`](Self::begin_move) ended: keep
    /// the new credential if the session moved, the old one if not. A
    /// no-op when no move began.
    pub(crate) fn end_move(&self, id: SessionId, moved: bool) {
        let mut sessions = self.lock();
        let Some(entry) = sessions.get_mut(&id) else {
            return;
        };
        if entry.credentials.len() == 2 {
            let _retired = if moved {
                entry.credentials.remove(0)
            } else {
                entry.credentials.swap_remove(1)
            };
        }
    }

    fn unregister(&self, session: SessionId, key: u64) {
        if let Some(entry) = self.lock().get_mut(&session) {
            drop(entry.connections.remove(&key));
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, Entry>> {
        // Every change is one insert or remove: a poisoned map is whole.
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One open agent connection; unregisters itself when dropped.
#[derive(Debug)]
pub(crate) struct Registration {
    agents: Arc<Agents>,
    session: SessionId,
    key: u64,
    closing: CancellationToken,
}

impl Registration {
    /// Cancelled when the session ends or the node closes.
    pub(crate) const fn closing(&self) -> &CancellationToken {
        &self.closing
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.agents.unregister(self.session, self.key);
    }
}

/// `scopes` limited to what an agent may have.
/// The depth of a session an agent at `depth` starts, within the limit.
fn deeper(depth: u8) -> Result<u8, ServiceError> {
    depth
        .checked_add(1)
        .filter(|depth| *depth <= MAX_DEPTH)
        .ok_or(ServiceError::AgentDepth { limit: MAX_DEPTH })
}

/// The agent `parent` may start another session.
fn owns_fewer_than_the_limit(
    sessions: &HashMap<SessionId, Entry>,
    parent: (NodeId, SessionId),
) -> Result<(), ServiceError> {
    // A session counts until it has ended: visibly ended is ended here too
    // (`SessionHandle::has_ended`), even before `retire` removes it.
    let owned = sessions
        .values()
        .filter(|entry| entry.parent == Some(parent))
        .filter(|entry| !entry.session.as_ref().is_some_and(SessionHandle::has_ended))
        .count();
    if owned >= MAX_OWNED {
        return Err(ServiceError::AgentLimit { limit: MAX_OWNED });
    }
    Ok(())
}

fn within_agent_scopes(scopes: &[Scope]) -> Vec<Scope> {
    AGENT_SCOPES
        .into_iter()
        .filter(|scope| scopes.contains(scope))
        .collect()
}

/// A fresh credential.
fn mint() -> Result<AgentSecret, ServiceError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(ServiceError::Random)?;
    Ok(AgentSecret::from_bytes(bytes))
}

#[cfg(test)]
mod tests;
