# Roadmap

Rough order. Each phase ends with something usable on its own.

## Where it stands (2026-11-07)

Milestones 1 to 4 are done and on `main`: they were built on stacked
branches (`backend/m1` to `backend/m4`, each on top of the one before) and
merged together from `backend/m4` once it was green on Linux, macOS and
Windows and every plan's manual smokes had passed. The branches stay as
the record. Each milestone's plan in `docs/plans/` records what was built,
what was checked by hand, and what it left open.

## Phase 0: one node, one machine (milestone 1, done)

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

## Phase 1: many nodes (milestones 2 and 3, done)

Plans: `docs/plans/backend-m2.md` (ADRs 0007 and 0008) and
`docs/plans/backend-m3.md` (ADRs 0009 and 0010).

- [x] SSH stdio transport (`vesseld attach`, Zed's proxy/daemon split).
- [x] Versioned side-by-side `vesseld` install and update on a remote.
- [x] `vessel` CLI addresses any node by name: `vessel --node gpu-box run ...`
      (the node book, ids pinned on first contact).
- [x] Relay: nodes and clients reach each other with no SSH and no open
      port. A dumb relay built into `vesseld` (`vesseld relay`),
      end-to-end encrypted with Noise NK, direct when the node is
      reachable. Chosen on 2026-11-06 over `tsnet` or WireGuard: no
      third-party account, the overlay stays out of the trust base, one
      binary (VISION section 2, ADR 0009).
- [x] Pairing and device principals: an expiring, single-use pairing link
      creates a revocable credential per device (ADR 0010).
- [x] Node-to-node calls: built in milestone 4 as calls through the fleet's
      home (protocol 6, below), not as a mesh.
- [x] By hand: a relay over `wss://` behind an HTTPS proxy on a public
      host (the first real TLS test), passed on 2026-11-07 (milestone 3's
      plan, "Smoke on one machine").

## Phase 2: the fleet (milestone 4, done)

Plan: `docs/plans/backend-m4.md` (ADRs 0011, 0012 and 0013; protocols 5
and 6).

- [x] Harness profiles: one harness, several accounts or configurations
      (`profiles.toml`), chosen per session, and a structured session moved
      to another profile without losing its conversation (ADR 0011).
- [x] Fleet sync: one home node shares instructions and skills, and every
      member follows them into every profile, keeping local edits; MCP
      servers handed to harnesses at launch; secrets set per node, or
      copied to a node that granted it (ADR 0012).
- [x] Conversations and memory copied or moved on request, between
      profiles and between nodes (`vessel session copy|move`, `vessel fleet
      memory copy`).
- [x] First-run setup: `vessel init` sets a machine up (node, harnesses,
      profiles, logins, `--service`) and joins a fleet with `--join`.
- [x] Agent tools: every session's agent gets the `vessel` MCP server with
      a credential of its own, starts and drives sessions of its own on
      any node of the fleet, with scopes that only shrink and limits on
      count and depth (ADR 0013, protocol 6).
- [x] No secret value in a bundle, a log, `profile.list` or a command line
      (a test plants values and searches everything a fleet run wrote).
- [x] Closing: the two manual smokes of the plan's done criteria, run by
      the coordinator with the scripts in `docs/smoke/` (2026-11-07): a
      structured session moved through the operator's three real accounts
      and back, and a fresh machine running `vessel init --join`. The
      security review closed after eleven rounds (the plan's "Security
      review").

## Next

Not planned yet. In roadmap order the next milestone is Phase 3. Milestone
4 left these follow-ups, none scheduled:

- Agent limits per fleet: today they are per node, so an agent may own 8
  live sessions on each node it reaches (ADR 0013, "Known limits").
- A relay grant with capped scopes (a read-only relay), which needs a
  field on invitations and devices, so a protocol change (ADR 0013).
- Agent grants per profile (`[profiles.X.agent]`, ADR 0013).
- A node id derived from the node's key, so pairing proves the id and the
  home's first-grant binding follows from the key alone (proposed by the
  security review; a protocol version and a migration of existing ids).
- `vessel init --service` on Windows (systemd and launchd only today).
- The first tagged release, and signing: the release workflow exists but
  has never run.
- A clean websocket close when `vessel` exits (the node logs "ended
  abnormally" today; harmless).

## Phase 3: model routing

- [ ] Local routing proxy so every harness can use every model.

## Phase 4: IDE context

- [ ] LSP client inside the node.
- [ ] Expose symbols, references, diagnostics as MCP tools.

## Phase 5: clients

- [x] Web UI: `vessel ui` (milestone 7, ADR 0017). Follow-ups: `vesseld
  --web` (a node serving the page itself); Safari, which does not resolve
  the per-launch `vessel-<id>.localhost` origins (revisit with a signed
  bootstrap page or Safari's `localhost` port isolation; ADR 0017).
- [ ] Zed / VS Code integration.
- [ ] Native editor (only if the above is not enough).
