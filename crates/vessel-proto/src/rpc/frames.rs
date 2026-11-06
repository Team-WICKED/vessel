//! The frames that carry calls, replies and subscription events, and the
//! handshake.

use serde::{Deserialize, Serialize};

use super::{Call, Outcome};
use crate::error::WireError;
use crate::event::EventRecord;
use crate::id::{EventSeq, RequestId, SubscriptionId};
use crate::node::{NodeInfo, ProtocolRange, ProtocolVersion};

/// Who is connecting; for logs and future per-client capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Client program name (`vessel-cli`, `vessel-web`, ...).
    pub name: String,
    /// Client program version.
    pub version: String,
}

/// First frame a client sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Protocols the client speaks.
    pub protocol: ProtocolRange,
    /// The client.
    pub client: ClientInfo,
}

/// The node's answer to a successful `Hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The negotiated protocol version for this connection.
    pub protocol: ProtocolVersion,
    /// The node.
    pub node: NodeInfo,
}

/// Why a subscription stopped delivering events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SubscriptionEnd {
    /// The session ended and its final event was delivered.
    SessionEnded,
    /// The client unsubscribed.
    Unsubscribed,
    /// The client fell too far behind the live stream. Nothing was skipped:
    /// read or resubscribe from `resume_from`.
    Lagged {
        /// First sequence number not delivered.
        resume_from: EventSeq,
    },
}

/// A frame from client to node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Handshake; must be the first frame.
    Hello(Hello),
    /// A call.
    Request {
        /// Correlation id, unique per connection.
        id: RequestId,
        /// The call.
        call: Call,
    },
    /// Abandon an in-flight call; the node may still answer it.
    Cancel {
        /// The call's id.
        id: RequestId,
    },
}

/// A frame from node to client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Handshake accepted.
    Welcome(Welcome),
    /// Handshake refused (for example `ProtocolMismatch`); the node closes the
    /// connection after sending this.
    Rejected {
        /// Why.
        error: WireError,
    },
    /// The answer to a call.
    Response {
        /// The call's id.
        id: RequestId,
        /// What happened.
        outcome: Outcome,
    },
    /// A subscribed session event.
    Event {
        /// Which subscription.
        subscription: SubscriptionId,
        /// The record.
        record: EventRecord,
    },
    /// A subscription stopped; no more events carry its id.
    SubscriptionEnded {
        /// Which subscription.
        subscription: SubscriptionId,
        /// Why.
        end: SubscriptionEnd,
    },
}
