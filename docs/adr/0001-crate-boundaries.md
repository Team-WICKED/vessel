# ADR 0001: Crate boundaries and dependency direction

Status: accepted, 2026-11-05

## Context

Phase 0 had three crates: `vessel-core` (types), `vessel-node` (everything
else, including process handling and HTTP) and `vessel-cli`. The backend is
about to grow a PTY runner, an ACP client, persistence, a typed RPC layer and
a client library, built by two agents in parallel. Without hard boundaries
this becomes Paseo's `session.ts` or Zed's `acp_thread.rs`.

## Options

1. Keep three crates, split by modules. Cheap, but module privacy is weak,
   every change recompiles the whole node, and two agents would edit the same
   crate.
2. Fine-grained crates by responsibility (Zed has ~250). Boundaries are
   enforced by the compiler, incremental builds stay small, and each agent
   owns whole crates.
3. A handful of mid-size crates (protocol, runtime, server, client).
   Middle ground, but "runtime" would mix PTY, ACP and supervision.

## Decision

Option 2, with this map and only these dependency edges:

```
vessel-proto    (wire contract; no I/O, no tokio)
vessel-rpc      -> proto
vessel-proc     (process spawning, process groups, kill tree)
vessel-harness  -> proto, proc
vessel-pty      -> proc
vessel-agent    -> proto, proc
vessel-session  -> proto, harness, pty, agent
vessel-store    -> proto
vessel-node     -> proto, rpc, harness, session, store, secure, relay
vessel-client   -> proto, rpc, secure, relay
vessel-remote   -> proto, rpc, client      (milestone 2)
vessel-secure   -> proto                   (milestone 3)
vessel-relay    -> proto, secure           (milestone 3)
vessel-cli      -> proto, client, node, remote
vessel-testkit  (fake harness binaries for tests; dev-dependency only)
```

Milestone 2 (2026-11-06) added `vessel-remote`, which runs the system
`ssh`, installs `vesseld` on remote machines and connects to them over SSH
stdio (ADRs 0007 and 0008), and the edge `vessel-cli -> remote`. It starts
`ssh` with `tokio::process` directly: one process, no process group, so no
`vessel-proc` edge.

Milestone 3 (2026-11-06, Track S) adds `vessel-secure`, the end-to-end
secure channel (ADR 0009). Its one edge, `-> proto`, is for the key types
that travel on the wire (`NodePublicKey`, `RoutingId`, the relay's
`HandshakeMessage`); everything computed from them with cryptography stays
in `vessel-secure`, so `vessel-proto` stays free of it. `vessel-relay`
(`-> proto, secure`) is the relay server, the node's link to it and the
client's dial, and the websocket carriers both ends use. Track P added
`vessel-node -> secure` (answering handshakes with the node's key pair) and
`vessel-client -> secure` (initiating them to a pinned key, and
fingerprints). `vessel-node` gains `-> relay` for `vesseld relay` and, with
Track P's integration, `/secure` and relay data connections. The client's
`-> relay` (Track P's integration) is for `connect`, `dial`, `WsCarrier`,
the close codes and `TlsConfig`: `SecureConnector`'s websocket dialer
(`WsDialer`).
`vessel-testkit` (dev-only) has a `relay` feature: a misbehaving relay
proxy built on `tokio-tungstenite`, with no edge to Vessel crates.

`vessel-core` is renamed `vessel-proto` because its one job is the wire
contract.

## Why

- The compiler enforces the boundaries; reviews do not have to.
- Each piece can be tested alone: the ACP mapping without processes, the
  store without a node, the RPC layer over an in-memory transport.
- Two agents can work in parallel on disjoint crates.
- `vessel-session` does not depend on `vessel-store`: the session runtime
  hands the node a bounded channel of records to persist. Persistence policy
  stays in the node, and the runtime stays testable without a disk.

## Consequences

- More `Cargo.toml` files and some re-exports.
- A new edge needs an update to this ADR and `docs/ARCHITECTURE.md`.
- Shared helpers have no grab-bag home; they go to the lowest crate whose job
  they are.
