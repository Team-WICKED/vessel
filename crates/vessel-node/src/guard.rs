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
//! - Cost: the rows repeat the protocol table's handler names and types;
//!   the compiler keeps them in step.

use vessel_proto::{
    Ack, CreateSession, DeviceInfo, EventPage, GetNode, GetSession, Handler, Harness, Invitation,
    InviteDevice, KillSession, ListDevices, ListHarnesses, ListSessions, NodeInfo, ReadEvents,
    Request, RevokeDevice, SendInput, SessionInfo, StopNode, Subscribe, Subscribed, Unsubscribe,
    WireError,
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
                    self.0.$handler(connection, request).await
                }
            )*
        }
    };
}

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
}
