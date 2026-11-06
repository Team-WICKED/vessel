//! Harness profiles, session origins and agent credentials (ADRs 0011 and
//! 0013, protocol 5).
//!
//! These are the first protocol 5 types. They are added before the fields
//! and methods that use them so milestone 4 Track H can build against them
//! while milestone 3 finishes; `SessionSpec.profile`, `SessionInfo.profile`
//! and `created_by`, and the `profile.*` methods follow in the next contract
//! step (docs/plans/backend-m4.md, "Contract as built").

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::harness::HarnessKind;
use crate::id::{DeviceId, NodeId, SessionId};
use crate::key::{secret, serde_via_text};

/// A profile's name: what `--profile` and `profiles.toml` use.
///
/// Letters, digits, `.`, `_` and `-`, 1 to 64 characters, so it is safe in
/// file names, command lines and TOML keys without quoting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileName(String);

/// A profile name with characters outside the allowed set, or of the wrong
/// length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidProfileName(pub String);

impl fmt::Display for InvalidProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a profile name (1-64 letters, digits, `.`, `_`, `-`)",
            self.0
        )
    }
}

impl std::error::Error for InvalidProfileName {}

impl ProfileName {
    /// Longest allowed name.
    pub const MAX_LEN: usize = 64;

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ProfileName {
    type Err = InvalidProfileName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = !s.is_empty()
            && s.len() <= Self::MAX_LEN
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidProfileName(s.to_owned()))
        }
    }
}

serde_via_text!(ProfileName);

/// A profile as a node reports it. Names of the variables it sets are
/// listed; their values never leave the node (they may be secrets or paths
/// to credentials).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInfo {
    /// The profile's name.
    pub name: ProfileName,
    /// A label for people (for example the account it logs in as).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The harness it applies to.
    pub harness: HarnessKind,
    /// Names of the environment variables it sets.
    #[serde(default)]
    pub sets: Vec<String>,
}

/// Who created a session (ADR 0013): the owner, a paired device, or an agent
/// running in another session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Creator {
    /// The node token or the local socket. Sessions from before protocol 5
    /// are the owner's: nothing else could create them.
    #[default]
    Owner,
    /// A paired device.
    Device {
        /// The device.
        id: DeviceId,
    },
    /// An agent, through its session's credential.
    Agent {
        /// The node the agent's session runs on.
        node: NodeId,
        /// The agent's session.
        session: SessionId,
    },
}

secret! {
    /// An agent's credential for its own node's local socket: minted per
    /// session, given only to that session's `vessel mcp` process (in its
    /// environment), refused once the session ends. 256 random bits.
    AgentSecret, 32
}

/// The first frame an agent's `vessel mcp` sends on the node's local
/// socket, instead of the node token (protocol 5). Text form:
/// `{"type":"agent","session":"<id>","secret":"<base64url>"}`. A token
/// frame is plain text, never a JSON object, so the node tells the two apart
/// by the first character.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "agent")]
pub struct AgentAuth {
    /// The agent's session.
    pub session: SessionId,
    /// That session's credential.
    pub secret: AgentSecret,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_are_plain_words() {
        for good in ["work", "claude-alt", "a.b_c", &"x".repeat(64)] {
            assert_eq!(good.parse::<ProfileName>().unwrap().as_str(), good);
        }
        for bad in ["", "two words", "a/b", "é", &"x".repeat(65)] {
            assert!(bad.parse::<ProfileName>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn creators_default_to_the_owner_and_are_tagged() {
        assert_eq!(Creator::default(), Creator::Owner);
        let agent = Creator::Agent {
            node: NodeId::random(),
            session: SessionId::random(),
        };
        let json = serde_json::to_string(&agent).unwrap();
        assert!(json.starts_with(r#"{"kind":"agent""#));
        assert_eq!(serde_json::from_str::<Creator>(&json).unwrap(), agent);
    }

    #[test]
    fn agent_auth_is_a_tagged_object_and_hides_its_secret() {
        let auth = AgentAuth {
            session: SessionId::random(),
            secret: AgentSecret::from_bytes([4; 32]),
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(json.starts_with(r#"{"type":"agent","#));
        assert_eq!(serde_json::from_str::<AgentAuth>(&json).unwrap(), auth);
        assert!(!format!("{auth:?}").contains(&auth.secret.to_string()));
    }

    #[test]
    fn profile_info_lists_names_not_values() {
        let info = ProfileInfo {
            name: "alt".parse().unwrap(),
            label: Some("help@example".to_owned()),
            harness: HarnessKind::ClaudeCode,
            sets: vec!["CLAUDE_CONFIG_DIR".to_owned()],
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<ProfileInfo>(&json).unwrap(), info);
    }
}
