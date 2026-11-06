# ADR 0003: Two session modes; structured mode speaks ACP

Status: accepted, 2026-11-05

## Context

ORIGIN item 2: wrap harnesses so their features work as-is, while remote
clients still get a useful view. Phase 0 piped stdout line by line, which
breaks every TUI harness and gives clients no structure.

## Options

1. PTY only. Every harness works exactly as locally, but clients see only
   terminal bytes.
2. Native protocol per harness, as Paseo does: Claude's stream-json control
   protocol, `codex app-server`, OpenCode's HTTP server, and so on. Highest
   fidelity, but one large adapter per harness (Paseo's are 5-8k lines each)
   and constant churn.
3. ACP (Agent Client Protocol: JSON-RPC 2.0 over stdio), as Zed does. Gemini
   CLI and OpenCode (`opencode acp`) speak it natively. Claude Code and Codex
   have maintained adapters (`claude-agent-acp`, `codex-acp`). One client
   implementation covers all four harnesses.

## Decision

Two modes, chosen per session (`SessionMode`):

- **Terminal**: the harness's own TUI in a PTY (`vessel-pty`). Raw bytes, any
  number of attached clients, resize.
- **Structured**: an ACP client in `vessel-agent` drives the harness (or its
  adapter). ACP `session/update`s map to Vessel's own `AgentEvent`s.
  `session/request_permission` becomes a `PermissionRequested` event with a
  node-minted `PermissionId`; the first client answer wins and the
  resolution is broadcast.

`vessel-agent` pins one ACP crate version and keeps ACP types internal. Each
transport inside it is split into transport (I/O), mapper (pure ACP-to-Vessel
conversion, unit-tested with fixtures) and session (actor) modules. A native
transport (for example Claude stream-json) can be added later behind the same
`AgentEvent` output if ACP adapters prove too lossy.

## Why

- One implementation instead of four, and the Rust ACP crate gives typed
  messages, so no hand-written JSON decoding.
- ACP is what editors speak (Zed, JetBrains). A later Vessel ACP endpoint lets
  an editor attach to a remote harness session directly.
- The adapters run the real harness underneath (the Claude adapter uses the
  Claude Agent SDK, which runs the `claude` binary), so skills, MCP servers and
  settings still apply.

## Consequences

- Claude and Codex structured mode requires their adapter on the node's
  `PATH`. Discovery reports `StructuredSupport::Unavailable` with an install
  hint instead of fetching it at runtime with `npx`.
- Adapter lag: new harness features reach structured mode only when the
  adapter supports them. Terminal mode always has them.
- Resume uses ACP `session/load` with the stored `HarnessSessionId`, where the
  agent supports it.
- Backpressure through the SDK (amended 2026-11-06, Track S review): the
  SDK (5.0) reads agent output into unbounded queues and expects the
  transport to push back. `vessel-agent` therefore feeds it its own line
  stream, which holds each `session/update` notification and
  `session/request_permission` request back until the driver has taken
  the previous ones (64 in flight); a stalled consumer stops the reading
  and the agent blocks on write. Not bounded: responses to Vessel's own
  requests (a handful outstanding at most) and methods Vessel does not
  handle, which the SDK answers or drops itself; those can only queue up
  while the driver is stalled and the agent sends unhandled methods.
  Messages Vessel sends to the agent go through the SDK's unbounded
  outgoing queue too; they are few (requests and answers) and need no
  bound.
