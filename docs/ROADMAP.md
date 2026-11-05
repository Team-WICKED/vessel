# Roadmap

Rough order. Each phase ends with something usable on its own.

## Phase 0: one node, one machine (now)

- [x] Workspace, lints, CI.
- [x] Harness discovery.
- [x] Spawn a harness, stream output, send input, kill.
- [x] CLI: status, harnesses, sessions, run, attach, send, kill.
- [ ] Interactive attach (forward local stdin).
- [ ] Session persistence across node restarts (at least the log).
- [ ] Auth token for non-loopback binds.

## Phase 1: many nodes

- [ ] Peer list and node identity exchange.
- [ ] Relay: nodes reach each other without port-forwarding (evaluate
      `tsnet` vs WireGuard vs a hosted rendezvous).
- [ ] `vessel` CLI addresses any node by name: `vessel --node gpu-box run ...`.

## Phase 2: fleet sync

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
