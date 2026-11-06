# ADR 0006: Subscription ordering and lag on both ends of a connection

Status: accepted, 2026-11-05 (Track S, `vessel-rpc`).

## Context

`session.subscribe` answers with `Subscribed{subscription}` and the node
then pushes `Event{subscription}` frames. Two races lose the first events of
a subscription:

1. On the node, the handler starts a forwarder task and returns. The
   forwarder can queue an `Event` before the connection queues the
   `Subscribed` reply, so the client sees events for an id it has not been
   told about yet.
2. On the client, the plan sketched one shared `Events` stream next to the
   call API. The connection reads `Subscribed{S}` and then `Event{S}` back
   to back; a router fed by that stream cannot know `S` until the caller who
   received the reply registers it, which happens later.

A third problem is lag on the client. If a subscriber stops reading and its
buffer fills, the connection must either block or drop. Blocking stalls
every call on the connection, including the `session.read` the subscriber
needs to catch up: a deadlock.

## Options

1. Clients buffer events for unknown ids until a reply names them.
   Unbounded, and wrong for a cancelled subscribe whose reply never comes.
2. The node delays its forwarder. The handler cannot know when the
   connection queued its reply.
3. `vessel-rpc` owns subscription lifecycles on both ends.

## Decision

Option 3.

- Node side: `Outbound::open_subscription` mints the id and returns a
  `SubscriptionSink`. The connection's writer owns the subscription table.
  Each sink holds a one-shot "announced" gate that the writer fires when it
  takes the `Subscribed` reply naming that id off the queue, so every event
  is written after its announcement. After `SubscriptionEnded` the writer
  drops any frame still queued for that id. `session.unsubscribe` goes
  through the writer too: the client receives `SubscriptionEnded{Unsubscribed}`
  before the call's `Ack`.
- Release: every sink carries a departure signal the writer watches. A
  sink that ends cleanly says so; one dropped without ending is noticed
  reliably (no best-effort message from `Drop`). Before its announcement
  the subscription is discarded silently; after it, the client gets
  `SubscriptionEnded{Lagged{resume_from}}` at the first record the writer
  did not write, so a crashed or buggy forwarder costs the client a
  resume, never a hang. Departures are handled after the queue, so records
  queued before the drop still go out.
- Client side: `Connection::subscribe` (not the generic `call`, which a
  `Unary` marker trait excludes `Subscribe` from at compile time) returns a
  `Subscription` with its own bounded channel, created by the connection's
  reader in the same step that reads the reply.
- The client's reader never blocks. When a subscription's channel is full,
  it ends that subscription locally with `Lagged{resume_from}` (the same
  promise the node makes: nothing skipped) and unsubscribes on the node.
  `vessel-client`'s `SessionStream` then pages the gap with `session.read`
  and subscribes again from its cursor.

## Why

- Bounded everywhere: the gate holds nothing (the forwarder waits), and a
  slow subscriber costs a few reads, never memory.
- Failed or cancelled subscribe calls simply never open: the sink is
  dropped, and a late reply for a cancelled call is unsubscribed by the
  client.
- One implementation for every transport, since all of it lives in
  `vessel-rpc`.

## Consequences

- `vessel-rpc`'s writer recognises the `session.subscribe` reply by type.
  That is protocol knowledge, which is that crate's job; it is the only
  method it singles out.
- Protocol guarantee worth stating in `docs/PROTOCOL.md`: a node never sends
  `Event{S}` before the response carrying `Subscribed{S}`, and never sends a
  frame for `S` after `SubscriptionEnded{S}`.
