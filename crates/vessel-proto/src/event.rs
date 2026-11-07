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
//!
//! [Decision Log: events newer than the connection] (protocol 5)
//! - Purpose: protocol 5 adds `ProfileChanged`. A client that negotiated an
//!   older version has no variant for it and would fail to decode the
//!   record, ending its subscription or read.
//! - Constraint: the log is gap-free; leaving the record out would show the
//!   client a gap it cannot fill (every re-read would leave it out again).
//! - Alternatives: a catch-all `Unknown` variant (helps only clients built
//!   after it, not the protocol 4 clients that exist); refusing old clients
//!   (the range `MIN_PROTOCOL_VERSION..` promises they work).
//! - Choice: every event knows the version it appeared in
//!   ([`SessionEvent::since`]); for an older connection it has a stand-in
//!   that version can read ([`SessionEvent::for_protocol`]), with the same
//!   `seq`. `vessel-rpc` applies it where it encodes frames, the one place
//!   that knows the connection's version.
//! - Trade-off: an older client sees a stand-in, not the event: for
//!   `ProfileChanged`, a `HarnessLog` line starting with `vessel:`. A new
//!   variant must add its stand-in here, or the exhaustive matches stop the
//!   build.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentEvent;
use crate::id::EventSeq;
use crate::name::ProfileName;
use crate::node::ProtocolVersion;
use crate::session::SessionStatus;
use crate::terminal::TerminalOutput;

/// Something that happened in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// The session moved to another profile (protocol 5): the harness was
    /// restarted under it and resumed the same conversation.
    ProfileChanged {
        /// The profile before; `None` is the harness's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<ProfileName>,
        /// The profile now.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<ProfileName>,
    },
}

impl SessionEvent {
    /// The first protocol version that has this event.
    #[must_use]
    pub const fn since(&self) -> ProtocolVersion {
        match self {
            Self::Status { .. } | Self::Terminal(_) | Self::Agent { .. } => ProtocolVersion::new(2),
            Self::ProfileChanged { .. } => ProtocolVersion::new(5),
        }
    }

    /// What a client speaking `protocol` receives instead, or `None` when it
    /// can read this event as it is.
    #[must_use]
    pub fn for_protocol(&self, protocol: ProtocolVersion) -> Option<Self> {
        if self.since() <= protocol {
            return None;
        }
        match self {
            Self::Status { .. } | Self::Terminal(_) | Self::Agent { .. } => None,
            Self::ProfileChanged { from, to } => {
                let name = |profile: &Option<ProfileName>| {
                    profile
                        .as_ref()
                        .map_or_else(|| "default".to_owned(), ToString::to_string)
                };
                Some(Self::Agent {
                    event: AgentEvent::HarnessLog {
                        line: format!(
                            "vessel: profile changed from {} to {}",
                            name(from),
                            name(to)
                        ),
                    },
                })
            }
        }
    }
}

/// An event with its position and time; the unit of the session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct EventRecord {
    /// Position in the session log.
    pub seq: EventSeq,
    /// When the node recorded it.
    pub at: DateTime<Utc>,
    /// What happened.
    pub event: SessionEvent,
}

impl EventRecord {
    /// This record as a client speaking `protocol` reads it, or `None` when
    /// it needs no change. The position and time stay the same.
    #[must_use]
    pub fn for_protocol(&self, protocol: ProtocolVersion) -> Option<Self> {
        self.event.for_protocol(protocol).map(|event| Self {
            seq: self.seq,
            at: self.at,
            event,
        })
    }
}
