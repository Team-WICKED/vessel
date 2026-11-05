//! Identifier and counter newtypes.
//!
//! Every identifier that crosses the wire has its own type so the compiler
//! stops a `SessionId` from being passed where a `NodeId` is expected. There
//! are three shapes:
//!
//! - node-minted random ids (`NodeId`, `SessionId`, `PermissionId`): `UUIDv4`;
//! - harness-minted ids we only carry (`HarnessSessionId`, `ToolCallId`, ...):
//!   opaque strings, compared but never parsed;
//! - per-connection or per-session counters (`RequestId`, `SubscriptionId`,
//!   `EventSeq`): `u64`, advanced only through checked methods.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Declares a `UUIDv4` identifier minted by the node.
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint a fresh random id.
            #[must_use]
            pub fn random() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s.trim()).map(Self)
            }
        }
    };
}

/// Declares an opaque identifier minted by something other than the node.
///
/// Backed by `Arc<str>`: these ids are copied into many events and fanned
/// out to many subscribers, so cloning must not allocate.
macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Wrap an id received from elsewhere.
            #[must_use]
            pub fn new(id: impl Into<Arc<str>>) -> Self {
                Self(id.into())
            }

            /// The id as received.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

/// Declares a monotonically increasing `u64` counter.
macro_rules! counter_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            /// The first value.
            pub const ZERO: Self = Self(0);

            /// Wrap a raw value (for decoding and tests).
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// The raw value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// The value after this one, or `None` once `u64` is exhausted.
            ///
            /// Callers treat `None` as "this stream is full" and end it,
            /// rather than wrapping around and reusing ids.
            #[must_use]
            pub const fn next(self) -> Option<Self> {
                match self.0.checked_add(1) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

uuid_id! {
    /// Identity of a Vessel node; one per machine, stable across restarts.
    NodeId
}

uuid_id! {
    /// A session on a node. Unique across the fleet, so it can be used
    /// without the node id once peers exist.
    SessionId
}

uuid_id! {
    /// A pending permission request inside a structured session. Minted by the
    /// node (not the harness) so many clients can answer the same request and
    /// the first answer wins.
    PermissionId
}

opaque_id! {
    /// The harness's own session id (an ACP `sessionId`). Kept so a
    /// structured session can be resumed by the harness after a node restart.
    HarnessSessionId
}

opaque_id! {
    /// A tool call id assigned by the harness.
    ToolCallId
}

opaque_id! {
    /// A permission option id offered by the harness (for example
    /// `allow-once`). Echoed back verbatim when a client picks it.
    PermissionOptionId
}

opaque_id! {
    /// A model name as the harness understands it (`opus`, `gpt-5`, ...).
    /// The set is open and harness-specific, so it is not an enum.
    ModelId
}

counter_id! {
    /// Client-assigned id correlating a request with its response; unique
    /// per connection.
    RequestId
}

counter_id! {
    /// Node-assigned id of one event subscription; unique per connection.
    SubscriptionId
}

counter_id! {
    /// Position of an event in one session's log. Starts at zero and has no
    /// gaps, so a client resumes with "everything from seq N".
    EventSeq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_stop_instead_of_wrapping() {
        assert_eq!(EventSeq::ZERO.next(), Some(EventSeq::new(1)));
        assert_eq!(EventSeq::new(u64::MAX).next(), None);
    }

    #[test]
    fn uuid_ids_round_trip_through_text() {
        let id = SessionId::random();
        assert_eq!(id.to_string().parse::<SessionId>().unwrap(), id);
    }

    #[test]
    fn opaque_ids_serialise_as_plain_strings() {
        let id = ToolCallId::new("toolu_01");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""toolu_01""#);
    }
}
