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

use vessel_proto::{DeviceId, ErrorCode, Method, NodeId, Scope, SessionId, WireError};

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
}

impl Principal {
    /// Whether this principal may call `method`; `forbidden` if not.
    pub(crate) fn permit(&self, method: Method) -> Result<(), WireError> {
        let needed = method.scope();
        let (who, scopes) = match self {
            Self::Owner => return Ok(()),
            Self::Device { scopes, .. } => ("device", scopes),
            Self::Agent { scopes, .. } => ("agent", scopes),
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
