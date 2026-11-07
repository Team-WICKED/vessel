//! What `vessel mcp` reads from its environment: where its node is, and
//! which session's agent it serves, with that session's credential. The
//! node sets them in the `vessel` server's definition (`vessel-node`,
//! `profiles/launch.rs`); the names there must match these (the
//! end-to-end test runs through both).

use std::ffi::OsString;
use std::path::PathBuf;

use vessel_proto::AgentAuth;

/// The node's local socket path, or its pipe name on Windows.
pub const SOCKET_VARIABLE: &str = "VESSEL_NODE_SOCKET";
/// The session whose agent this is.
pub const SESSION_VARIABLE: &str = "VESSEL_AGENT_SESSION";
/// That session's credential (`SECRET` in the name keeps Codex from
/// passing it to the commands its agent runs).
pub const SECRET_VARIABLE: &str = "VESSEL_AGENT_SECRET";

/// Where the node is and who this is.
#[derive(Debug, Clone)]
pub struct AgentEnv {
    /// The node's local endpoint.
    pub socket: PathBuf,
    /// The session and its credential.
    pub auth: AgentAuth,
}

/// Why the environment cannot be used. Never quotes a value.
#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    /// A variable is not set: not started by a node.
    #[error(
        "{0} is not set: `vessel mcp` runs as a session's `vessel` MCP server, started by its node"
    )]
    Missing(&'static str),
    /// A variable is set to something unusable.
    #[error("{0} is not valid")]
    Invalid(&'static str),
}

impl AgentEnv {
    /// From this process's environment.
    ///
    /// # Errors
    /// See [`EnvError`].
    pub fn from_env() -> Result<Self, EnvError> {
        Self::read(|name| std::env::var_os(name))
    }

    /// From `get`, which looks a variable up.
    ///
    /// # Errors
    /// See [`EnvError`].
    pub fn read(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, EnvError> {
        let value = |name: &'static str| get(name).ok_or(EnvError::Missing(name));
        let text = |name: &'static str| {
            value(name)?
                .into_string()
                .map_err(|_not_unicode| EnvError::Invalid(name))
        };
        let socket = PathBuf::from(value(SOCKET_VARIABLE)?);
        let session = text(SESSION_VARIABLE)?
            .parse()
            .map_err(|_invalid| EnvError::Invalid(SESSION_VARIABLE))?;
        let secret = text(SECRET_VARIABLE)?
            .parse()
            .map_err(|_invalid| EnvError::Invalid(SECRET_VARIABLE))?;
        Ok(Self {
            socket,
            auth: AgentAuth { session, secret },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use vessel_proto::{AgentSecret, SessionId};

    use super::*;

    #[test]
    fn the_environment_names_the_node_and_the_session_and_errors_hide_values() {
        let session = SessionId::random();
        let secret = AgentSecret::from_bytes([9; 32]);
        let mut vars = HashMap::from([
            (SOCKET_VARIABLE, OsString::from("/run/node.sock")),
            (SESSION_VARIABLE, OsString::from(session.to_string())),
            (SECRET_VARIABLE, OsString::from(secret.to_string())),
        ]);
        let env = AgentEnv::read(|name| vars.get(name).cloned()).unwrap();
        assert_eq!(env.socket, PathBuf::from("/run/node.sock"));
        assert_eq!(env.auth.session, session);
        assert_eq!(env.auth.secret, secret);

        let _old = vars.insert(SECRET_VARIABLE, OsString::from("not-a-secret-value"));
        let error = AgentEnv::read(|name| vars.get(name).cloned()).unwrap_err();
        assert!(!error.to_string().contains("not-a-secret-value"), "{error}");
        let _gone = vars.remove(SOCKET_VARIABLE);
        assert!(matches!(
            AgentEnv::read(|name| vars.get(name).cloned()),
            Err(EnvError::Missing(SOCKET_VARIABLE))
        ));
    }
}
