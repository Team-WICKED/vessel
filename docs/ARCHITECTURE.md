# Architecture

## Pieces

```
  vessel (CLI)      editor plugins      web UI
        \                |                /
         \               |               /
          +--------- HTTP + WebSocket ---------+
                          |
                    vesseld (node)        <-- one per machine
                   /       |       \
             harnesses   sessions   (later: relay, sync, lsp, router)
          claude / codex / gemini / opencode
```

- **`vessel-core`** holds every type that crosses the wire. It has no I/O, so
  any client (CLI, editor, web) can depend on it without pulling in tokio or
  axum.
- **`vessel-node`** is the daemon. It owns processes. Everything stateful
  lives here.
- **`vessel-cli`** is the first client and the reference for how to talk to a
  node. It can also run a node in-process (`vessel node start`).

## Node internals

- `harness` finds installed harness binaries on `PATH` and probes
  `--version` with a timeout.
- `session` spawns a harness as a child process with piped stdio. Two pump
  tasks turn stdout and stderr into `OutputChunk` lines, kept in a bounded
  backlog (so late subscribers can catch up) and fanned out over a broadcast
  channel (so live subscribers get them as they happen). A supervisor task
  waits for the child, drains the pumps, then publishes the final status. Kill
  is a cancellation token the supervisor listens to.
- `routes` is the HTTP surface. The websocket handler sends the backlog, then
  live lines, then one final `status` event when the session ends.
- Identity: `~/.vessel/node.id` holds a UUID created on first run so the node
  id survives restarts.

## HTTP API (protocol 1)

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/health` | | `ok` |
| GET | `/node` | | `NodeInfo` |
| GET | `/harnesses` | | `Harness[]` |
| GET | `/sessions` | | `SessionInfo[]` newest first |
| POST | `/sessions` | `SessionSpec` | `201 SessionInfo` |
| GET | `/sessions/{id}` | | `SessionInfo` |
| DELETE | `/sessions/{id}` | | `202` |
| POST | `/sessions/{id}/input` | `SessionInput` | `204` |
| GET | `/sessions/{id}/stream` | websocket | `SessionEvent` per message |

Errors are `{"error": "..."}` with a fitting status code.

## Design rules

- The node binds to loopback by default. Exposing it to a network is a
  deliberate choice and, once the relay exists, goes through the relay.
- Harnesses are run as-is. Vessel never re-implements a harness; it wraps it.
- Lints forbid panics, `unwrap`, indexing, and raw arithmetic. A node must not
  die because one session misbehaved.

## Not yet designed

Peers, relay, sync, the model router, and the LSP bridge. Each gets its own
document when it starts. See [ROADMAP.md](ROADMAP.md).
