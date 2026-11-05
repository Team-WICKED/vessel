//! Typed RPC: the method table, the frames, and the generated dispatcher.
//!
//! [Decision Log]
//! - Purpose: every request must name its response type, and the node must
//!   be unable to compile while any method is unhandled.
//! - Alternatives: REST routes with per-route JSON (no shared method table,
//!   unhandled routes are runtime 404s); Zed's `TypeId`-keyed handler map
//!   with `downcast().unwrap()` (dynamic typing at the core); Paseo's
//!   request id inside each payload with one predicate scan per pending call.
//! - Choice: one table (`rpc_methods!` below) generates `Method`, the `Call`
//!   and `Reply` enums, the `Request` impls binding each request to its
//!   response, a `Handler` trait with one method per call, and `dispatch`,
//!   an exhaustive `match`. Correlation ids live in the frame, not the
//!   payload.
//! - Cost: adding a method means editing this table (deliberate: it is the
//!   one place the protocol is defined) and every `Handler` implementation.

use std::future::Future;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::WireError;
use crate::event::EventRecord;
use crate::harness::Harness;
use crate::id::{EventSeq, RequestId, SessionId, SubscriptionId};
use crate::node::{NodeInfo, ProtocolRange, ProtocolVersion};
use crate::session::{SessionInfo, SessionInput, SessionSpec};

/// A request type, bound to its response type and its method.
pub trait Request: Serialize + DeserializeOwned + Send + 'static {
    /// The method this request invokes.
    const METHOD: Method;
    /// What a successful call returns.
    type Response: Serialize + DeserializeOwned + Send + 'static;

    /// Wrap into the generic call enum for sending.
    fn into_call(self) -> Call;

    /// Extract this request's response from a generic reply.
    ///
    /// # Errors
    /// Fails if the reply belongs to a different method, which means the
    /// peer is broken.
    fn response_from(reply: Reply) -> Result<Self::Response, UnexpectedReply>;
}

/// A reply arrived for a different method than the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnexpectedReply {
    /// Method that was called.
    pub expected: Method,
    /// Method the reply belongs to.
    pub got: Method,
}

impl std::fmt::Display for UnexpectedReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reply for `{}` received for a `{}` request",
            self.got.as_str(),
            self.expected.as_str()
        )
    }
}

impl std::error::Error for UnexpectedReply {}

/// Declares every RPC method in one table.
///
/// Each row: `Variant / handler_fn / "wire.name": RequestType => ResponseType;`
macro_rules! rpc_methods {
    ($(
        $(#[$doc:meta])*
        $variant:ident / $handler:ident / $wire:literal : $request:ty => $response:ty;
    )*) => {
        /// Every RPC method.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum Method {
            $(
                $(#[$doc])*
                #[serde(rename = $wire)]
                $variant,
            )*
        }

        impl Method {
            /// Wire name.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $wire, )*
                }
            }
        }

        /// A request of any method, as sent on the wire.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum Call {
            $(
                $(#[$doc])*
                #[serde(rename = $wire)]
                $variant($request),
            )*
        }

        impl Call {
            /// Which method this is.
            #[must_use]
            pub const fn method(&self) -> Method {
                match self {
                    $( Self::$variant(_) => Method::$variant, )*
                }
            }
        }

        /// A successful response of any method, as sent on the wire.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "result")]
        pub enum Reply {
            $(
                $(#[$doc])*
                #[serde(rename = $wire)]
                $variant($response),
            )*
        }

        impl Reply {
            /// Which method this answers.
            #[must_use]
            pub const fn method(&self) -> Method {
                match self {
                    $( Self::$variant(_) => Method::$variant, )*
                }
            }
        }

        $(
            impl Request for $request {
                const METHOD: Method = Method::$variant;
                type Response = $response;

                fn into_call(self) -> Call {
                    Call::$variant(self)
                }

                fn response_from(reply: Reply) -> Result<$response, UnexpectedReply> {
                    let got = reply.method();
                    if let Reply::$variant(response) = reply {
                        Ok(response)
                    } else {
                        Err(UnexpectedReply { expected: Method::$variant, got })
                    }
                }
            }
        )*

        /// Server side of the protocol: one method per RPC.
        ///
        /// `Context` is per-connection state (for example the connection's
        /// subscriptions), so connection-scoped methods such as `subscribe`
        /// go through the same exhaustive table as everything else.
        pub trait Handler: Sync {
            /// Per-connection state passed to every call.
            type Context: Sync;

            $(
                $(#[$doc])*
                fn $handler(
                    &self,
                    context: &Self::Context,
                    request: $request,
                ) -> impl Future<Output = Result<$response, WireError>> + Send;
            )*
        }

        /// Route a call to its handler method. Exhaustive: a method without
        /// a handler does not compile.
        pub async fn dispatch<H: Handler>(handler: &H, context: &H::Context, call: Call) -> Outcome {
            match call {
                $(
                    Call::$variant(request) => match handler.$handler(context, request).await {
                        Ok(response) => Outcome::Ok(Reply::$variant(response)),
                        Err(error) => Outcome::Err(error),
                    },
                )*
            }
        }
    };
}

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

/// Empty success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack;

rpc_methods! {
    /// Describe the node.
    NodeInfo / node_info / "node.info": GetNode => NodeInfo;
    /// List installed harnesses.
    ListHarnesses / list_harnesses / "harness.list": ListHarnesses => Vec<Harness>;
    /// List sessions.
    ListSessions / list_sessions / "session.list": ListSessions => Vec<SessionInfo>;
    /// Get one session.
    GetSession / get_session / "session.get": GetSession => SessionInfo;
    /// Start a session.
    CreateSession / create_session / "session.create": CreateSession => SessionInfo;
    /// Send input to a session.
    SendInput / send_input / "session.send": SendInput => Ack;
    /// Kill a session.
    KillSession / kill_session / "session.kill": KillSession => Ack;
    /// Read a page of a session's log.
    ReadEvents / read_events / "session.read": ReadEvents => EventPage;
    /// Subscribe to a session's events.
    Subscribe / subscribe / "session.subscribe": Subscribe => Subscribed;
    /// End a subscription.
    Unsubscribe / unsubscribe / "session.unsubscribe": Unsubscribe => Ack;
}

/// Result of a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Success.
    Ok(Reply),
    /// Failure.
    Err(WireError),
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_requests_round_trip() {
        let frame = ClientFrame::Request {
            id: RequestId::new(7),
            call: GetNode.into_call(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            json,
            r#"{"type":"request","id":7,"call":{"method":"node.info","params":null}}"#
        );
        assert_eq!(serde_json::from_str::<ClientFrame>(&json).unwrap(), frame);
    }

    #[test]
    fn replies_bind_to_their_request() {
        let reply = Reply::SendInput(Ack);
        assert_eq!(SendInput::response_from(reply.clone()), Ok(Ack));
        assert_eq!(
            KillSession::response_from(reply),
            Err(UnexpectedReply {
                expected: Method::KillSession,
                got: Method::SendInput,
            })
        );
    }
}
