//! The per-session event log every client reads.
//!
//! [Decision Log]
//! - Purpose: clients disconnect, reconnect, and attach late. Each must be
//!   able to say "give me everything from N" and get an exact, gap-free
//!   continuation.
//! - Alternatives: Paseo's in-memory timeline with a random epoch per daemon
//!   run (every restart invalidates every client cursor and forces a full
//!   provider-history re-read); Zed's unacked-message replay buffer (per
//!   connection, unbounded).
//! - Choice: every session event gets a durable, gap-free `EventSeq` assigned
//!   by the session runtime and persisted by the node, so `(SessionId, seq)`
//!   is a cursor that survives reconnects and node restarts. Live delivery
//!   and paged reads (`session.read`) serve the same records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentEvent;
use crate::id::EventSeq;
use crate::session::SessionStatus;
use crate::terminal::TerminalOutput;

/// Something that happened in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The process changed state. A final `Status` is the last event of
    /// every session.
    Status {
        /// New state.
        status: SessionStatus,
    },
    /// Terminal mode: output bytes.
    Terminal(TerminalOutput),
    /// Structured mode: a timeline entry.
    Agent {
        /// The entry.
        event: AgentEvent,
    },
}

/// An event with its position and time; the unit of the session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Position in the session log.
    pub seq: EventSeq,
    /// When the node recorded it.
    pub at: DateTime<Utc>,
    /// What happened.
    pub event: SessionEvent,
}
