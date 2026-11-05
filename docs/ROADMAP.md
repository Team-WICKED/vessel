# Roadmap

Rough order. Each phase ends with something usable on its own.

## Phase 0: one node, one machine (milestone 1, in progress)

Plan: `docs/plans/backend-m1.md`.

- [x] Workspace, lints, CI.
- [x] Protocol 2 contract (`vessel-proto`): typed RPC, event log, ACP-shaped
      agent timeline.
- [ ] Terminal mode (PTY) and structured mode (ACP) sessions.
- [ ] Durable session log with resume by sequence number (SQLite).
- [ ] Token auth on every connection.
- [ ] Typed client library; CLI with interactive attach.

## Phase 1: many nodes

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
