//! Turning a queued frame into text, without copying fanned-out records.
//!
//! [Decision Log]
//! - Purpose: one live record goes to every subscriber of its session. The
//!   session runtime shares it as an `Arc<EventRecord>`; building a
//!   `ServerFrame::Event` (which owns its record) would deep-copy it once
//!   per subscriber just to serialise it.
//! - Choice: events are queued as `Arc`s and serialised through
//!   [`EventFrame`], a borrowed view that writes exactly the bytes
//!   `ServerFrame::Event` writes (a test pins this). Every other frame is a
//!   `ServerFrame`.
//! - Cost: the event frame's shape is stated twice. The test fails on any
//!   drift; if `vessel-proto` ever carries `Arc<EventRecord>` in
//!   `ServerFrame::Event`, this module goes away.
//! - Protocol 5: encoding is also where a frame is adapted to the
//!   connection's negotiated version (`ServerFrame::for_protocol`): the one
//!   place that knows both the frame and the version. Frames that need no
//!   change are written as they are, without a copy.
//! - `Frame::Server` is boxed: frames wait in the connection's bounded
//!   queue, and an inline `ServerFrame` (264 bytes since `SessionInfo` grew)
//!   would size every slot for the rare large reply.

use std::sync::Arc;

use serde::Serialize;
use vessel_proto::{EventRecord, ProtocolVersion, ServerFrame, SubscriptionId};

/// A frame ready to be written.
pub(super) enum Frame {
    /// Anything but a subscription event.
    Server(Box<ServerFrame>),
    /// A subscription event, record shared with other subscribers.
    Event {
        subscription: SubscriptionId,
        record: Arc<EventRecord>,
    },
}

/// `ServerFrame::Event`, borrowing its record.
#[derive(Serialize)]
#[serde(tag = "type", rename = "event")]
struct EventFrame<'a> {
    subscription: SubscriptionId,
    record: &'a EventRecord,
}

impl Frame {
    /// A frame that is not a subscription event.
    pub(super) fn server(frame: ServerFrame) -> Self {
        Self::Server(Box::new(frame))
    }

    /// The frame's text for a client speaking `protocol`.
    pub(super) fn encode(&self, protocol: ProtocolVersion) -> serde_json::Result<String> {
        match self {
            Self::Server(frame) => match frame.for_protocol(protocol) {
                Some(adapted) => serde_json::to_string(&adapted),
                None => serde_json::to_string(frame),
            },
            Self::Event {
                subscription,
                record,
            } => match record.for_protocol(protocol) {
                Some(adapted) => serde_json::to_string(&EventFrame {
                    subscription: *subscription,
                    record: &adapted,
                }),
                None => serde_json::to_string(&EventFrame {
                    subscription: *subscription,
                    record,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use vessel_proto::{EventSeq, SessionEvent, SessionStatus};

    use super::*;

    #[test]
    fn event_frames_encode_exactly_like_server_frames() {
        let record = EventRecord {
            seq: EventSeq::new(9),
            at: DateTime::UNIX_EPOCH,
            event: SessionEvent::Status {
                status: SessionStatus::Exited { code: Some(1) },
            },
        };
        let subscription = SubscriptionId::new(4);
        let borrowed = Frame::Event {
            subscription,
            record: Arc::new(record.clone()),
        }
        .encode(vessel_proto::PROTOCOL_VERSION)
        .unwrap();
        let owned = serde_json::to_string(&ServerFrame::Event {
            subscription,
            record,
        })
        .unwrap();
        assert_eq!(borrowed, owned);
    }
}
