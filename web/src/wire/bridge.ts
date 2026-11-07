// Generated from the Rust wire types (ADR 0017). Do not edit: running
// `VESSEL_WRITE_TS=1 cargo test --features ts typescript` in the crate that
// generates this file rewrites it, and CI fails while it differs.

import type { Call, EventRecord, EventSeq, Method, NodeId, NodeInfo, PairingOffer, ProtocolVersion, Reply, SessionId, WireError } from "./proto";

/**
 * A node the UI lists.
 */
export type BookNode = { 
/**
 * How the browser names it.
 */
key: NodeKey, 
/**
 * The book's name for it, or a description of this machine's node.
 */
label: string, 
/**
 * Where it is, as the book has it (an address, an SSH target, a
 * paired node's key fingerprint).
 */
address: string, 
/**
 * How it is reached.
 */
transport: Transport, 
/**
 * Whether commands use it when none is named.
 */
default: boolean, 
/**
 * Its id, once seen.
 */
id: NodeId | null, };

/**
 * The browser's id for a call or an op.
 */
export type CallId = number;

/**
 * A call's answer.
 */
export type CallOutcome = { "ok": Reply } | { "err": Failure };

/**
 * A frame from the browser.
 */
export type ClientFrame = { "type": "hello", 
/**
 * The token from the page's URL.
 */
token: PresentedToken, } | { "type": "call", 
/**
 * The browser's id for the call, echoed in the reply.
 */
id: CallId, 
/**
 * The node.
 */
node: NodeKey, 
/**
 * The protocol's own `Call` (`{ "method", "params" }`). Kept as
 * text until the frame is read, then decoded into the typed
 * `Call`, so that a call this bridge cannot read is still
 * answered, by id.
 */
call: Call, } | { "type": "follow", 
/**
 * The browser's id for the stream.
 */
stream: StreamId, 
/**
 * The node.
 */
node: NodeKey, 
/**
 * The session.
 */
session: SessionId, 
/**
 * The first record wanted.
 */
from: EventSeq, } | { "type": "unfollow", 
/**
 * The stream.
 */
stream: StreamId, } | { "type": "op", 
/**
 * The browser's id for the request, echoed in the answer.
 */
id: CallId, 
/**
 * What to do.
 */
op: Op, };

/**
 * Why a call has no reply.
 */
export type Failure = { "kind": "remote", 
/**
 * The node's error.
 */
error: WireError, } | { "kind": "not_in_protocol", 
/**
 * The method.
 */
method: Method, 
/**
 * The version it needs.
 */
needs: ProtocolVersion, 
/**
 * The node's.
 */
negotiated: ProtocolVersion, } | { "kind": "disconnected" } | { "kind": "unreachable", 
/**
 * Why.
 */
message: string, } | { "kind": "refused", 
/**
 * Why.
 */
message: string, } | { "kind": "failed", 
/**
 * What happened.
 */
message: string, };

/**
 * How the browser names a node: a book entry's name, or [`NodeKey::HERE`]
 * for this machine's node. Book names start with a letter or a digit, so
 * the two never meet.
 */
export type NodeKey = string;

/**
 * Where a node's connection is.
 */
export type NodeStatus = { "state": "idle" } | { "state": "connecting" } | { "state": "connected", 
/**
 * The node, as it introduced itself.
 */
node: NodeInfo, 
/**
 * The protocol version in use.
 */
protocol: ProtocolVersion, } | { "state": "reconnecting", 
/**
 * Why it is down.
 */
message: string, 
/**
 * Which attempt comes next, from 1.
 */
attempt: number, 
/**
 * Milliseconds until it.
 */
retry_in_ms: number, } | { "state": "failed", 
/**
 * Why.
 */
message: string, };

/**
 * What the browser asks the bridge itself.
 */
export type Op = { "op": "nodes" } | { "op": "retry", 
/**
 * The node.
 */
node: NodeKey, } | { "op": "inspect", 
/**
 * The link (`vessel://pair#...`).
 */
link: string, } | { "op": "pair", 
/**
 * The name to give it in the book.
 */
name: string, 
/**
 * The link.
 */
link: string, } | { "op": "qr", 
/**
 * What to encode (a pairing link).
 */
text: string, };

/**
 * An op's answer.
 */
export type OpOutcome = { "ok": OpResult } | { "err": { 
/**
 * Why.
 */
message: string, } };

/**
 * An op's result.
 */
export type OpResult = { "op": "nodes", 
/**
 * The nodes, in the order to show them.
 */
nodes: Array<BookNode>, } | { "op": "retry" } | { "op": "inspect", 
/**
 * The offer.
 */
offer: PairingOffer, 
/**
 * The fingerprint of its key, to compare with what the node
 * shows.
 */
fingerprint: string, } | { "op": "pair", 
/**
 * The node as the book has it now.
 */
node: BookNode, } | { "op": "qr", 
/**
 * The code as an SVG document.
 */
svg: string, };

/**
 * A token as a browser presented it: not yet checked, never printed.
 */
export type PresentedToken = string;

/**
 * A frame to the browser.
 */
export type ServerFrame = { "type": "welcome", 
/**
 * The server's version (`vessel`'s).
 */
version: string, } | { "type": "reply", 
/**
 * The call's id.
 */
id: CallId, 
/**
 * The node's reply, or why there is none.
 */
outcome: CallOutcome, } | { "type": "op_reply", 
/**
 * The op's id.
 */
id: CallId, 
/**
 * Its result, or why there is none.
 */
outcome: OpOutcome, } | { "type": "node", 
/**
 * The node.
 */
node: NodeKey, 
/**
 * Where its connection is now.
 */
status: NodeStatus, } | { "type": "event", 
/**
 * The stream.
 */
stream: StreamId, 
/**
 * The record (gap-free and in order, within one stream).
 */
record: EventRecord, } | { "type": "stream", 
/**
 * The stream.
 */
stream: StreamId, 
/**
 * What happened.
 */
state: StreamState, } | { "type": "unreadable", 
/**
 * Why.
 */
message: string, };

/**
 * `GET /api/status`: what is serving.
 */
export type Status = { 
/**
 * The server's version (`vessel`'s).
 */
version: string, 
/**
 * Whether the real UI is embedded (else the placeholder page).
 */
bundled: boolean, };

/**
 * The browser's id for a followed session.
 */
export type StreamId = number;

/**
 * What happened to a followed session's stream.
 */
export type StreamState = { "state": "missing", 
/**
 * The first record that was wanted.
 */
expected: EventSeq, 
/**
 * Where it continues.
 */
resumed_at: EventSeq, } | { "state": "reconnecting", 
/**
 * Why.
 */
message: string, } | { "state": "resumed", 
/**
 * The next record.
 */
at: EventSeq, } | { "state": "ended" } | { "state": "failed", 
/**
 * Why.
 */
message: string, };

/**
 * How a node is reached.
 */
export type Transport = "ws" | "ssh" | "paired";
