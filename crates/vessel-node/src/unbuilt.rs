//! Protocol 6 methods this node does not run yet: each answers
//! `unsupported`, naming the method and the track that builds it.
//!
//! [Decision Log]
//! - Purpose: the protocol 6 contract (`docs/plans/backend-m4.md`,
//!   "Protocol 6 (accepted)") lands before the tracks that implement it
//!   (F3 secrets, F4 transfers, X the envelope, built in `envelope`), so
//!   each track builds on reviewed types and the node keeps compiling in
//!   between.
//! - Alternatives: land each row with its implementation (three tracks
//!   would edit the method table at once, and none could build on the
//!   others' types); answer with a generic error from the guard (a call
//!   would pass the scope check and then fail without saying what is
//!   missing).
//! - Choice: one stub per row, generated, behind the same guard as every
//!   method (scopes and ownership are checked first, so a test of the
//!   guard holds for the finished method too). A track lands its method
//!   by moving the row out of here into a real handler.
//! - Constraint: a stub changes nothing and holds nothing; the error code
//!   is `unsupported`, what a client gets for any method a node lacks.

use vessel_proto::{Ack, CopySecret, ErrorCode, Method, Request as _, WireError};

use crate::service::{Connection, NodeService};

/// What a call of `method` gets until `track` builds it.
pub(crate) fn unbuilt(method: Method, track: &str) -> WireError {
    WireError::new(
        ErrorCode::Unsupported,
        format!(
            "`{}` is in protocol 6 but not built on this node yet ({track})",
            method.as_str()
        ),
    )
}

macro_rules! unbuilt {
    ($( $handler:ident : $request:ty => $response:ty, $track:literal; )*) => {
        impl NodeService {
            $(
                pub(crate) async fn $handler(
                    &self,
                    _: &Connection,
                    _: $request,
                ) -> Result<$response, WireError> {
                    Err(unbuilt(<$request>::METHOD, $track))
                }
            )*
        }
    };
}

unbuilt! {
    copy_secret: CopySecret => Ack, "F3, with Track X's envelope";
}
