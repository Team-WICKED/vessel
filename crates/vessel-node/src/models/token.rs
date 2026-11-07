//! Model tokens: what a routed session's harness presents to the node's
//! proxy (ADR 0015, decision 6).
//!
//! [Decision Log]
//! - Purpose: the proxy is on loopback, where any local process can reach
//!   it; a call must say which session makes it, so it gets that session's
//!   route and is recorded in that session's log, and nothing else.
//! - Constraint: the token sits in the harness's environment (Claude Code's
//!   `ANTHROPIC_AUTH_TOKEN`, Codex's provider key variable), so the commands
//!   the agent runs may see it; it must give no more than that session's
//!   model calls, and only while the session lives.
//! - Alternatives: the session's agent credential (ADR 0013; kept out of
//!   the harness's environment on purpose, and it opens the node's
//!   methods); no token on loopback (opencodex: any local process, any
//!   route).
//! - Choice: 256 random bits per session, minted at launch, kept here only
//!   as their SHA-256 and compared against every entry in constant time
//!   (`subtle`); minted before the session is launched, opening nothing
//!   until it has started, refused once its end is visible
//!   (`SessionHandle::has_ended`, as the agent credential is) and removed
//!   when the session is retired or does not start. A restart voids every token with the
//!   sessions it interrupts. The text form is `vslm-` and the bytes in
//!   hexadecimal, so it is told apart in a log by its prefix alone.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use vessel_proto::{ModelRouteName, SessionId};
use vessel_session::SessionHandle;

/// What a token's text starts with.
const PREFIX: &str = "vslm-";

/// A session's model token, as the harness gets it. `Debug` is redacted;
/// only `Display` (for the harness's environment) shows it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ModelToken([u8; 32]);

impl ModelToken {
    /// A fresh token from the operating system's random source.
    ///
    /// # Errors
    /// No random bytes.
    pub(crate) fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// The token `text` spells, if it is one.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().strip_prefix(PREFIX)?;
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let text = std::str::from_utf8(pair).ok()?;
            *byte = u8::from_str_radix(text, 16).ok()?;
        }
        Some(Self(bytes))
    }

    fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }
}

impl fmt::Debug for ModelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ModelToken(<redacted>)")
    }
}

impl fmt::Display for ModelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Why a move's token could not be made.
#[derive(Debug)]
pub(crate) enum MoveTokenError {
    /// Another move of the session is running.
    Running,
    /// No random bytes.
    Random(getrandom::Error),
}

/// A token's session and route.
#[derive(Debug, Clone)]
pub(crate) struct Holder {
    pub(crate) session: SessionHandle,
    pub(crate) route: ModelRouteName,
}

#[derive(Debug)]
struct Entry {
    /// The session's token; two during a profile move (the running agent's
    /// and the one it starts), until the move ends.
    hashes: Vec<[u8; 32]>,
    route: ModelRouteName,
    /// Set once the session has started; until then the token opens
    /// nothing.
    session: Option<SessionHandle>,
}

/// Every routed session's token, by session. A token is minted before its
/// session is launched (it goes into the harness's environment), bound to
/// the session once it has started, and removed when it ends or does not
/// start, as an agent's credential is (`agents`).
#[derive(Debug, Default)]
pub(crate) struct Tokens {
    entries: Mutex<HashMap<SessionId, Entry>>,
}

impl Tokens {
    /// Mint a token for session `id`, about to be launched on `route`. A
    /// second mint for the same session replaces the first.
    ///
    /// # Errors
    /// No random bytes.
    pub(crate) fn mint(
        &self,
        id: SessionId,
        route: ModelRouteName,
    ) -> Result<ModelToken, getrandom::Error> {
        let token = ModelToken::random()?;
        let _replaced = self.lock().insert(
            id,
            Entry {
                hashes: vec![token.hash()],
                route,
                session: None,
            },
        );
        Ok(token)
    }

    /// Session `id` has started: its token opens its model calls.
    pub(crate) fn started(&self, id: SessionId, handle: SessionHandle) {
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.session = Some(handle);
        }
    }

    /// The session and route `presented` belongs to, if it is a started,
    /// live session's token. Every entry is compared, in constant time, so
    /// the time taken says nothing about which, if any, matched.
    pub(crate) fn find(&self, presented: &str) -> Option<Holder> {
        let hash = ModelToken::parse(presented)?.hash();
        let entries = self.lock();
        let mut found = None;
        for entry in entries.values() {
            for known in &entry.hashes {
                if bool::from(known.ct_eq(&hash)) {
                    found = Some(entry);
                }
            }
        }
        let entry = found?;
        let session = entry.session.as_ref()?;
        if session.has_ended() {
            return None;
        }
        Some(Holder {
            session: session.clone(),
            route: entry.route.clone(),
        })
    }

    /// A profile move of `id` begins: a second token, for the agent it
    /// starts (as `Agents::begin_move` does for the agent's credential).
    /// `None` when the session has no token.
    ///
    /// # Errors
    /// Another move of the session is running, or no random bytes.
    pub(crate) fn begin_move(&self, id: SessionId) -> Result<Option<ModelToken>, MoveTokenError> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&id) else {
            return Ok(None);
        };
        if entry.hashes.len() > 1 {
            return Err(MoveTokenError::Running);
        }
        let token = ModelToken::random().map_err(MoveTokenError::Random)?;
        entry.hashes.push(token.hash());
        Ok(Some(token))
    }

    /// The move begun with [`begin_move`](Self::begin_move) ended: keep the
    /// new token if the session moved, the old one if not. A no-op when no
    /// move began.
    pub(crate) fn end_move(&self, id: SessionId, moved: bool) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&id) else {
            return;
        };
        if entry.hashes.len() == 2 {
            let _retired = if moved {
                entry.hashes.remove(0)
            } else {
                entry.hashes.swap_remove(1)
            };
        }
    }

    /// Forget session `id`'s token: the session ended, or never started.
    pub(crate) fn retire(&self, id: SessionId) {
        drop(self.lock().remove(&id));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_reads_back_and_never_shows_in_debug() {
        let token = ModelToken::random().unwrap();
        let text = token.to_string();
        assert!(text.starts_with("vslm-"));
        assert_eq!(text.len(), 5 + 64);
        assert_eq!(ModelToken::parse(&text), Some(token.clone()));
        assert_eq!(format!("{token:?}"), "ModelToken(<redacted>)");
        for wrong in ["", "vslm-", "vslm-zz", &text.replacen("vslm-", "vsl-", 1)] {
            assert_eq!(ModelToken::parse(wrong), None, "{wrong}");
        }
        assert_ne!(ModelToken::random().unwrap(), token);
    }
}
