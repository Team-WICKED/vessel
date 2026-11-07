//! The frames between the browser and `vessel ui` (ADR 0017): JSON text
//! frames on `/api/ws`, one message each, and the HTTP API's bodies.
//!
//! The browser's first frame must be [`ClientFrame::Hello`] with the
//! proof of its bootstrap/session key after the server challenge, within
//! a short deadline; anything else closes the
//! socket with [`CLOSE_UNAUTHORIZED`]. After the [`ServerFrame::Welcome`]
//! the browser:
//!
//! - calls methods on nodes ([`ClientFrame::Call`]), each answered once by
//!   a [`ServerFrame::Reply`] with the same id;
//! - follows sessions ([`ClientFrame::Follow`]): records come as
//!   [`ServerFrame::Event`], the stream's ups and downs as
//!   [`ServerFrame::Stream`];
//! - asks the bridge itself ([`ClientFrame::Op`]) for what only this
//!   machine knows: the book's nodes, pairing, QR codes.
//!
//! Every node's connection state is pushed as [`ServerFrame::Node`].
//!
//! Ids (`CallId`, `StreamId`) are the browser's: the bridge only echoes
//! them, so they need to be unique among one socket's open calls and
//! streams, nothing more.

mod auth;
pub use auth::{AuthMode, InvalidAuthValue, LaunchId, Nonce};

use std::fmt;

use serde::{Deserialize, Serialize};
use vessel_proto::{
    EventRecord, EventSeq, Method, NodeId, NodeInfo, PairingOffer, ProtocolVersion, RawJson, Reply,
    SessionId, WireError,
};

/// Closes a websocket that sent no valid token in time.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// A frame from the browser.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// The client's response to the server challenge.
    Hello {
        /// Bootstrap or a session resumed by this tab.
        mode: AuthMode,
        /// The browser's fresh challenge; binds the welcome to this socket.
        nonce: Nonce,
        /// HMAC of both challenges, launch id and role (never the key).
        proof: PresentedToken,
    },
    /// Call a method on a node.
    Call {
        /// The browser's id for the call, echoed in the reply.
        id: CallId,
        /// The node.
        node: NodeKey,
        /// The protocol's own `Call` (`{ "method", "params" }`). Kept as
        /// text until the frame is read, then decoded into the typed
        /// `Call`, so that a call this bridge cannot read is still
        /// answered, by id.
        #[cfg_attr(feature = "ts", ts(as = "vessel_proto::Call"))]
        call: RawJson,
    },
    /// Follow a session from a record on.
    Follow {
        /// The browser's id for the stream.
        stream: StreamId,
        /// The node.
        node: NodeKey,
        /// The session.
        session: SessionId,
        /// The first record wanted.
        from: EventSeq,
    },
    /// Stop following.
    Unfollow {
        /// The stream.
        stream: StreamId,
    },
    /// Ask the bridge itself.
    Op {
        /// The browser's id for the request, echoed in the answer.
        id: CallId,
        /// What to do.
        op: Op,
    },
}

/// A frame to the browser.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The server's fresh challenge, before authentication.
    Challenge {
        /// This UI launch.
        launch: LaunchId,
        /// A fresh random value, used on this connection only.
        nonce: Nonce,
    },
    /// Both peers prove their key before the browser opens the bridge.
    Welcome {
        /// The server's version (`vessel`'s).
        version: String,
        /// Must match the challenge and the tab's previous launch.
        launch: LaunchId,
        /// HMAC including the client nonce and any issued session key.
        proof: PresentedToken,
        /// An independent key, only when redeeming the one-time code.
        session: Option<PresentedToken>,
    },
    /// The answer to a call.
    Reply {
        /// The call's id.
        id: CallId,
        /// The node's reply, or why there is none.
        outcome: CallOutcome,
    },
    /// The answer to an op.
    OpReply {
        /// The op's id.
        id: CallId,
        /// Its result, or why there is none.
        outcome: OpOutcome,
    },
    /// A node's connection changed.
    Node {
        /// The node.
        node: NodeKey,
        /// Where its connection is now.
        status: NodeStatus,
    },
    /// A record of a followed session.
    Event {
        /// The stream.
        stream: StreamId,
        /// The record (gap-free and in order, within one stream).
        record: EventRecord,
    },
    /// A followed session's stream changed.
    Stream {
        /// The stream.
        stream: StreamId,
        /// What happened.
        state: StreamState,
    },
    /// A frame the bridge could not read, and could not answer by id.
    Unreadable {
        /// Why.
        message: String,
    },
}

/// A call's answer.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// The node's reply (boxed: a reply is several hundred bytes, a
    /// failure a few words).
    Ok(Box<Reply>),
    /// No reply.
    Err(Failure),
}

/// Why a call has no reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    /// The node answered with an error.
    Remote {
        /// The node's error.
        error: WireError,
    },
    /// The node's protocol is older than the method; it was not sent.
    NotInProtocol {
        /// The method.
        method: Method,
        /// The version it needs.
        needs: ProtocolVersion,
        /// The node's.
        negotiated: ProtocolVersion,
    },
    /// The connection closed with the call in flight. It may or may not
    /// have run on the node, and it is not repeated.
    Disconnected,
    /// The node cannot be reached now.
    Unreachable {
        /// Why.
        message: String,
    },
    /// The bridge does not pass this call on: not a call it can read, a
    /// method a browser does not call, or too many calls at once.
    Refused {
        /// Why.
        message: String,
    },
    /// Anything else (a broken peer).
    Failed {
        /// What happened.
        message: String,
    },
}

/// An op's answer.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum OpOutcome {
    /// It worked.
    Ok(OpResult),
    /// It did not.
    Err {
        /// Why.
        message: String,
    },
}

/// What the browser asks the bridge itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// The nodes to show: the book's and this machine's.
    Nodes,
    /// Connect to a node again now (after it failed, or while waiting to
    /// retry).
    Retry {
        /// The node.
        node: NodeKey,
    },
    /// What a pairing link offers, before pairing with it. Nothing is
    /// sent anywhere.
    Inspect {
        /// The link (`vessel://pair#...`).
        link: String,
    },
    /// Pair this machine with the node a link offers and add it to the
    /// book.
    Pair {
        /// The name to give it in the book.
        name: String,
        /// The link.
        link: String,
    },
    /// Draw `text` as a QR code.
    Qr {
        /// What to encode (a pairing link).
        text: String,
    },
}

/// An op's result.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OpResult {
    /// [`Op::Nodes`].
    Nodes {
        /// The nodes, in the order to show them.
        nodes: Vec<BookNode>,
    },
    /// [`Op::Retry`].
    Retry,
    /// [`Op::Inspect`].
    Inspect {
        /// The offer.
        offer: PairingOffer,
        /// The fingerprint of its key, to compare with what the node
        /// shows.
        fingerprint: String,
    },
    /// [`Op::Pair`].
    Pair {
        /// The node as the book has it now.
        node: BookNode,
    },
    /// [`Op::Qr`].
    Qr {
        /// The code as an SVG document.
        svg: String,
    },
}

/// A node the UI lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct BookNode {
    /// How the browser names it.
    pub key: NodeKey,
    /// The book's name for it, or a description of this machine's node.
    pub label: String,
    /// Where it is, as the book has it (an address, an SSH target, a
    /// paired node's key fingerprint).
    pub address: String,
    /// How it is reached.
    pub transport: Transport,
    /// Whether commands use it when none is named.
    pub default: bool,
    /// Its id, once seen.
    pub id: Option<NodeId>,
}

/// How a node is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// A websocket and a token.
    Ws,
    /// SSH.
    Ssh,
    /// A paired node's routes (direct or through relays), end to end
    /// encrypted.
    Paired,
}

/// Where a node's connection is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NodeStatus {
    /// Nothing has asked for it yet.
    Idle,
    /// Dialling it.
    Connecting,
    /// Connected.
    Connected {
        /// The node, as it introduced itself.
        node: NodeInfo,
        /// The protocol version in use.
        protocol: ProtocolVersion,
    },
    /// The connection is down; dialling again in `retry_in_ms`.
    Reconnecting {
        /// Why it is down.
        message: String,
        /// Which attempt comes next, from 1.
        attempt: u32,
        /// Milliseconds until it.
        retry_in_ms: u64,
    },
    /// It cannot be reached in a way that waiting does not fix (no such
    /// node, a wrong token, a changed identity); `Op::Retry`, or the next
    /// call, tries again.
    Failed {
        /// Why.
        message: String,
    },
}

/// What happened to a followed session's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StreamState {
    /// Records `expected..resumed_at` were pruned by the node; the stream
    /// continues at `resumed_at`.
    Missing {
        /// The first record that was wanted.
        expected: EventSeq,
        /// Where it continues.
        resumed_at: EventSeq,
    },
    /// The node's connection is down; the stream resumes when it is back.
    Reconnecting {
        /// Why.
        message: String,
    },
    /// Connected again; the next record is `at`, nothing repeated.
    Resumed {
        /// The next record.
        at: EventSeq,
    },
    /// The session ended; no more records.
    Ended,
    /// The stream stopped (the session is gone, or the node cannot be
    /// reached again); follow again to retry.
    Failed {
        /// Why.
        message: String,
    },
}

/// `GET /api/status`: what is serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Status {
    /// The server's version (`vessel`'s).
    pub version: String,
    /// Whether the real UI is embedded (else the placeholder page).
    pub bundled: bool,
}

/// How the browser names a node: a book entry's name, or [`NodeKey::HERE`]
/// for this machine's node. Book names start with a letter or a digit, so
/// the two never meet.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct NodeKey(String);

impl NodeKey {
    /// This machine's node (the CLI's default when nothing names one).
    pub const HERE: &str = "_here";

    /// A key as given.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// This machine's node.
    #[must_use]
    pub fn here() -> Self {
        Self(Self::HERE.to_owned())
    }

    /// The key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether it is this machine's node.
    #[must_use]
    pub fn is_here(&self) -> bool {
        self.0 == Self::HERE
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The browser's id for a call or an op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct CallId(u64);

impl CallId {
    /// An id.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// The browser's id for a followed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct StreamId(u64);

impl StreamId {
    /// An id.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// A token as a browser presented it: not yet checked, never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct PresentedToken(String);

impl PresentedToken {
    /// The token as given.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The text, for comparing.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PresentedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PresentedToken(..)")
    }
}

/// The bridge's frames in TypeScript, beside the protocol's (ADR 0017).
#[cfg(feature = "ts")]
pub mod ts {
    use vessel_proto::ts::{Declarations, NameClash};

    use super::{ClientFrame, ServerFrame, Status};

    /// Where the web UI keeps them, from the workspace's root.
    pub const BRIDGE_FILE: &str = "web/src/wire/bridge.ts";

    /// The bridge's module: its frames, importing the protocol's types
    /// from `./proto`.
    ///
    /// # Errors
    /// Two types share a TypeScript name.
    pub fn module() -> Result<String, NameClash> {
        let proto = Declarations::wire()?;
        let mut bridge = Declarations::new();
        bridge.add::<ClientFrame>()?;
        bridge.add::<ServerFrame>()?;
        bridge.add::<Status>()?;
        Ok(bridge.render(Some((&proto, "./proto"))))
    }

    #[cfg(test)]
    mod tests {
        use std::path::Path;

        use vessel_proto::ts::{check_file, check_strict};

        use super::*;

        #[test]
        fn typescript_declarations_are_current() {
            let generated = module().unwrap();
            check_strict(&generated).unwrap();
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(BRIDGE_FILE);
            check_file(&path, &generated).unwrap();
        }
    }
}
