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

use crate::device::{DeviceInfo, Invitation, InviteDevice, ListDevices, RevokeDevice, Scope};
use crate::error::WireError;
use crate::fleet::{
    BlobChunk, FleetMember, Generation, GetManifest, ListMembers, Manifest, ReadBlob,
    ReportApplied, WatchFleet,
};
use crate::forward::{
    Deliver, FleetNode, ForgetNode, Forward, Forwarded, GrantHome, ListNodes, ListPeers,
    NodeForgotten, RelayCall,
};
use crate::harness::Harness;
use crate::name::SecretName;
use crate::node::{NodeInfo, ProtocolVersion};
use crate::profile::{ListProfiles, MoveProfile, ProfileInfo, ReloadProfiles};
use crate::secret::{CopySecret, DeleteSecret, ListSecrets, SetSecret};
use crate::session::SessionInfo;
use crate::transfer::{
    CloseTransfer, ExportMemory, ExportSession, ImportMemory, ImportSession, MemoryImported,
    ReadTransfer, SessionImported, TransferManifest,
};

mod frames;
mod requests;

pub use frames::{ClientFrame, ClientInfo, Hello, ServerFrame, SubscriptionEnd, Welcome};
pub use requests::{
    Ack, CreateSession, EventPage, GetNode, GetSession, KillSession, ListHarnesses, ListSessions,
    ReadEvents, SendInput, StopNode, StopWhen, Subscribe, Subscribed, Unsubscribe,
};

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
/// Each row: `Variant / handler_fn / "wire.name" [Scope, since]: Request => Response;`
/// The scope column (protocol 4, ADR 0010) is what a paired device must hold
/// to call the method; adding a method without one does not compile. `since`
/// (protocol 5) is the first protocol version with the method: the client
/// refuses to send it on an older connection and the node refuses to run it
/// there, so neither side relies on the other ignoring what it cannot know.
macro_rules! rpc_methods {
    ($(
        $(#[$doc:meta])*
        $variant:ident / $handler:ident / $wire:literal [$scope:ident, $since:literal] : $request:ty => $response:ty;
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

            /// The scope a paired device needs to call it.
            #[must_use]
            pub const fn scope(self) -> Scope {
                match self {
                    $( Self::$variant => Scope::$scope, )*
                }
            }

            /// The first protocol version with this method.
            #[must_use]
            pub const fn since(self) -> ProtocolVersion {
                match self {
                    $( Self::$variant => ProtocolVersion::new($since), )*
                }
            }

            /// Every method, in table order.
            pub const ALL: &'static [Self] = &[$( Self::$variant, )*];
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

rpc_methods! {
    /// Describe the node.
    NodeInfo / node_info / "node.info" [Read, 2]: GetNode => NodeInfo;
    /// List installed harnesses.
    ListHarnesses / list_harnesses / "harness.list" [Read, 2]: ListHarnesses => Vec<Harness>;
    /// List sessions.
    ListSessions / list_sessions / "session.list" [Read, 2]: ListSessions => Vec<SessionInfo>;
    /// Get one session.
    GetSession / get_session / "session.get" [Read, 2]: GetSession => SessionInfo;
    /// Start a session.
    CreateSession / create_session / "session.create" [Operate, 2]: CreateSession => SessionInfo;
    /// Send input to a session.
    SendInput / send_input / "session.send" [Operate, 2]: SendInput => Ack;
    /// Kill a session.
    KillSession / kill_session / "session.kill" [Operate, 2]: KillSession => Ack;
    /// Read a page of a session's log.
    ReadEvents / read_events / "session.read" [Read, 2]: ReadEvents => EventPage;
    /// Subscribe to a session's events.
    Subscribe / subscribe / "session.subscribe" [Read, 2]: Subscribe => Subscribed;
    /// End a subscription.
    Unsubscribe / unsubscribe / "session.unsubscribe" [Read, 2]: Unsubscribe => Ack;
    /// Stop the node (protocol 3).
    StopNode / stop_node / "node.stop" [Admin, 3]: StopNode => Ack;
    /// Make a pairing invitation (protocol 4).
    InviteDevice / invite_device / "device.invite" [Admin, 4]: InviteDevice => Invitation;
    /// List paired devices (protocol 4).
    ListDevices / list_devices / "device.list" [Admin, 4]: ListDevices => Vec<DeviceInfo>;
    /// Revoke a device and close its connections (protocol 4).
    RevokeDevice / revoke_device / "device.revoke" [Admin, 4]: RevokeDevice => Ack;
    /// List profiles (protocol 5).
    ListProfiles / list_profiles / "profile.list" [Read, 5]: ListProfiles => Vec<ProfileInfo>;
    /// Read `profiles.toml` again (protocol 5).
    ReloadProfiles / reload_profiles / "profile.reload" [Admin, 5]: ReloadProfiles => Vec<ProfileInfo>;
    /// Move a structured session to another profile (protocol 5).
    MoveProfile / move_profile / "session.move_profile" [Operate, 5]: MoveProfile => SessionInfo;
    /// The fleet's current bundle (protocol 5).
    GetManifest / get_manifest / "fleet.manifest" [Sync, 5]: GetManifest => Manifest;
    /// Part of a bundle blob (protocol 5).
    ReadBlob / read_blob / "fleet.blob" [Sync, 5]: ReadBlob => BlobChunk;
    /// Wait for a newer bundle generation (protocol 5).
    WatchFleet / watch_fleet / "fleet.watch" [Sync, 5]: WatchFleet => Generation;
    /// Report an applied generation (protocol 5).
    ReportApplied / report_applied / "fleet.report" [Sync, 5]: ReportApplied => Ack;
    /// List fleet members (protocol 5).
    ListMembers / list_members / "fleet.members" [Admin, 5]: ListMembers => Vec<FleetMember>;
    /// Run a call on another node of the fleet (protocol 6).
    Forward / forward / "fleet.forward" [Read, 6]: Forward => Forwarded;
    /// A member asks its home to pass a call on (protocol 6).
    Relay / relay / "fleet.relay" [Relay, 6]: RelayCall => Forwarded;
    /// The home has a member run a call (protocol 6).
    Deliver / deliver / "fleet.deliver" [Read, 6]: Deliver => Forwarded;
    /// A member grants its home a credential on itself (protocol 6).
    GrantHome / grant_home / "fleet.grant" [Sync, 6]: GrantHome => Ack;
    /// The home's view of the fleet, for members (protocol 6).
    ListPeers / list_peers / "fleet.peers" [Sync, 6]: ListPeers => Vec<FleetNode>;
    /// The fleet as this node knows it (protocol 6).
    ListNodes / list_nodes / "fleet.nodes" [Read, 6]: ListNodes => Vec<FleetNode>;
    /// The home forgets a member node's binding (protocol 6).
    ForgetNode / forget_node / "fleet.forget" [Admin, 6]: ForgetNode => NodeForgotten;
    /// Snapshot a session's harness files (protocol 6).
    ExportSession / export_session / "session.export" [Operate, 6]: ExportSession => TransferManifest;
    /// Copy a session here from a node of the fleet (protocol 6).
    ImportSession / import_session / "session.import" [Operate, 6]: ImportSession => SessionImported;
    /// Snapshot a profile's memory (protocol 6).
    ExportMemory / export_memory / "memory.export" [Admin, 6]: ExportMemory => TransferManifest;
    /// Copy memory here from a node of the fleet (protocol 6).
    ImportMemory / import_memory / "memory.import" [Admin, 6]: ImportMemory => MemoryImported;
    /// Part of a file of an open transfer (protocol 6).
    ReadTransfer / read_transfer / "transfer.read" [Read, 6]: ReadTransfer => BlobChunk;
    /// Drop an open transfer (protocol 6).
    CloseTransfer / close_transfer / "transfer.close" [Read, 6]: CloseTransfer => Ack;
    /// Store a secret (protocol 6).
    SetSecret / set_secret / "secret.set" [Admin, 6]: SetSecret => Ack;
    /// List secrets by name (protocol 6).
    ListSecrets / list_secrets / "secret.list" [Admin, 6]: ListSecrets => Vec<SecretName>;
    /// Remove a secret (protocol 6).
    DeleteSecret / delete_secret / "secret.delete" [Admin, 6]: DeleteSecret => Ack;
    /// Copy a secret to a node of the fleet (protocol 6).
    CopySecret / copy_secret / "secret.copy" [Admin, 6]: CopySecret => Ack;
}

impl Call {
    /// The protocol version this call needs: its method's, or a later one
    /// when it uses a field added since. A node that predates a field would
    /// ignore it silently (and, say, start a session with the wrong profile
    /// or invite a device with more scopes than asked), so the client checks
    /// this before sending. A request that gains a field adds a line here.
    #[must_use]
    pub fn since(&self) -> ProtocolVersion {
        let method = self.method().since();
        let fields = match self {
            Self::CreateSession(request) => request.spec.profile.as_ref().map(|_| 5),
            // `scopes` came in 5; a scope newer than that needs its own
            // version, or an older node would refuse the name it lacks.
            Self::InviteDevice(request) => request.scopes.as_ref().map(|scopes| {
                scopes
                    .iter()
                    .map(|scope| scope.since().get())
                    .fold(5, u32::max)
            }),
            Self::ReadEvents(request) => request.wait.map(|_| 6),
            Self::NodeInfo(_)
            | Self::ListHarnesses(_)
            | Self::ListSessions(_)
            | Self::GetSession(_)
            | Self::SendInput(_)
            | Self::KillSession(_)
            | Self::Subscribe(_)
            | Self::Unsubscribe(_)
            | Self::StopNode(_)
            | Self::ListDevices(_)
            | Self::RevokeDevice(_)
            | Self::ListProfiles(_)
            | Self::ReloadProfiles(_)
            | Self::MoveProfile(_)
            | Self::GetManifest(_)
            | Self::ReadBlob(_)
            | Self::WatchFleet(_)
            | Self::ReportApplied(_)
            | Self::ListMembers(_)
            // The inner call of an envelope is checked against the target's
            // protocol where it is delivered, not against this connection.
            | Self::Forward(_)
            | Self::Relay(_)
            | Self::Deliver(_)
            | Self::GrantHome(_)
            | Self::ForgetNode(_)
            | Self::ListPeers(_)
            | Self::ListNodes(_)
            | Self::ExportSession(_)
            | Self::ImportSession(_)
            | Self::ExportMemory(_)
            | Self::ImportMemory(_)
            | Self::ReadTransfer(_)
            | Self::CloseTransfer(_)
            | Self::SetSecret(_)
            | Self::ListSecrets(_)
            | Self::DeleteSecret(_)
            | Self::CopySecret(_) => None,
        };
        fields.map_or(method, |since| method.max(ProtocolVersion::new(since)))
    }
}

/// Result of a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "one Outcome per call, moved once to the writer and never stored in bulk; \
              boxing the reply would add an allocation to every call to save a copy"
)]
pub enum Outcome {
    /// Success.
    Ok(Reply),
    /// Failure.
    Err(WireError),
}

#[cfg(test)]
mod tests;
