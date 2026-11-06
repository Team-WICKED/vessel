# Architecture

The backend as built through milestone 4 (2026-11-06). What each crate
holds now is in "State" below; each milestone's plan is in `docs/plans/`,
and the reasons are in `docs/adr/`.

## Pieces

```
  vessel CLI      agents (`vessel mcp`)      editors, web UI (later)   (clients)
        \                 |                        /
   protocol 2..=6: typed frames over one connection, which is one of
   a websocket with the token, the local socket, SSH stdio (`vesseld
   attach`), or a Noise channel to the node's key, direct or relayed
                                  |
                       vesseld (one per machine)
     +-------------+--------------+--------------+---------------+
     |             |              |              |               |
  rpc server    session        store          fleet          secure gateway
  (principals,  runtime        (SQLite log,   (home or       (paired devices,
   scopes, the  (one actor     identity,      member;        relay links)
   envelope)    per session)   devices)       transfers,
                /        \                    secrets)
     terminal mode      structured mode
     (PTY, raw TUI)     (ACP client)
                \        /
     claude / codex / gemini / opencode, each under a profile

  vesseld relay: splices end-to-end encrypted connections, keeps nothing
```

A fleet is nodes of this shape: one is the home, the others are members
that follow it, and calls between nodes go through the home (ADRs 0012
and 0013).

## Crates

| Crate | Job | Depends on |
| --- | --- | --- |
| `vessel-proto` | Wire contract: ids, sessions, events, agent timeline, errors, typed RPC table; keys, devices, pairing links, relay control (milestone 3); validated names, profiles, creators, agent credentials, fleet manifests and MCP server definitions (milestone 4); protocol 6's envelope (`forward`: `Origin`, `fleet.forward`/`relay`/`deliver`), transfers (`transfer`, with the size limits) and secrets on the wire (`secret`: `SecretValue`, redacted). No I/O. | - |
| `vessel-rpc` | Moves `ClientFrame`/`ServerFrame` over any frame transport; handshake, correlation, concurrency limits, generic over `Handler`. From protocol 5 it holds both sides to the negotiated version: a call newer than the connection (`Call::since`) is refused by the client before sending and by the server before dispatch, and outgoing frames are adapted to an older client where they are encoded (`ServerFrame::for_protocol`). | proto |
| `vessel-proc` | Command specs, spawning in a process group (a kill-on-close job object on Windows), killing the group. | - |
| `vessel-harness` | Everything harness-specific, one module per harness: discovery, version probe, terminal and ACP launch commands; `HarnessEnv` (the account harnesses run as, given by the binaries, never read from the process by a library); since milestone 4 harness profiles and `profiles.toml`, each harness's home and login check, a session's files and linking them between homes, a profile's memory files and renaming a path for another working directory (transfers, protocol 6), MCP servers for terminal sessions, and what `vessel init` offers (install and login commands, other accounts' homes). | proto, proc |
| `vessel-pty` | Runs a command in a PTY; byte I/O and resize; on Windows, answers the pseudo console's startup cursor query. | proc |
| `vessel-agent` | ACP client: drives a harness or adapter over stdio and maps ACP updates to `AgentEvent`s; gives MCP servers in `session/new` and `session/load` (milestone 4). | proto, proc |
| `vessel-session` | Session actors: owns the process, assigns `EventSeq`, keeps a ring cache, broadcasts, hands every record to the node for persistence. | proto, harness, pty, agent |
| `vessel-store` | Data directory (`node.id`, `token`, `node.key`, `vessel.db`, `profiles.toml`, `mcp/`) and its locks, SQLite event log (schema 2: paired devices; schema 3: sessions' current profile and creator; schema 4: where a session came from, `origin` and `via`, protocol 6), retention, restart recovery. | proto |
| `vessel-node` | `vesseld`: config, auth, HTTP/websocket server, `Handler` implementation, persistence and subscription plumbing; devices, scopes and the secure gateway, and `vesseld relay` (milestone 3); harness profiles (`profiles`: `profiles.toml`, `profile.list`/`reload`, what `session.create` launches with, what `session.move_profile` moves to), agent principals (`agents`: a credential per session, grants, limits, ownership) and fleet sync (`fleet`: the home publishing `fleet.toml`'s sources and serving `fleet.*`, members joining, pulling, applying into every profile's home and reporting; this node's secrets) (milestone 4); transfers (`transfers`: `session.export`/`memory.export` snapshots held for whoever opened them, `transfer.read`/`close`, expiry, and `session.import`/`memory.import` pulling through a `Source`: this node, or another through the envelope as the caller) (protocol 6, F4); `envelope` and `fleet/links` (protocol 6, Track X): `fleet.forward`/`relay`/`deliver` with the delegated principal (the delivering device's scopes ∩ the origin's, ownership and agent limits applied to the origin), the mutual grant (`fleet.grant`, `fleet/granted.json` on a member, `fleet/grants/` and the binding record `fleet/bindings.json` on the home, `fleet.forget`, which withdraws the home's outstanding invitations, and `fleet.callback`, the home's relays and a relay ticket for a member with no route of its own, which `relays` links to: `fleet/links/callback.rs`), `fleet.peers`/`nodes`, and `session.read` waiting with its shares (`waits`). | proto, rpc, harness, session, store, secure, relay, fleet, client |
| `vessel-client` | Typed async client and gap-free session streams; the node book; pairing and secure connections over direct and relay routes (milestone 3); the local socket with the token or an agent's credential (milestone 4); fetching a file chunk by chunk into a caller's `ChunkSink` (`fetch_chunks`, protocol 6). | proto, rpc, secure, relay |
| `vessel-remote` | Remote nodes over SSH (milestone 2): the `ssh` runner, platform detection, installing `vesseld` by upload, `cleanup`; the SSH connector. | proto, rpc, client |
| `vessel-secure` | End-to-end secure channel (milestone 3, ADR 0009): Noise NK through `snow`, the node key pair, routing ids and fingerprints, the record layer, `SecureChannel` (a frame transport over any binary-message carrier), the relay's possession proof. | proto |
| `vessel-relay` | The relay (milestone 3, ADR 0009): `serve` (registration with token or ticket and possession proof, splicing, limits, close codes), registration tickets (`ticket`: HMAC-SHA256 under the token, minted by a fleet's home for a member's key, protocol 6), which addresses a link may dial (`reach`: `Reach::Public` resolves the relay's host at dial time and connects only to public addresses, for relays a member's home named), the node's `NodeLink` (with the token, or tickets it renews), the client's `dial`/`connect`, websocket carriers (`WsCarrier`, `AxumCarrier`) and the node's `respond` (closing with `4401`), TLS for `wss://` (rustls with `ring`). | proto, secure |
| `vessel-fleet` | Fleet bundles (milestone 4, ADR 0012): the home's sources into a manifest and a content-addressed blob store; plan and apply into harness homes with recorded hashes (`managed.json`), local edits kept with the incoming version beside them, symlinks never followed, targets linked to the sources left alone; skills named by `SKILL.md`; MCP secret references resolved at launch; transfers as file sets (`snapshot` into a staging store within protocol 6's limits, `place` into a home never overwriting: a session whole or not at all, memory beside what differs). | proto, harness |
| `vessel-mcp` | `vessel mcp` (milestone 4, ADR 0013): the MCP server (stdio, on `rmcp`) a node attaches to every session as `vessel`; each tool is one call through the session's agent principal. | proto, client |
| `vessel-cli` | `vessel` binary: commands, interactive attach, `vessel mcp`, `vessel init` (first-run setup, milestone 4). | proto, client, node, remote, mcp, harness |
| `vessel-testkit` | Fake harnesses (`fake-tui`, `fake-acp`) and `fake-ssh` for tests; with the `relay` feature, a relay that misbehaves on command. The fakes report the environment, arguments, MCP servers, loaded session id and working directory they got (milestone 4). | - |

The dependency direction is fixed by ADR 0001; adding an edge means updating
that ADR and this table. Test-only edges: `vessel-node`'s end-to-end tests
drive a node through `vessel-client` (a dev-dependency; the client does not
depend on the node, so there is no cycle), and `vessel-remote`'s and
`vessel-cli`'s SSH tests read `vessel-store`'s node lock to stop any node
they started.

## Data flow

1. A client opens a connection and presents who it is: the bearer token on
   `/rpc` or as the local socket's first frame (the owner, ADR 0005), a
   paired device's credential inside the Noise channel (ADR 0010), or a
   session's agent credential on the local socket (ADR 0013). It sends
   `Hello` with its protocol range and gets `Welcome` with the negotiated
   version and `NodeInfo`. Every call is then checked against its
   method's scope and, for an agent, against ownership.
2. `session.create` resolves the profile, asks `vessel-harness` for a
   launch command, and `vessel-session` starts an actor in terminal mode
   (`vessel-pty`) or structured mode (`vessel-agent`). The command carries
   the profile's environment and, for a terminal session, its MCP servers
   in the harness's launch-time form; a structured session gets them in
   `session/new` or `session/load` (ADRs 0011, 0012). The fleet's servers
   come with this node's secrets, and the session's own `vessel` server
   with its agent credential.
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
6. `session.move_profile` (ADR 0011) moves a structured session to another
   profile: the node resolves the target, finds both homes and runs the
   target's login check (`profiles::moving`); the session's actor holds
   input, refuses or cancels a running turn, hard-links the session's
   files into the target home, stops the agent and starts it again under
   the new profile with `session/load`, then publishes `ProfileChanged`.
   The session keeps its id, log and subscribers.
7. Agents (ADR 0013): `session.create` gives the new session's agent a
   place in `agents` (its creator's scopes, at most `read` and `operate`;
   its depth) and, when the node runs `vessel mcp`, a credential, which
   goes only into the session's `vessel` MCP server definition. That
   server connects to the node's local socket with the credential as its
   first frame; its calls run as `Principal::Agent`, and `operate` calls on
   a session need the session to descend from the agent's. The credential
   and the agent's connections end with the session.
8. Fleet sync (ADR 0012): the home bundles its sources (instructions,
   skills, MCP definitions) into a manifest and a content-addressed blob
   store whenever they change. A member, paired with the home as a `sync`
   device, long-polls `fleet.watch`, fetches the manifest and the blobs it
   lacks, applies them into every profile's home (keeping local edits as
   conflicts), and reports what it did.
9. Calls across nodes (protocol 6): a client or agent asks its own node
   with `fleet.forward { node, call }`. A member passes it to the home
   (`fleet.relay`, needs `relay`), and the home delivers it to the target
   (`fleet.deliver`) as the device the target granted it at join. The target
   runs it as `Principal::Delegated`: the scopes it granted the home,
   intersected with the origin's, with ownership and agent limits applied
   to the origin.
10. Transfers (protocol 6): `session.export` or `memory.export` snapshots
    files on the source for the caller that asked; `session.import` or
    `memory.import` on the target pulls them chunk by chunk (from this
    node, or from another through the envelope as the same caller), checks
    each file's hash, renames paths for the new working directory and
    places them without overwriting anything.
11. On shutdown the node kills each session's process group, waits for final
    statuses, and flushes the store. On the next start, sessions that were live
    are marked `Interrupted`. Their logs stay readable, and structured ones can
    be resumed through their `HarnessSessionId`.

## Design rules

- The daemon is the product; clients are thin.
- Harnesses are wrapped, never re-implemented (ADR 0003).
- Wire types exist once, in `vessel-proto`.
- No panics, no unbounded queues, no untyped JSON (see `AGENTS.md`).

## State (2026-11-06)

### Milestones

- **Milestone 1** (one node; `docs/plans/backend-m1.md`): merged on
  `backend/m1`, draft PR #1; CI first all green in run 42141788299.
  Checked by hand on Linux (2026-11-05) with real binaries: `vesseld`
  with the CLI ran `codex --version` in terminal mode; a structured Claude
  Code session (`claude-agent-acp` 0.100.0) answered; a `SIGTERM` stop
  left no adapter processes behind, and after a restart the session showed
  `interrupted` with its full log; `--resume` (ACP `session/load`)
  continued the conversation; `/rpc` without a token returned 401.
- **Milestone 2** (SSH; `docs/plans/backend-m2.md`, ADRs 0007 and 0008):
  merged on `backend/m2`. Checked by hand over real OpenSSH: install by
  upload, auto-start with `--bind none`, `run` over SSH, `update` refused
  while a session was live, a clean stop.
- **Milestone 3** (relay and pairing; `docs/plans/backend-m3.md`, ADRs 0009
  and 0010): built on `backend/m3`. Checked by hand on one machine
  through a local relay (pairing, every command through it, revocation).
  Still to do by hand: `wss://` behind an HTTPS proxy on a public host.
- **Milestone 4** (the fleet; `docs/plans/backend-m4.md`, ADRs 0011 to
  0013, protocols 5 and 6): Tracks H, F and X are merged on `backend/m4`,
  green on Linux, macOS and Windows. An independent security review of
  the fleet and the envelope (Track R) ran five rounds and is closed; its
  findings and fixes are recorded round by round in the plan.
  Still to do by hand: the plan's two smokes (four profiles on one node; a
  fresh machine joining), with the scripts in `docs/smoke/`.

### By crate

In the crate table's order.

- `vessel-proto`: the wire contract, versions 2 to 6 (`docs/PROTOCOL.md`).
  Protocol 2, milestone 1: typed RPC, the event log, the ACP-shaped agent
  timeline. 3: `node.stop` and `NodeInfo.state`. 4: devices, scopes on
  every method, the node's public key. 5: profiles, `session.move_profile`,
  creators, agent credentials, fleet manifests, `fleet.*` and MCP server
  definitions. 6: the `relay` scope, the envelope (`forward`: `Origin`,
  `fleet.forward`/`relay`/`deliver`), `fleet.grant`/`forget`/`peers`/`nodes`,
  `session.read` with `wait`, transfers (`transfer`, with the size
  limits) and secrets (`secret`: `SecretValue`, redacted in `Debug`).
  Every method has a first version (`since`), and a call that uses a
  later field needs that version (`Call::since`). No I/O.
- `vessel-rpc`: handshake, typed calls with cancellation and an in-flight
  bound (calls beyond it are refused `Unavailable`; the connection keeps
  being read so `Cancel` gets through), per-connection outbound queue
  with bounded flush batching, subscription ordering, release and lag on
  both ends (ADR 0006), in-memory transport for tests, and `Framed`
  (length-delimited frames over any byte stream: SSH stdio and the local
  socket). The `Welcome`'s `NodeInfo` is read from a watch, so a draining
  node says so. A connection the server closes still answers the calls
  already running (within the close timeout) and writes every frame it
  took off its queue, so `node.stop{now}` gets its `ack`. Both sides hold
  to the negotiated version: a call newer than the connection is refused
  by the client before sending and by the server before dispatch, and
  outgoing frames are adapted to an older client (`ServerFrame::for_protocol`).
- `vessel-proc`: `CommandSpec`, `spawn` as a process-group leader,
  `ProcessGroup` (Unix). Stopping signals the whole group, and the group
  is swept once its leader exits (`signal_with_leader`: an `EPERM` from a
  leader that has begun exiting counts as gone only once the group is).
  On Windows each child starts suspended in its own kill-on-close job
  object (`process-wrap`), so stopping ends the whole tree: an npm `.cmd`
  shim's `cmd.exe`, the adapter it started, and their children.
- `vessel-harness`: one descriptor module per harness, `Lookup` (explicit
  search path), concurrent `discover` with a version-probe timeout, and
  `launch` (the working directory absolute and existing, the home
  directory when none is given). `HarnessEnv` is the account harnesses
  run as (the user's home and the harness home variables), given by the
  binaries and never read from the process by library code: every
  in-process node a test starts gets a temporary one. Milestone 4:
  `profile` (`Profile`, the implicit default profile per harness,
  `parse_profiles`, typed errors that name the profile and key and never
  quote values); `home` and `login_check`; `layout` (where shared
  instructions and skills live in a home); `sessions` (`session_files`,
  `link_session`: hard links, never `memory/`, never through a symbolic
  link, conflicts found before anything is linked); `memory_files` and
  `rebase` (a path renamed for another working directory: Claude Code's
  project directory; Codex's dated rollouts stay); `mcp` (`McpFile`, the
  owner-only per-session file). No MCP value is ever an argument: Claude
  Code reads an owner-only `--mcp-config=` file, Codex gets `-c
  mcp_servers.<name>` overrides that name variables in its environment.
  `setup` holds what `vessel init` offers (install and login commands,
  other accounts' homes). The `fakes` feature installs testkit binaries
  under real harness names. Model selection is not here: it goes over ACP
  in `vessel-agent`.
- `vessel-pty`: `PtyProcess` over `portable-pty`, with dedicated reader,
  control (input and resize, in order) and waiter threads behind bounded
  channels. On Windows it answers the pseudo console's startup cursor
  query itself (`startup`), and a session terminates only its direct
  child; the rest end when the pseudo console closes.
- `vessel-agent`: ACP v1 client on `agent-client-protocol` 5.0.
  `AgentProcess` (own process group), `AgentInput` (prompt, cancel,
  permission answers with typed refusals, `turn_running`), `AgentOutputs`
  (bounded). A single driver loop owns the session; handlers forward into
  one ordered channel. The model is chosen through the `model` config
  option. The handshake has a deadline (60 s by default, `vesseld
  --agent-handshake-timeout`); a stuck adapter fails the session. MCP
  servers go in `session/new` and `session/load` (HTTP servers only when
  the agent advertises them). The `mapper` is pure and tested against
  fixtures in `crates/vessel-agent/tests/fixtures/acp/`.
- `vessel-session`: `SessionRuntime`, `SessionHandle`, `Follow`, records
  shared as `Arc<EventRecord>`; terminal and structured actors share the
  `lifecycle` module (stop-responsive publishing, two-step stop).
  Structured status is `Starting` until the agent's `Ready`, then
  `Running`; one that never became ready ends `Failed`, unless it was
  killed while starting. An ended session stays (ring included) until the
  node removes it once the store has the whole log. Milestone 4:
  `create_with(spec, LaunchOptions)` (profile, MCP servers and directory,
  creator, origin, the id chosen by the node so an agent credential is
  bound before the agent starts), and `move_profile` (`moving`): input is
  held for the move, a running turn is refused unless forced (then
  cancelled, waited for up to 10 s), the files are linked while the old
  agent runs, the agent is stopped and started again with
  `session/load`; if the new one does not start, the old command starts
  again on the same conversation and the move fails without
  `ProfileChanged`.
- `vessel-store`: an owner-only data directory (Unix: 0700, an existing
  one tightened or refused if another user's; secrets 0600), the SQLite
  event log with a batching writer thread, paged reads, restart recovery
  to `Interrupted`, retention, `user_version` migrations. `node.id`,
  `token`, `node.key` (32 bytes, created on first start and never
  replaced, wiped from memory when dropped); `NodeLock` (one node per data
  directory, an OS lock with the holder in `vesseld.owner`) and the
  `StartLock`; `profiles.toml` (`DataDir::write_profiles`, owner-only and
  atomic). Schemas: 2 adds `devices` (only the SHA-256 of each
  credential, compared in constant time; a revoked device stays as a
  tombstone), 3 each session's current profile and creator, 4 where a
  session came from (`origin`, `via`).
- `vessel-node` (`vesseld`):
  - Sessions: axum `/health` and token-checked `/rpc`, the `Handler`
    routing to `sessions` (live runtime sessions plus stored ones; a
    session whose log is complete is served from the store, and a live
    one serves from its ring the records retention already pruned),
    `persist` (one task per session drains its durable channel into the
    store), `subscribe` (ring, then a durable-gated store gap fill, then
    live; `Lagged` when a subscriber falls behind), and an ordered
    shutdown in which subscribers still receive the final `Interrupted`.
  - Transports: the local socket (Unix socket 0600, or a named pipe on
    Windows) whose first frame is the token or an agent's credential;
    `--bind none`; `vesseld attach`, the byte-pumping proxy SSH runs,
    which starts the node through a login shell when none answers;
    `vesseld version` and `cleanup`; `node.stop` (`now`, or drain).
  - Devices (milestone 3): every connection has a principal (`Owner`,
    `Device`, `Agent`, `Delegated`), and every call is checked against its
    method's scope by `guard::Guarded`, generated from one row per method;
    `NodeService`'s calls are inherent methods, so only `Guarded` can be
    served. `device.invite` (single-use, in memory by hash, at most a day,
    at most 64 waiting), `device.list`, `device.revoke` (closes the
    device's connections, each of which has its own RPC server).
    `SecureGateway::accept` answers the Noise handshake with the node's key
    pair through `vessel_relay::respond` (closing with `4401` when it
    fails), then the device preamble; at most 64 channels may be in their
    handshake or preamble at once, with frames limited to 64 KiB until
    admitted. Carriers come from `/secure` (pinged every 10 s, dropped
    after 30 s of silence) and from one `NodeLink` per relay (`relays`):
    each `--relay`, with the relays' token, and on a fleet member with no
    route of its own its current home's relays, added while the node runs
    with a ticket for its own key that it renews (`fleet.callback`), named
    only in its grant's link and unlinked when it follows another home or
    none; `--public` adds direct routes. Device labels lose
    control and invisible formatting characters.
  - Profiles (H1, H2): `profiles` reads `profiles.toml` at start (a broken
    file stops the node) and on `profile.reload` (a broken one leaves the
    set in effect); `NodeProfiles` owns the `HarnessEnv` and is the one
    place node code resolves a home; `profiles::launch` turns a spec and
    the caller's principal into `LaunchOptions` (missing secrets become
    `vessel:` lines in the session's log). `profiles::moving::plan`
    resolves the target, computes both homes, and runs the target's login
    check (30 s); terminal sessions are refused first.
  - Agents (H4, ADR 0013): the `agents` registry (credential hashes in
    memory, scopes, depth, the agent's connections; at most 8 live
    sessions an agent created and depth 3, checked under one lock), the
    agent preamble on the local socket, the ownership rule in the guard,
    and the `vessel` MCP server in every session from `Config::vessel_mcp`.
  - Fleet (F2, F3, ADR 0012): state under `<data_dir>/fleet/`, with
    `<data_dir>/fleet.toml` making a node the home. One loop looks every
    5 s: the home bundles when its sources changed (at least every 10
    minutes), publishes, and applies to its own profiles (its sources
    protected); a member pulls on (re)connecting, on a `fleet.watch`
    answer and every 10 minutes, fetches missing blobs in 1 MiB chunks,
    applies into every profile's home and reports. The home serves
    `fleet.manifest`, `fleet.blob`, `fleet.watch` (a 50 s long poll),
    `fleet.report` and `fleet.members`. `<data_dir>/secrets.toml`
    (owner-only, errors without values) and `FleetMcp`, which resolves
    the fleet's MCP servers with this node's secrets at each launch;
    `secret.set`/`list`/`delete`, and `secret.copy`, which asks the target
    `secret.list` first and sends the value as `secret.set` only if that
    passes.
  - Across nodes (Track X, protocol 6): `envelope` (`fleet.forward`,
    `relay`, `deliver`, with `Principal::Delegated`: the delivering
    device's scopes intersected with the origin's, ownership and the
    agent limits applied to the origin); `fleet/links` (the mutual grant:
    a member grants its home the scopes chosen at join, once per join,
    `fleet/granted.json`; the home keeps the credentials under
    `fleet/grants/`); `fleet.peers` and `fleet.nodes`; `session.read` with
    `wait` and its shares (`waits`: 8 per origin, 16 per other node, 16 per
    relaying member device, 32 per connection).
  - Fleet identity, after the security review: the home binds each
    member's node id, key and name for good (`fleet/bindings.json`); a
    name stays reserved while its node's device is revoked, and the home
    refuses a member named like itself or another member (the fix is the
    node's `--name`). Rebinding a reinstalled or renamed node takes the
    owner's `fleet.forget { node }` on the home (admin, not forwardable):
    it withdraws every outstanding invitation and records the devices that
    exist then, and the forgotten id or name goes only to a device paired
    afterwards, the first to claim it keeping it. Refusals about revoked
    or forgotten nodes carry no ids or times.
  - Transfers (F4, protocol 6): `transfers` keeps `session.export` and
    `memory.export` snapshots under `<data_dir>/transfers/<id>/` for the
    caller that opened them (another caller gets `not_found`), at most 16
    at once, for 10 idle minutes, and empties the directory at start; a
    live session mid-turn and terminal sessions are refused.
    `session.import` and `memory.import` pull through a `Source` (this
    node, or another through the envelope as the importing caller),
    fetch with hashes checked (`fleet::chunks`, shared with the member's
    pull), rename and place, and with `start` create the session with
    `origin: imported`.
- `vessel-client`: the websocket connection with the bearer token (`wss://`
  refused for token connections: ADR 0005's amendment), one typed method
  per call, the gap-free `SessionStream`. `Connector` (`WsConnector`; the
  SSH one is in `vessel-remote`), `follow_resuming`/`ResumingStream`
  (redials with 0.5-30 s jittered backoff and follows again from its
  cursor; never repeats a call), `Pinned` (refuses a node whose id is not
  the pinned one). The node `book` (`<config_dir>/vessel/nodes.toml`, a
  project's `.vessel/config.toml` that may only pick one of the user's
  nodes, `resolve` for `--node`; edited with `toml_edit`, keeping
  comments). Milestone 3: paired nodes in the book (`key`, `relay`,
  `direct`, `credential = { file }`), `DeviceCredential` files (0600, in a
  0700 directory, bound to their node's id and key, so a book entry naming
  another key cannot draw the secret out), `pairing::pair` and the device
  preamble, and `secure::SecureConnector`: direct routes first (3 s), then
  relays (10 s), each followed by the handshake to the pinned key, the
  preamble and `hello`; a refusal after the handshake ends the search.
  Milestone 4: `NodeClient::connect_local` (the local socket, with the
  token or an agent's credential), `call_on` (`fleet.forward`), the
  fleet, secret and transfer calls, and `fetch_chunks`, the one per-file
  loop that streams a file into a caller's `ChunkSink` (the fleet's blob
  writer: hashed as written, kept only if right).
- `vessel-remote`: `ssh` (the system client with fixed options,
  `ControlMaster` sharing on Unix, commands from fixed words and
  single-quoted values), `platform` (`uname -sm`, four release targets),
  `install` (upload through stdin, SHA-256 check, run its `version`, then
  rename onto `vesseld-<version>` or `vesseld-<version>-dev-<hash>`;
  then `cleanup`), `release` (the archive and `SHA256SUMS` fetched with
  `gh release download` and checked), `provision` (`local`, `release` or
  `auto`), and `SshConnector` (`ssh … <data_dir>/bin/<binary> attach`,
  `ProxyExit` codes as typed errors, exit 127 installs and retries once).
  Tested against a temporary home through `ssh` and `gh` stand-ins, and
  with the real `vesseld` through `fake-ssh` (Unix only).
- `vessel-secure`: Noise `NK_25519_ChaChaPoly_SHA256` (prologue
  `vessel/secure/1`, one 10 s deadline) over any `Carrier`.
  `SecureChannel` is a `FrameTransport`: frames are chunked canonically
  into records of at most 65,535 bytes, sealed only when the carrier can
  take them, and every size and chunk rule is checked before anything is
  buffered; any tampering ends the channel. `NodeKeypair`, `routing_id`,
  `fingerprint`, the relay's possession proof, `set_max_frame` (the node
  admits under 64 KiB and raises it after the preamble). Tested with the
  published NK vectors, tamper cases, a seeded fuzz of the parser, and 16
  MiB frames.
- `vessel-relay`: `serve` on plain HTTP behind a reverse proxy
  (`/v1/node`, `/v1/accept`, `/v1/connect`, `/health`), state in memory
  only. A node registers with the relay token and the possession proof. A
  client is not read until the node's data connection arrives (no
  buffering), then both directions are forwarded a message at a time; a
  sender's close frame goes through unchanged. Limits: one record per
  message, 64 waiting clients per node, 10 s to accept, a per-address
  token bucket, and resource caps (ADR 0009's amendment): registered nodes
  (refused `full`), nodes still registering and relayed clients per node
  and in all (closed `4503`), and a byte rate per direction of a relayed
  connection (`server::throttle`); refusals logged at info, throttled,
  never with a token. Its own accept loop (`server::accept`, hyper's
  HTTP/1 with a 10 s header deadline) counts every socket from `accept`,
  in all and per address (`503` past either), and drops what is left 5 s
  into a stop; registering places and clients are also counted per
  address, a registering node and a spliced client have 2 s for their
  first message, and some registering places are kept for nodes that
  registered before (ADR 0009's amendments). Registration takes the token or a ticket for the
  registering key (`ticket`, ADR 0009's amendment); a ticket's
  registration counts against the node cap and ends when the ticket
  expires. `NodeLink` keeps a node registered (jittered backoff,
  `LinkState` on a watch), with the token or tickets it is handed, again
  at once with each newer one. One `Keepalive` (a ping every 10 s, a drop after
  30 s of silence) covers relayed and direct connections. `respond` runs
  the node's handshake on a websocket and closes with `4401` when it
  fails. `vesseld relay` runs it (README, "Running a relay").
- `vessel-fleet`: `bundle` turns the home's sources into a `Manifest` and
  a content-addressed `BlobStore` (owner-only, hash-checked writers; a
  damaged blob is removed so it is fetched again). Skills are named by
  their `SKILL.md`'s `name`; dot entries are never bundled; names that
  collide on macOS or Windows are reported. `plan` decides per place in
  each harness home (write, adopt identical content, keep a local edit
  with the incoming version beside it, leave a deleted file deleted until
  restored, remove what left the bundle if unchanged), skipping places in
  or linked to the sources; `apply` reaches every place through
  directories held open from the root (no symlink followed), decides
  again right before acting, writes owner-only and atomically, and
  records what it wrote in `managed.json`. `resolve` fills secret
  references and names every missing secret. Transfers as file sets:
  `snapshot` into a staging store (256 MiB per file, 2 GiB in all, checked
  as read) and `place` (never overwriting: a session whole or not at all,
  memory beside what differs).
- `vessel-mcp`: `vessel mcp`, the MCP server on `rmcp` (stdio) a node
  attaches to every session as `vessel`. Tools: `list_nodes`,
  `list_sessions`, `create_session`, `send_input`, `answer_permission`,
  `read_events`, `wait_turn` (a long poll of `session.read` with `wait`),
  `kill_session`; each but `list_nodes` takes `node` and goes through
  `fleet.forward` for another node.
- `vessel-cli` (`vessel`): `node start`, `status`, `harnesses`,
  `sessions`, `run`, `attach`, `send`, `log`, `kill`; terminal attach in
  raw mode with `Ctrl-]` to detach (also under the kitty keyboard
  protocol and `modifyOtherKeys`; the terminal is reset on every way out),
  structured attach as a conversation with numbered permission answers.
  The local token file is sent only to a loopback `--node`, and a relative
  `--cwd` is resolved only for one. Milestone 2: `--node` through the book, `vessel nodes`, ids pinned on
  first contact, attach through a `ResumingStream` (`[vessel] connection
  lost; reconnecting…`), `vessel node install|status|update`. Milestone 3:
  `vessel node pair` (the link on stdout, a QR code on a terminal),
  `vessel node devices [revoke]`, `vessel nodes add NAME --pair` (the link
  from standard input only), every command to a paired node through
  `Pinned<SecureConnector<WsDialer>>`. Milestone 4: `profiles`, `run
  --profile`, `session move-profile|copy|move`, `fleet
  init|invite|join|leave|status|members|forget|restore` (`forget` against
  the home over `--node`), `fleet secret
  set|list|delete|copy` (values from a prompt without echo or stdin,
  never argv), `fleet memory copy`, `init` (idempotent steps: node
  identity, harnesses, install offers, profiles for other accounts' homes,
  login checks, `--service` as a systemd user unit or launchd agent,
  `--join`), and `mcp`. The CLI reads `VESSEL_NODE`, `VESSEL_TOKEN`,
  `VESSEL_DATA_DIR`, `VESSEL_CONFIG_DIR`, `VESSEL_SSH` and `VESSEL_GH`;
  tests set them so they never touch the user's book, `ssh` or GitHub.
  Tested through the binary (`tests/cli/`), including a test that plants
  secret values and searches every log, file and command line a fleet run
  produced (`canary.rs`).
- `vessel-testkit`: `fake-tui`, `fake-acp`, `fake-ssh` (the OpenSSH
  command line, running the "remote" command locally under a fake home,
  with an injectable connection drop), the `Fake::path` locator (a nested
  `cargo build` once per test process; the first one of a run recompiles
  about 30 crates), and `within`, the test safety net whose clock starts
  after that build. The fakes report the environment, arguments, MCP
  servers, loaded session id and working directory they got (`report
  <path>` writes them to a file). With the `relay` feature, a relay that
  drops, replays, reorders, corrupts or delays records on command.
- Release workflow (`.github/workflows/release.yml`, milestone 2): on a
  `v<version>` tag matching the workspace version, it builds `vesseld` for
  four targets (musl through `cargo-zigbuild`), packaged with
  `SHA256SUMS`, and attaches them to the tag's release. Not run yet: no
  tag has been pushed. Signing waits for the owner's decision on public
  releases.

## Later

Model routing (Phase 3), the LSP bridge (Phase 4) and the clients (Phase
5) each get a plan and ADRs when they start. See `ROADMAP.md`.
