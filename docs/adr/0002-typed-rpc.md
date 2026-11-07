# ADR 0002: One typed RPC connection instead of REST routes

Status: accepted, 2026-11-05. Protocol versions 3 to 6 extend the RPC table
and keep its rules (`docs/PROTOCOL.md`).

## Context

Protocol 1 was REST routes plus one websocket per session stream. Each route
had its own JSON shape and the client re-declared the mapping. The relay
(Phase 1) and SSH transport want a single byte stream per client, and
unhandled routes were only found at runtime.

## Options

1. Keep REST, add routes. Familiar, but no shared method table, and every
   transport (relay, SSH stdio) would need an HTTP shim.
2. Zed style: protobuf envelopes, macro-generated request/response binding,
   handlers registered dynamically by `TypeId`.
3. One connection carrying JSON frames (`ClientFrame`/`ServerFrame`), one
   macro table generating the request/response binding, a `Handler` trait with
   one method per call, and an exhaustive `dispatch`.

## Decision

Option 3 (protocol version 2):

- Frames: `Hello`/`Welcome`/`Rejected` handshake with a protocol range,
  `Request{id, call}`/`Response{id, outcome}`, `Cancel{id}`,
  `Event{subscription, record}`, `SubscriptionEnded{subscription, end}`.
- `rpc_methods!` in `vessel-proto` is the only definition of the method set.
- Transport for now: a websocket at `/rpc` with text frames; `GET /health`
  stays for probes. Every transport (websocket, SSH stdio, relay) only moves
  frames, so `vessel-rpc` is generic over the frame transport.

## Why

- Adding a method without handling it does not compile.
- `call::<R>()` returns `R::Response`, typed end to end, with no `Value`.
- Static dispatch instead of Zed's `TypeId` map: no `Any`, no downcasts.
- JSON text frames stay debuggable and usable from a browser. A binary codec
  can replace the encoding later without touching the types.

## Consequences

- Protocol 1 clients stop working; there are none outside this repo.
- Terminal bytes travel base64-encoded (about 33% overhead); see
  `TerminalBytes`.
- Breaking wire changes bump `PROTOCOL_VERSION` and are logged in
  `docs/PROTOCOL.md`.
