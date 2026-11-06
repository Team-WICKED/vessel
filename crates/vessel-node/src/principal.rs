//! Who is calling on a connection, and what it may do (ADR 0010).
//!
//! [Decision Log]
//! - Purpose: a paired device is checked against the scope each method
//!   declares; the machine's own clients are not limited.
//! - Choice: the principal is fixed when the connection is admitted (the
//!   token on `/rpc`, the token preamble on the local socket, the device
//!   preamble on a secure channel) and travels in the connection's context,
//!   so every call is checked against it without a lookup. Revoking a
//!   device does not edit principals: it closes the device's connections
//!   (`devices::Connected`), which is simpler to get right than revoking
//!   authority mid-connection.
//! - A call delivered from another node of the fleet (protocol 6, ADR 0013)
//!   runs as a [`Principal::Delegated`]: the device that delivered it, with
//!   its scopes cut down to the origin's, so a target never grants more
//!   than its owner granted the home, and an agent never more than its own.
//!   Ownership and the agent limits apply to the origin (`origin_agent`).

use vessel_proto::{
    DeviceId, ErrorCode, Method, NodeId, Origin, OriginBy, Scope, SessionId, WireError,
};

/// The caller behind one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Principal {
    /// A holder of the node token (`/rpc`) or a client of the local socket
    /// (behind SSH): the machine's own clients, with every scope (ADR 0005,
    /// ADR 0010).
    Owner,
    /// A paired device with the scopes it was granted.
    Device {
        /// The device.
        id: DeviceId,
        /// What it may do.
        scopes: Vec<Scope>,
    },
    /// A session's agent, through `vessel mcp` with its session's
    /// credential (ADR 0013): its creator's scopes, at most `read` and
    /// `operate`, and `operate` on a session only where the agent started
    /// it or one of its ancestors (checked by the service).
    Agent {
        /// The node whose session it is.
        node: NodeId,
        /// Its session.
        session: SessionId,
        /// What it may do.
        scopes: Vec<Scope>,
    },
    /// A call another node of the fleet asked for, delivered by `device`:
    /// the home's device on a member, or a relaying member's device on the
    /// home.
    Delegated {
        /// The delivering device.
        device: DeviceId,
        /// Its scopes here, intersected with `origin.scopes`.
        scopes: Vec<Scope>,
        /// Who the call speaks for.
        origin: Origin,
        /// The home it came through.
        via: NodeId,
    },
}

impl Principal {
    /// Whether this principal may call `method`; `forbidden` if not.
    pub(crate) fn permit(&self, method: Method) -> Result<(), WireError> {
        let needed = method.scope();
        let (who, scopes) = match self {
            Self::Owner => return Ok(()),
            Self::Device { scopes, .. } => ("device", scopes),
            Self::Agent { scopes, .. } => ("agent", scopes),
            Self::Delegated { scopes, .. } => ("call from another node", scopes),
        };
        if scopes.contains(&needed) {
            return Ok(());
        }
        Err(WireError::new(
            ErrorCode::Forbidden,
            format!(
                "this {who} may not call `{}` (it needs the `{}` scope)",
                method.as_str(),
                needed.as_str()
            ),
        ))
    }
}

impl Principal {
    /// A call delivered by `device` (holding `granted` here) for `origin`,
    /// which came through the home `via`.
    pub(crate) fn delegated(
        device: DeviceId,
        granted: &[Scope],
        origin: Origin,
        via: NodeId,
    ) -> Self {
        let scopes = granted
            .iter()
            .copied()
            .filter(|scope| origin.scopes.contains(scope))
            .collect();
        Self::Delegated {
            device,
            scopes,
            origin,
            via,
        }
    }

    /// The agent this principal acts for, as `(its node, its session)`, so
    /// ownership and the agent limits apply to it: a local agent, or an
    /// agent of another node whose call was delivered here.
    pub(crate) const fn agent(&self) -> Option<(NodeId, SessionId)> {
        match self {
            Self::Agent { node, session, .. }
            | Self::Delegated {
                origin:
                    Origin {
                        node,
                        by: OriginBy::Agent { session, .. },
                        ..
                    },
                ..
            } => Some((*node, *session)),
            Self::Owner | Self::Device { .. } | Self::Delegated { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_are_held_to_their_scopes_and_owners_are_not() {
        let viewer = Principal::Device {
            id: DeviceId::random(),
            scopes: vec![Scope::Read],
        };
        assert!(viewer.permit(Method::ListSessions).is_ok());
        let refused = viewer.permit(Method::CreateSession).unwrap_err();
        assert_eq!(refused.code, ErrorCode::Forbidden);
        assert!(refused.message.contains("`operate`"), "{}", refused.message);
        assert!(viewer.permit(Method::RevokeDevice).is_err());
        for method in Method::ALL {
            assert!(Principal::Owner.permit(*method).is_ok());
        }
    }

    /// A delivered call holds what both the delivering device and the
    /// origin hold, and acts for the origin's agent.
    #[test]
    fn a_delivered_call_holds_the_intersection() {
        let session = SessionId::random();
        let origin = Origin {
            node: NodeId::random(),
            by: OriginBy::Agent { session, depth: 1 },
            scopes: vec![Scope::Read, Scope::Operate],
        };
        let home = Principal::delegated(
            DeviceId::random(),
            &[Scope::Read, Scope::Admin],
            origin.clone(),
            NodeId::random(),
        );
        assert!(home.permit(Method::ListSessions).is_ok());
        // The home may not operate here, and the origin may not administer.
        for refused in [Method::CreateSession, Method::ListDevices] {
            let refused = home.permit(refused).unwrap_err();
            assert_eq!(refused.code, ErrorCode::Forbidden);
        }
        assert_eq!(home.agent(), Some((origin.node, session)));
        let owner = Principal::delegated(
            DeviceId::random(),
            &[Scope::Read],
            Origin {
                by: OriginBy::Owner,
                ..origin
            },
            NodeId::random(),
        );
        assert_eq!(owner.agent(), None);
    }

    #[test]
    fn agents_are_held_to_their_scopes() {
        let agent = Principal::Agent {
            node: NodeId::random(),
            session: SessionId::random(),
            scopes: vec![Scope::Read, Scope::Operate],
        };
        assert!(agent.permit(Method::CreateSession).is_ok());
        let refused = agent.permit(Method::StopNode).unwrap_err();
        assert_eq!(refused.code, ErrorCode::Forbidden);
        assert!(refused.message.contains("agent"), "{}", refused.message);
    }
}
