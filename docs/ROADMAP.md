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
- [ ] Verified on Windows and macOS in CI (Linux only so far). Windows
      structured sessions stop their whole process tree (job objects);
      Windows PTY sessions terminate the direct child and rely on the
      pseudo console closing for the rest.

## Phase 1: many nodes

Milestone 2 (SSH transport, remote install and update, named nodes) is
drafted in `docs/plans/backend-m2.md`, with proposed ADRs 0007 and 0008;
the relay follows in milestone 3.

- [ ] SSH stdio transport (`vesseld attach`, Zed's proxy/daemon split).
- [ ] Versioned side-by-side `vesseld` install and update on a remote.
- [ ] Peer list and node identity exchange.
- [ ] Relay: nodes reach each other without port-forwarding (evaluate
      `tsnet` vs WireGuard vs a hosted rendezvous).
- [ ] `vessel` CLI addresses any node by name: `vessel --node gpu-box run ...`.

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
