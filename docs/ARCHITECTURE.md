# Architecture

Target shape of the backend after milestone 1. Status per crate is in
"State" below; the plan is `docs/plans/backend-m1.md`, the reasons are in
`docs/adr/`.

## Pieces

```
  vessel CLI        editor plugins        web UI          (clients)
        \                 |                  /
         +---- protocol 2: typed frames over one connection ----+
                 (websocket today; SSH stdio and relay later)
                                |
                       vesseld (one per machine)
          +---------------------+----------------------+
          |                     |                      |
     rpc server            session runtime          store
     (auth, dispatch,      (one actor per session)  (SQLite event log,
      subscriptions)          /            \         identity, token)
                       terminal mode   structured mode
                       (PTY, raw TUI)  (ACP client)
                              \            /
                       claude / codex / gemini / opencode
```

## Crates

| Crate | Job | Depends on |
| --- | --- | --- |
| `vessel-proto` | Wire contract: ids, sessions, events, agent timeline, errors, typed RPC table. No I/O. | - |
| `vessel-rpc` | Moves `ClientFrame`/`ServerFrame` over any frame transport; handshake, correlation, concurrency limits, generic over `Handler`. | proto |
| `vessel-proc` | Command specs, spawning in a process group, killing the group. | - |
| `vessel-harness` | Everything harness-specific, one module per harness: discovery, version probe, terminal and ACP launch commands. | proto, proc |
| `vessel-pty` | Runs a command in a PTY; byte I/O and resize. | proc |
| `vessel-agent` | ACP client: drives a harness or adapter over stdio and maps ACP updates to `AgentEvent`s. | proto, proc |
| `vessel-session` | Session actors: owns the process, assigns `EventSeq`, keeps a ring cache, broadcasts, hands every record to the node for persistence. | proto, harness, pty, agent |
| `vessel-store` | Data directory (`node.id`, `token`, `vessel.db`), SQLite event log, retention, restart recovery. | proto |
| `vessel-node` | `vesseld`: config, auth, HTTP/websocket server, `Handler` implementation, persistence and subscription plumbing. | proto, rpc, harness, session, store |
| `vessel-client` | Typed async client and gap-free session streams. | proto, rpc |
| `vessel-cli` | `vessel` binary: commands, interactive attach. | proto, client, node |
| `vessel-testkit` | Fake harnesses (`fake-tui`, `fake-acp`) for tests. | - |

The dependency direction is fixed by ADR 0001; adding an edge means updating
that ADR and this table.

## Data flow

1. A client opens `/rpc` with `Authorization: Bearer <token>` (ADR 0005),
   sends `Hello` with its protocol range, and gets `Welcome` with the
   negotiated version and `NodeInfo`.
2. `session.create` asks `vessel-harness` for a launch command, and
   `vessel-session` starts an actor in terminal mode (`vessel-pty`) or
   structured mode (`vessel-agent`).
3. The actor turns process output into `SessionEvent`s, numbers them
   (`EventSeq`, gap-free from zero), caches them in a ring, pushes each one
   into the bounded durable channel (the node writes it to the store), and
   broadcasts it to live followers.
4. `session.subscribe{from}` sends cached and stored records from `from`, then
   live ones. A subscriber that falls behind gets
   `SubscriptionEnded{Lagged{resume_from}}` and continues with `session.read`
   plus a new subscription. Nothing is skipped silently.
5. `session.send` routes `SessionInput` to the actor: terminal bytes and
   resizes, or prompts, cancels and permission answers. The first answer to a
   permission request wins; the resolution is an event, so every client sees
   it.
6. On shutdown the node kills each session's process group, waits for final
   statuses, and flushes the store. On the next start, sessions that were live
   are marked `Interrupted`. Their logs stay readable, and structured ones can
   be resumed through their `HarnessSessionId`.

## Design rules

- The daemon is the product; clients are thin.
- Harnesses are wrapped, never re-implemented (ADR 0003).
- Wire types exist once, in `vessel-proto`.
- No panics, no unbounded queues, no untyped JSON (see `AGENTS.md`).

## State (2026-11-05)

- Done: `vessel-proto` (protocol 2 contract).
- In progress (milestone 1): every other crate. `vessel-node` and
  `vessel-cli` still contain the protocol 1 code and are excluded from the
  workspace until they are ported.

## Later

Peers, relay, SSH transport and self-update, sync, harness profiles, the
model router and the LSP bridge each get a plan and ADRs when they start. See
`ROADMAP.md`.
