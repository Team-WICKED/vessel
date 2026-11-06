# Roadmap

Rough order. Each phase ends with something usable on its own.

## Phase 0: one node, one machine (milestone 1, implemented 2026-11-05)

Plan: `docs/plans/backend-m1.md`.

- [x] Workspace, lints, CI.
- [x] Protocol 2 contract (`vessel-proto`): typed RPC, event log, ACP-shaped
      agent timeline.
- [x] Terminal mode (PTY) and structured mode (ACP) sessions.
- [x] Durable session log with resume by sequence number (SQLite).
- [x] Token auth on every connection.
- [x] Typed client library; CLI with interactive attach.
- [x] Verified on Linux, macOS and Windows in CI (first all-green run
      42141788299, 2026-11-06). Windows structured sessions stop their whole
      process tree (job objects); Windows PTY sessions terminate the direct
      child and rely on the pseudo console closing for the rest.

## Phase 1: many nodes

Milestone 2 (SSH transport, remote install and update, named nodes) is
implemented on `backend/m2` (plan `docs/plans/backend-m2.md`, ADRs 0007 and
0008). Milestone 3, the relay, is drafted in `docs/plans/backend-m3.md`, with
proposed ADRs 0009 and 0010.

- [x] SSH stdio transport (`vesseld attach`, Zed's proxy/daemon split).
- [x] Versioned side-by-side `vesseld` install and update on a remote.
- [x] `vessel` CLI addresses any node by name: `vessel --node gpu-box run ...`
      (the node book, ids pinned on first contact).
- [ ] Relay (milestone 3): nodes and clients reach each other with no SSH and
      no open port.
  - A dumb relay built into `vesseld` (`vesseld relay`), end-to-end
    encrypted with Noise NK, direct when the node is reachable.
  - Decided 2026-11-06 instead of `tsnet` or WireGuard: no third-party
    account, the overlay stays out of the trust base, one binary. See
    VISION section 2 and ADR 0009.
- [ ] Pairing and device principals (milestone 3): an expiring, single-use
      pairing link creates a revocable credential per device (ADR 0010).
- [ ] Peers: node-to-node routed calls (after milestone 3).

## Phase 2: fleet sync

- [ ] Harness profiles (one harness, several accounts or configs).
- [ ] Sync skills, MCP server configs, memory, and conversation history.
- [ ] First-run setup: `vessel init` on a fresh machine pulls the fleet's
      config and installs harnesses.
- [ ] Agent-to-agent visibility: a session on one node can list and spawn
      sessions on another.

## Phase 3: model routing

- [ ] Local routing proxy so every harness can use every model.

## Phase 4: IDE context

- [ ] LSP client inside the node.
- [ ] Expose symbols, references, diagnostics as MCP tools.

## Phase 5: clients

- [ ] Web UI.
- [ ] Zed / VS Code integration.
- [ ] Native editor (only if the above is not enough).
