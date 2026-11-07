//! Naming a session on the command line: its full id, or any prefix of it
//! that matches exactly one session (ids are long; the first few characters
//! are usually enough, as with git commits).

use std::str::FromStr;

use vessel_client::NodeClient;
use vessel_proto::{SessionId, SessionInfo};

use crate::error::CliError;

/// A full session id or a prefix of one, as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSelector(String);

/// Text that cannot be (part of) a session id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a session id (or its prefix) is made of hex digits and dashes")]
pub(crate) struct InvalidSelector;

impl FromStr for SessionSelector {
    type Err = InvalidSelector;

    fn from_str(text: &str) -> Result<Self, InvalidSelector> {
        let text = text.trim().to_ascii_lowercase();
        let valid = !text.is_empty()
            && text
                .chars()
                .all(|character| character.is_ascii_hexdigit() || character == '-');
        if valid {
            Ok(Self(text))
        } else {
            Err(InvalidSelector)
        }
    }
}

impl SessionSelector {
    /// The session this names on `client`'s node.
    pub(crate) async fn resolve(&self, client: &NodeClient) -> Result<SessionInfo, CliError> {
        if let Ok(id) = self.0.parse::<SessionId>() {
            return Ok(client.session(id).await?);
        }
        let mut matching = client
            .sessions()
            .await?
            .into_iter()
            .filter(|session| session.id.to_string().starts_with(&self.0));
        match (matching.next(), matching.next()) {
            (None, _) => Err(CliError::NoSuchSession(self.0.clone())),
            (Some(session), None) => Ok(session),
            (Some(_), Some(_)) => Err(CliError::AmbiguousSession {
                prefix: self.0.clone(),
                matches: matching.count().saturating_add(2),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ids_and_prefixes_only() {
        assert_eq!(
            " 3F2a ".parse::<SessionSelector>(),
            Ok(SessionSelector("3f2a".to_owned()))
        );
        assert!(
            "00000000-0000-4000-8000-000000000001"
                .parse::<SessionSelector>()
                .is_ok()
        );
        assert!("".parse::<SessionSelector>().is_err());
        assert!("nope".parse::<SessionSelector>().is_err());
    }
}
