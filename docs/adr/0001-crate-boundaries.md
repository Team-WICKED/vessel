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
vessel-node     -> proto, rpc, harness, session, store
vessel-client   -> proto, rpc
vessel-cli      -> proto, client, node
vessel-testkit  (fake harness binaries for tests; dev-dependency only)
```

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
