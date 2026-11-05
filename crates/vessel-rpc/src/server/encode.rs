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

use std::sync::Arc;

use serde::Serialize;
use vessel_proto::{EventRecord, ServerFrame, SubscriptionId};

/// A frame ready to be written.
pub(super) enum Frame {
    /// Anything but a subscription event.
    Server(ServerFrame),
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
    pub(super) fn encode(&self) -> serde_json::Result<String> {
        match self {
            Self::Server(frame) => serde_json::to_string(frame),
            Self::Event {
                subscription,
                record,
            } => serde_json::to_string(&EventFrame {
                subscription: *subscription,
                record,
            }),
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
        .encode()
        .unwrap();
        let owned = serde_json::to_string(&ServerFrame::Event {
            subscription,
            record,
        })
        .unwrap();
        assert_eq!(borrowed, owned);
    }
}
