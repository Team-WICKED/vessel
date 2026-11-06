//! The request and reply payloads named in the method table (`rpc.rs`).

use serde::{Deserialize, Serialize};

use crate::event::EventRecord;
use crate::id::{EventSeq, SessionId, SubscriptionId};
use crate::session::{SessionInfo, SessionInput, SessionSpec};

/// `node.info`: describe the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetNode;

/// `harness.list`: discover installed harnesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListHarnesses;

/// `session.list`: every session the node knows, newest first, including
/// ended sessions restored from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessions;

/// `session.get`: one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSession {
    /// Which session.
    pub session: SessionId,
}

/// `session.create`: start a harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    /// What to run.
    pub spec: SessionSpec,
}

/// `session.send`: send input to a live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendInput {
    /// Which session.
    pub session: SessionId,
    /// What to send.
    pub input: SessionInput,
}

/// `session.kill`: stop a session's process (and its process group).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSession {
    /// Which session.
    pub session: SessionId,
}

/// `session.read`: a page of a session's log. The authoritative way to fill
/// gaps; live subscriptions are an optimisation on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadEvents {
    /// Which session.
    pub session: SessionId,
    /// First sequence number wanted.
    pub from: EventSeq,
    /// Maximum number of records; the node may return fewer and caps it.
    pub limit: u32,
}

/// Reply to `session.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPage {
    /// Records in order, starting at the requested `from` (or the oldest
    /// retained record if older ones were pruned).
    pub records: Vec<EventRecord>,
    /// Sequence number to ask for next.
    pub next: EventSeq,
    /// Whether `next` is the end of the log right now.
    pub at_end: bool,
}

/// `session.subscribe`: stream a session's events from `from` onwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    /// Which session.
    pub session: SessionId,
    /// First sequence number wanted (zero for the whole history).
    pub from: EventSeq,
}

/// Reply to `session.subscribe`. Events follow as `ServerFrame::Event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribed {
    /// Id that tags the events of this subscription.
    pub subscription: SubscriptionId,
    /// The session at the moment of subscribing.
    pub info: SessionInfo,
}

/// `session.unsubscribe`: stop a subscription on this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsubscribe {
    /// Which subscription.
    pub subscription: SubscriptionId,
}

/// When `node.stop` takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopWhen {
    /// Stop now: live sessions end `Interrupted`, as on a signal.
    Now,
    /// Drain: refuse new sessions and stop once none is live.
    WhenIdle,
}

/// `node.stop` (protocol 3): stop the node, now or once idle. Used by
/// remote update; a client that negotiated protocol 2 never sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopNode {
    /// When to stop.
    pub when: StopWhen,
}

/// Empty success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack;
