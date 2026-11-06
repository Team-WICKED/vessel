//! The scope check (ADR 0010): every call is checked against the scope its
//! method declares, before it runs.
//!
//! [Decision Log]
//! - Purpose: a paired device may only call what its scopes allow, and no
//!   method may be added without the check.
//! - Constraint: `vessel-proto`'s `Handler` and `dispatch` are frozen for
//!   milestone 3, so the check cannot live in the generated dispatch, and
//!   `vessel-rpc` knows nothing of principals.
//! - Alternatives: a check at the top of every `NodeService` method (one
//!   forgotten line is a hole); a check in `vessel-rpc` before dispatch (a
//!   principal concept in the transport layer).
//! - Choice: [`Guarded`] wraps the service and implements `Handler` itself,
//!   generated from one row per method. Every generated method runs the
//!   same check first, with the method taken from the request type
//!   (`Request::METHOD`), so a row cannot check the wrong scope. A method
//!   added to the protocol is a new `Handler` method, which does not compile
//!   here until it gets its row, and the row cannot leave the check out.
//! - `NodeService`'s calls are inherent methods, not a `Handler`, so the
//!   service cannot be handed to a `Server` without this wrapper.
//! - Cost: the rows repeat the protocol table's handler names and types;
//!   the compiler keeps them in step.
//!
//! [Decision Log: ownership] (protocol 5, ADR 0013)
//! - Agents may `operate` only on sessions their own session started (or
//!   that those started). Every request type declares which session it
//!   operates on ([`Target`]); the generated method checks ownership right
//!   after the scope, so, as with scopes, a new method does not compile
//!   until it says. Reads (`session.get`, `session.read`,
//!   `session.subscribe`) name a session too but are not ownership-checked:
//!   an agent sees the fleet's sessions, as ADR 0013 grants.

use vessel_proto::{
    Ack, BlobChunk, CloseTransfer, CopySecret, CreateSession, DeleteSecret, Deliver, DeviceInfo,
    EventPage, ExportMemory, ExportSession, FleetMember, FleetNode, Forward, Forwarded, Generation,
    GetManifest, GetNode, GetSession, GrantHome, Handler, Harness, ImportMemory, ImportSession,
    Invitation, InviteDevice, KillSession, ListDevices, ListHarnesses, ListMembers, ListNodes,
    ListPeers, ListProfiles, ListSecrets, ListSessions, Manifest, MemoryImported, MoveProfile,
    NodeInfo, ProfileInfo, ReadBlob, ReadEvents, ReadTransfer, RelayCall, ReloadProfiles,
    ReportApplied, Request, RevokeDevice, SecretName, SendInput, SessionId, SessionImported,
    SessionInfo, SetSecret, StopNode, Subscribe, Subscribed, TransferManifest, Unsubscribe,
    WatchFleet, WireError,
};

use crate::service::{Connection, NodeService};

/// The node's service behind the scope check. This is the handler every
/// transport serves.
#[derive(Debug)]
pub(crate) struct Guarded(pub(crate) NodeService);

macro_rules! guarded {
    ($( $handler:ident : $request:ty => $response:ty; )*) => {
        impl Handler for Guarded {
            type Context = Connection;

            $(
                async fn $handler(
                    &self,
                    connection: &Connection,
                    request: $request,
                ) -> Result<$response, WireError> {
                    connection.principal().permit(<$request as Request>::METHOD)?;
                    if let Some(session) = Target::target(&request) {
                        self.0.check_owner(connection.principal(), session).await?;
                    }
                    self.0.$handler(connection, request).await
                }
            )*
        }
    };
}

/// The session an `operate` call acts on, for the ownership rule.
trait Target {
    fn target(&self) -> Option<SessionId>;
}

macro_rules! targets {
    ($( $request:ty ),* $(,)?) => {
        $(
            impl Target for $request {
                fn target(&self) -> Option<SessionId> {
                    Some(self.session)
                }
            }
        )*
    };
}

macro_rules! untargeted {
    ($( $request:ty ),* $(,)?) => {
        $(
            impl Target for $request {
                fn target(&self) -> Option<SessionId> {
                    None
                }
            }
        )*
    };
}

// `session.export` copies a session's conversation away: an agent may do
// that only to a session it started, like any other `operate` call on one.
targets!(SendInput, KillSession, MoveProfile, ExportSession);

// `session.create` is limited by `agents` instead (count and depth), and so
// is `session.import`, which creates one; the reads are open to every
// agent; the envelope's rows carry a call whose own row is checked where it
// runs (protocol 6, Track X); the rest are not `operate` calls.
untargeted!(
    GetNode,
    ListHarnesses,
    ListSessions,
    GetSession,
    CreateSession,
    ReadEvents,
    Subscribe,
    Unsubscribe,
    StopNode,
    InviteDevice,
    ListDevices,
    RevokeDevice,
    ListProfiles,
    ReloadProfiles,
    GetManifest,
    ReadBlob,
    WatchFleet,
    ReportApplied,
    ListMembers,
    Forward,
    RelayCall,
    Deliver,
    GrantHome,
    ListPeers,
    ListNodes,
    ImportSession,
    ExportMemory,
    ImportMemory,
    ReadTransfer,
    CloseTransfer,
    SetSecret,
    ListSecrets,
    DeleteSecret,
    CopySecret,
);

guarded! {
    node_info: GetNode => NodeInfo;
    list_harnesses: ListHarnesses => Vec<Harness>;
    list_sessions: ListSessions => Vec<SessionInfo>;
    get_session: GetSession => SessionInfo;
    create_session: CreateSession => SessionInfo;
    send_input: SendInput => Ack;
    kill_session: KillSession => Ack;
    read_events: ReadEvents => EventPage;
    subscribe: Subscribe => Subscribed;
    unsubscribe: Unsubscribe => Ack;
    stop_node: StopNode => Ack;
    invite_device: InviteDevice => Invitation;
    list_devices: ListDevices => Vec<DeviceInfo>;
    revoke_device: RevokeDevice => Ack;
    list_profiles: ListProfiles => Vec<ProfileInfo>;
    reload_profiles: ReloadProfiles => Vec<ProfileInfo>;
    move_profile: MoveProfile => SessionInfo;
    get_manifest: GetManifest => Manifest;
    read_blob: ReadBlob => BlobChunk;
    watch_fleet: WatchFleet => Generation;
    report_applied: ReportApplied => Ack;
    list_members: ListMembers => Vec<FleetMember>;
    forward: Forward => Forwarded;
    relay: RelayCall => Forwarded;
    deliver: Deliver => Forwarded;
    grant_home: GrantHome => Ack;
    list_peers: ListPeers => Vec<FleetNode>;
    list_nodes: ListNodes => Vec<FleetNode>;
    export_session: ExportSession => TransferManifest;
    import_session: ImportSession => SessionImported;
    export_memory: ExportMemory => TransferManifest;
    import_memory: ImportMemory => MemoryImported;
    read_transfer: ReadTransfer => BlobChunk;
    close_transfer: CloseTransfer => Ack;
    set_secret: SetSecret => Ack;
    list_secrets: ListSecrets => Vec<SecretName>;
    delete_secret: DeleteSecret => Ack;
    copy_secret: CopySecret => Ack;
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;

    use super::*;

    /// Method resolution on a concrete type picks `IsHandler` when the
    /// type implements `Handler`, and falls back to `NotHandler` (one
    /// autoref further) when it does not: a test of a missing impl that
    /// stable Rust can write.
    struct Probe<T>(PhantomData<T>);

    trait IsHandler {
        fn is_handler(&self) -> bool {
            true
        }
    }

    impl<T: Handler> IsHandler for Probe<T> {}

    trait NotHandler {
        fn is_handler(&self) -> bool {
            false
        }
    }

    impl<T> NotHandler for &Probe<T> {}

    /// Only the guarded service can be handed to a `Server`; the service
    /// itself has no `Handler` impl to serve without the scope check.
    #[test]
    fn only_the_guarded_service_is_a_handler() {
        assert!(Probe::<Guarded>(PhantomData).is_handler());
        assert!(!(&Probe::<NodeService>(PhantomData)).is_handler());
    }
}
