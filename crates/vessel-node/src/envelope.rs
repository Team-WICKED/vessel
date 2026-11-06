//! Calls across the fleet (protocol 6, Track X, ADR 0013 "across nodes
//! through the home"): `fleet.forward`, `fleet.relay` and `fleet.deliver`,
//! and the fleet's `grant`, `peers` and `nodes`.
//!
//! ```text
//!  caller ─forward─▶ its node ─relay─▶ home ─deliver─▶ target
//!                    (checks the       (checks the     (runs it as the
//!                     caller, builds    member speaks   delivering device
//!                     the origin)       for itself)     ∩ the origin)
//! ```
//!
//! [Decision Log]
//! - Purpose: an agent or a person's client reaches only its own node, yet
//!   acts on other nodes of its fleet, with no more authority there than
//!   it has at home and no more than the target's owner granted the home.
//! - Each hop checks what it can know: the first node checks the caller
//!   against the inner call's row as if it were local (so nobody gets a
//!   hop for what they may not do) and vouches for the caller in the
//!   `Origin`; the home checks that a relaying member speaks only for its
//!   own node (by the node its grant came from); the target accepts a
//!   delivery only from its home's device and runs the inner call as
//!   `Principal::Delegated`, the delivering device's scopes intersected
//!   with the origin's, with ownership and the agent limits applied to
//!   the origin's agent. Calls the home runs for a member are delegated the
//!   same way, from the member's device on the home.
//! - Running a call here goes through the guarded handlers, so the scope
//!   and ownership checks run again for the delegated principal. The
//!   dispatch below names every forwardable method (`Method::forwardable`,
//!   pinned by a test); the envelope's own are not among them, so a hop is
//!   never wrapped twice and the dispatch cannot recurse.
//! - Alternatives: a `node` field on every request (every type changes,
//!   and the guard would rediscover the intersection per method); trusting
//!   a delivered `Origin` without the home's check (any member could claim
//!   to be another node's agent).
//! - Trade-off: a call to another member takes two hops; a long poll
//!   (`session.read` with `wait`) waits on the target, holding one call on
//!   each hop.

use vessel_proto::{
    Ack, Call, Deliver, ErrorCode, FleetNode, Forward, Forwarded, GrantHome, ListNodes, ListPeers,
    Method, NodeId, Origin, OriginBy, RelayCall, Reply, Scope, WireError,
};

use crate::guard::Guarded;
use crate::principal::Principal;
use crate::service::{Connection, NodeService};
use crate::waits::{WaitSlot, Waiter};

fn forbidden(message: &str) -> WireError {
    WireError::new(ErrorCode::Forbidden, message.to_owned())
}

fn not_forwardable(method: Method) -> WireError {
    WireError::new(
        ErrorCode::InvalidRequest,
        format!(
            "`{}` cannot be sent to another node (only work on sessions, profiles, \
             transfers and secret.set can)",
            method.as_str()
        ),
    )
}

fn forwardable(method: Method) -> Result<(), WireError> {
    if method.forwardable() {
        Ok(())
    } else {
        Err(not_forwardable(method))
    }
}

impl NodeService {
    /// `fleet.forward`: check the call against the caller, then run it here
    /// or pass it on with the caller's origin.
    pub(crate) async fn forward(
        &self,
        connection: &Connection,
        request: Forward,
        guard: &Guarded,
    ) -> Result<Forwarded, WireError> {
        let Forward { node, mut call } = request;
        if node == self.links.node() {
            forwardable(call.method())?;
            connection.principal().permit(call.method())?;
            return forwarded(run_here(guard, connection, *call).await);
        }
        let waiter = Waiter::of(connection.principal(), self.links.node());
        let _slot = self.passed_wait(connection, waiter, &mut call);
        self.pass_on(connection.principal(), node, call).await
    }

    /// Run `call` on fleet node `node` (another node) for `principal`: the
    /// route `fleet.forward` takes, checks included (forwardable, the
    /// caller's own scope for it). For node code that acts on another node
    /// for its caller, such as an import pulling a transfer (F4): the
    /// source node sees the caller's origin, so its ownership rules and
    /// its grant to the home apply there.
    pub(crate) async fn pass_on(
        &self,
        principal: &Principal,
        node: NodeId,
        call: Box<Call>,
    ) -> Result<Forwarded, WireError> {
        forwardable(call.method())?;
        principal.permit(call.method())?;
        let origin = self.origin_of(principal)?;
        if self.links.is_home() {
            self.links.call_member(node, Deliver { call, origin }).await
        } else if self.links.is_member() {
            self.links.call_home(RelayCall { node, call, origin }).await
        } else {
            Err(WireError::new(
                ErrorCode::NotFound,
                format!("this node is in no fleet, so it cannot reach node {node}"),
            ))
        }
    }

    /// `fleet.relay` (the home): pass a member's call on, or run it here.
    pub(crate) async fn relay(
        &self,
        connection: &Connection,
        request: RelayCall,
        guard: &Guarded,
    ) -> Result<Forwarded, WireError> {
        let RelayCall { node, call, origin } = request;
        forwardable(call.method())?;
        let Principal::Device { id, scopes } = connection.principal() else {
            return Err(forbidden("only a fleet member's device relays calls"));
        };
        if !self.links.is_home() {
            return Err(WireError::new(
                ErrorCode::Unavailable,
                "this node is not a fleet's home".to_owned(),
            ));
        }
        let grant = self.links.grant_of(*id).await.ok_or_else(|| {
            forbidden(
                "this member has not granted the home access yet (its node does when it \
                 connects, if it joined with `--grant`)",
            )
        })?;
        if origin.node != grant.node {
            return Err(forbidden("a member relays only calls of its own node"));
        }
        if node == self.links.node() {
            let delegated = Principal::delegated(*id, scopes, origin, self.links.node());
            return forwarded(run_here(guard, &connection.acting_as(delegated), *call).await);
        }
        let mut call = call;
        let _slot = self.passed_wait(connection, Waiter::origin(&origin), &mut call);
        self.links.call_member(node, Deliver { call, origin }).await
    }

    /// `fleet.deliver` (a member): run a call the home delivered, as the
    /// home's device cut down to the origin.
    pub(crate) async fn deliver(
        &self,
        connection: &Connection,
        request: Deliver,
        guard: &Guarded,
    ) -> Result<Forwarded, WireError> {
        let Deliver { call, origin } = request;
        forwardable(call.method())?;
        let Principal::Device { id, scopes } = connection.principal() else {
            return Err(forbidden("only this node's fleet home delivers calls here"));
        };
        let Some(home) = self.links.home().await else {
            return Err(forbidden("this node is not a member of a fleet"));
        };
        if self.links.home_device(home).await != Some(*id) {
            return Err(forbidden("only this node's fleet home delivers calls here"));
        }
        let delegated = Principal::delegated(*id, scopes, origin, home);
        forwarded(run_here(guard, &connection.acting_as(delegated), *call).await)
    }

    /// A waiting read passed on to another node holds a call of this
    /// connection until it is answered: within this connection's and the
    /// waiter's share (`waits`), or it goes without its wait.
    fn passed_wait(
        &self,
        connection: &Connection,
        waiter: Waiter,
        call: &mut Call,
    ) -> Option<WaitSlot> {
        if let Call::ReadEvents(read) = call {
            self.wait_slot(connection, waiter, &mut read.wait)
        } else {
            None
        }
    }

    /// Who a caller here is, for a call passed on to another node.
    fn origin_of(&self, principal: &Principal) -> Result<Origin, WireError> {
        let node = self.links.node();
        Ok(match principal {
            Principal::Owner => Origin {
                node,
                by: OriginBy::Owner,
                scopes: Scope::ALL.to_vec(),
            },
            Principal::Device { id, scopes } => Origin {
                node,
                by: OriginBy::Device { id: *id },
                scopes: scopes.clone(),
            },
            Principal::Agent {
                session, scopes, ..
            } => Origin {
                node,
                by: OriginBy::Agent {
                    session: *session,
                    depth: self
                        .agents
                        .depth(*session)
                        .ok_or_else(|| forbidden("the agent's session has ended"))?,
                },
                scopes: scopes.clone(),
            },
            Principal::Delegated { .. } => {
                return Err(forbidden(
                    "a call delivered from another node is not passed on again",
                ));
            }
        })
    }

    /// `fleet.grant` (the home): keep a member's credential.
    pub(crate) async fn grant_home(
        &self,
        connection: &Connection,
        request: GrantHome,
    ) -> Result<Ack, WireError> {
        let Principal::Device { id, .. } = connection.principal() else {
            return Err(forbidden(
                "only a fleet member's device grants its home access",
            ));
        };
        self.links
            .accept_grant(*id, &request.link)
            .await
            .map(|()| Ack)
    }

    /// `fleet.peers` (the home).
    pub(crate) async fn list_peers(
        &self,
        _: &Connection,
        _: ListPeers,
    ) -> Result<Vec<FleetNode>, WireError> {
        if !self.links.is_home() {
            return Err(WireError::new(
                ErrorCode::Unavailable,
                "this node is not a fleet's home".to_owned(),
            ));
        }
        Ok(self.links.peers(&self.lifecycle.info().name).await)
    }

    /// `fleet.nodes`: the home's view, from the home itself or asked of
    /// it; a node in no fleet is a fleet of one.
    pub(crate) async fn list_nodes(
        &self,
        _: &Connection,
        _: ListNodes,
    ) -> Result<Vec<FleetNode>, WireError> {
        let me = self.lifecycle.info();
        if self.links.is_home() {
            Ok(self.links.peers(&me.name).await)
        } else if self.links.is_member() {
            self.links.call_home(ListPeers).await
        } else {
            Ok(vec![FleetNode {
                node: me.id,
                name: me.name,
                protocol: vessel_proto::PROTOCOL_VERSION,
                reachable: true,
            }])
        }
    }
}

fn forwarded(reply: Result<Reply, WireError>) -> Result<Forwarded, WireError> {
    reply.map(|reply| Forwarded {
        reply: Box::new(reply),
    })
}

/// Run a forwardable call here through the guard. Every forwardable method
/// has its arm; every other is refused (and was refused before any hop).
async fn run_here(
    guard: &Guarded,
    connection: &Connection,
    call: Call,
) -> Result<Reply, WireError> {
    use vessel_proto::Handler as _;
    Ok(match call {
        Call::NodeInfo(request) => Reply::NodeInfo(guard.node_info(connection, request).await?),
        Call::ListHarnesses(request) => {
            Reply::ListHarnesses(guard.list_harnesses(connection, request).await?)
        }
        Call::ListSessions(request) => {
            Reply::ListSessions(guard.list_sessions(connection, request).await?)
        }
        Call::GetSession(request) => {
            Reply::GetSession(guard.get_session(connection, request).await?)
        }
        Call::CreateSession(request) => {
            Reply::CreateSession(guard.create_session(connection, request).await?)
        }
        Call::SendInput(request) => Reply::SendInput(guard.send_input(connection, request).await?),
        Call::KillSession(request) => {
            Reply::KillSession(guard.kill_session(connection, request).await?)
        }
        Call::ReadEvents(request) => {
            Reply::ReadEvents(guard.read_events(connection, request).await?)
        }
        Call::MoveProfile(request) => {
            Reply::MoveProfile(guard.move_profile(connection, request).await?)
        }
        Call::ListProfiles(request) => {
            Reply::ListProfiles(guard.list_profiles(connection, request).await?)
        }
        Call::ExportSession(request) => {
            Reply::ExportSession(guard.export_session(connection, request).await?)
        }
        Call::ImportSession(request) => {
            Reply::ImportSession(guard.import_session(connection, request).await?)
        }
        Call::ExportMemory(request) => {
            Reply::ExportMemory(guard.export_memory(connection, request).await?)
        }
        Call::ImportMemory(request) => {
            Reply::ImportMemory(guard.import_memory(connection, request).await?)
        }
        Call::ReadTransfer(request) => {
            Reply::ReadTransfer(guard.read_transfer(connection, request).await?)
        }
        Call::CloseTransfer(request) => {
            Reply::CloseTransfer(guard.close_transfer(connection, request).await?)
        }
        Call::SetSecret(request) => Reply::SetSecret(guard.set_secret(connection, request).await?),
        Call::StopNode(_)
        | Call::InviteDevice(_)
        | Call::ListDevices(_)
        | Call::RevokeDevice(_)
        | Call::ReloadProfiles(_)
        | Call::ListSecrets(_)
        | Call::DeleteSecret(_)
        | Call::CopySecret(_)
        | Call::Forward(_)
        | Call::Relay(_)
        | Call::Deliver(_)
        | Call::GrantHome(_)
        | Call::ListPeers(_)
        | Call::ListNodes(_)
        | Call::GetManifest(_)
        | Call::ReadBlob(_)
        | Call::WatchFleet(_)
        | Call::ReportApplied(_)
        | Call::ListMembers(_)
        | Call::Subscribe(_)
        | Call::Unsubscribe(_) => return Err(not_forwardable(call.method())),
    })
}
