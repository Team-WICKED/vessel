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
   live ones, never before the `Subscribed` reply (ADR 0006). A subscriber
   that falls behind, on the node or in its own client, gets
   `SubscriptionEnded{Lagged{resume_from}}` and continues with `session.read`
   plus a new subscription (`vessel-client`'s `SessionStream` does this).
   Nothing is skipped silently.
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
- Runtime track, terminal and structured sessions working end to end:
  - `vessel-proc`: `CommandSpec`, `spawn` as a process-group leader,
    `ProcessGroup` (Unix). Stopping signals the whole group, and the group
    is swept once its leader exits. Windows terminates only the direct
    child (no job objects yet).
  - `vessel-harness`: one descriptor module per harness, `Lookup` (explicit
    search path), concurrent `discover` with a version-probe timeout,
    `launch` (working directory made absolute and checked). The `fakes`
    feature installs testkit binaries under real harness names for other
    crates' tests. Model selection is not here: it goes over ACP in
    `vessel-agent`.
  - `vessel-pty`: `PtyProcess` over `portable-pty`, with dedicated reader,
    control (input and resize, in order) and waiter threads behind bounded
    channels; modules `process`, `streams`, `threads`, `command`.
  - `vessel-agent`: ACP v1 client on `agent-client-protocol` 5.0.
    `AgentProcess` (own process group), `AgentInput` (prompt, cancel,
    permission answers with typed refusals), `AgentOutputs` (bounded).
    A single driver loop owns the session; handlers forward into one
    ordered channel. The model is chosen through the `model` config option.
    The `mapper` is pure and tested against fixtures in
    `crates/vessel-agent/tests/fixtures/acp/`.
  - `vessel-session`: the R6 API (`SessionRuntime`, `SessionHandle`,
    `Follow`/`FollowError`, `Created::durable`; records shared as
    `Arc<EventRecord>`). Terminal actor and structured actor share the
    `lifecycle` module (stop-responsive publishing, two-step stop).
    Structured status is `Starting` until the agent's `Ready`, then
    `Running`; a session that never became ready ends `Failed`.
  - `vessel-testkit`: `fake-tui`, `fake-acp` and the `Fake::path` locator.
- Spine track, transport and storage done:
  - `vessel-rpc`: handshake, typed calls with cancellation and an in-flight
    bound, per-connection outbound queue, subscription ordering and lag on
    both ends (ADR 0006), in-memory transport for tests.
  - `vessel-store`: data directory (`node.id`, 0600 `token`), SQLite event
    log with a batching writer thread, paged reads, restart recovery to
    `Interrupted`, retention, `user_version` migrations.
  - `vessel-client`: websocket connection with the bearer token, one typed
    method per call, gap-free `SessionStream` (`follow`). `wss://` is
    refused: no TLS stack in milestone 1 (ADR 0005 amendment).
- In progress (milestone 1): `vessel-node` and `vessel-cli`, which still
  contain the protocol 1 code and are excluded from the workspace until
  they are ported.

## Later

Peers, relay, SSH transport and self-update, sync, harness profiles, the
model router and the LSP bridge each get a plan and ADRs when they start. See
`ROADMAP.md`.
