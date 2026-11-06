//! Secrets on the wire (protocol 6, plan F3): set, list, delete and copy a
//! node's secrets, never read one back.
//!
//! [Decision Log]
//! - Purpose: an MCP server definition names a secret, and each node that
//!   launches it needs the value in its own store; the person should not
//!   have to log in to every node to type it.
//! - Constraint: a value leaves a node only into another node's store, and
//!   nothing logs it (`vessel-rpc` logs method names, never parameters).
//! - Alternatives: a `secret.get` (every admin client becomes a way to read
//!   secrets); a value sealed to the target's key (a second Noise pattern,
//!   against ADR 0009's one mode, to hide it from a home that can already
//!   call the target with its own credential).
//! - Choice: `secret.copy` makes the source send `secret.set` to the target
//!   itself, through `fleet.forward`. The target's `admin` check applies to
//!   the delivering grant, so copying needs the target's owner to have
//!   granted the home `admin` there, explicitly (`vessel fleet invite`
//!   never grants it by default).
//! - [`SecretValue`] is serialisable, unlike `ResolvedValue` (it must cross
//!   the wire inside `secret.set`), so it is the one type here that holds a
//!   value: its `Debug` is redacted and it has no `Display`, so neither a
//!   log line nor a format string can show it by accident.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::name::SecretName;

/// A secret's value, on its way into a node's store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wrap a value.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    /// The value, for the store only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// `secret.set` (protocol 6): store a value in this node's `secrets.toml`,
/// replacing one of the same name. Needs `admin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSecret {
    /// Its name.
    pub name: SecretName,
    /// Its value.
    pub value: SecretValue,
}

/// `secret.list` (protocol 6): the names of this node's secrets, never
/// their values. Needs `admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSecrets;

/// `secret.delete` (protocol 6): remove a secret. Needs `admin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSecret {
    /// Which secret.
    pub name: SecretName,
}

/// `secret.copy` (protocol 6): send this node's value for `name` to fleet
/// node `to`, which stores it under the same name. Needs `admin` here, and
/// the delivering grant on `to` must hold `admin` too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySecret {
    /// Which secret.
    pub name: SecretName,
    /// The node to copy it to.
    pub to: NodeId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_travels_but_never_shows() {
        let set = SetSecret {
            name: "DOCS_TOKEN".parse().unwrap(),
            value: SecretValue::new("value-7f3".to_owned()),
        };
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"{"name":"DOCS_TOKEN","value":"value-7f3"}"#);
        assert_eq!(serde_json::from_str::<SetSecret>(&json).unwrap(), set);
        let shown = format!("{set:?}");
        assert!(!shown.contains("value-7f3"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }
}
