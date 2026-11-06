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
| `vessel-node` | `vesseld`: config, auth, HTTP/websocket server, `Handler` implementation, persistence and subscription plumbing; devices, scopes and the secure gateway, and `vesseld relay` (milestone 3). | proto, rpc, harness, session, store, secure, relay |
| `vessel-client` | Typed async client and gap-free session streams; the node book; pairing and secure connections over direct and relay routes (milestone 3). | proto, rpc, secure, relay |
| `vessel-remote` | Remote nodes over SSH (milestone 2): the `ssh` runner, platform detection, installing `vesseld` by upload, `cleanup`; the SSH connector. | proto, rpc, client |
| `vessel-secure` | End-to-end secure channel (milestone 3, ADR 0009): Noise NK through `snow`, the node key pair, routing ids and fingerprints, the record layer, `SecureChannel` (a frame transport over any binary-message carrier), the relay's possession proof. | proto |
| `vessel-relay` | The relay (milestone 3, ADR 0009): `serve` (registration with token and possession proof, splicing, limits, close codes), the node's `NodeLink`, the client's `dial`/`connect`, websocket carriers (`WsCarrier`, `AxumCarrier`) and the node's `respond` (closing with `4401`), TLS for `wss://` (rustls with `ring`). | proto, secure |
| `vessel-cli` | `vessel` binary: commands, interactive attach. | proto, client, node, remote |
| `vessel-testkit` | Fake harnesses (`fake-tui`, `fake-acp`) and `fake-ssh` for tests; with the `relay` feature, a relay that misbehaves on command. | - |

The dependency direction is fixed by ADR 0001; adding an edge means updating
that ADR and this table. Test-only edges: `vessel-node`'s end-to-end tests
drive a node through `vessel-client` (a dev-dependency; the client does not
depend on the node, so there is no cycle), and `vessel-remote`'s and
`vessel-cli`'s SSH tests read `vessel-store`'s node lock to stop any node
they started.

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
    is swept once its leader exits. On Windows each child starts suspended
    in its own kill-on-close job object (`process-wrap`), so stopping ends
    the whole tree: an npm `.cmd` shim's `cmd.exe`, the adapter it started,
    and their children. PTY sessions (`vessel-pty`) still terminate only
    the direct child on Windows; the rest end when the pseudo console
    closes.
  - `vessel-harness`: one descriptor module per harness, `Lookup` (explicit
    search path), concurrent `discover` with a version-probe timeout,
    `launch` (working directory absolute and existing: a relative one is
    refused with `InvalidRequest`, since it would name a directory relative
    to `vesseld`'s own; the home directory when none is given). The `fakes`
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
    ordered channel (`driver/`: the loop, the agent's traffic, the
    client's commands, settling after a close). The model is chosen through the `model` config option.
    The whole handshake has a deadline (`AgentSetup::handshake_timeout`,
    60 s by default); a stuck adapter fails the session instead of leaving
    it `Starting`. A node sets it through
    `RuntimeConfig::agent_handshake_timeout` (`vesseld
    --agent-handshake-timeout SECONDS`). The `mapper` is pure and tested against fixtures in
    `crates/vessel-agent/tests/fixtures/acp/`.
  - `vessel-session`: the R6 API (`SessionRuntime`, `SessionHandle`,
    `Follow`/`FollowError`, `Created::durable`; records shared as
    `Arc<EventRecord>`). Terminal actor and structured actor share the
    `lifecycle` module (stop-responsive publishing, two-step stop).
    Structured status is `Starting` until the agent's `Ready`, then
    `Running`; a session that never became ready ends `Failed`, unless it
    was killed while starting (then `Exited`, as a killed terminal
    session). Ended
    sessions stay (ring included) until the node calls
    `SessionRuntime::remove`, which it may only do once the session ended
    (`SessionHandle::ended`); the node is to call it once the store has the
    whole log.
  - `vessel-testkit`: `fake-tui`, `fake-acp`, `fake-ssh` (the OpenSSH
    command line, running the "remote" command locally under a fake home,
    with an injectable connection drop; milestone 2), the `Fake::path` locator
    (a nested `cargo build` once per test process; the first one of a run
    recompiles about 30 crates under this package's own feature set), and
    `within`, the test safety net whose clock starts after that build.
- Spine track:
  - `vessel-rpc`: handshake, typed calls with cancellation and an in-flight
    bound (calls beyond it are refused `Unavailable`; the connection keeps
    being read so `Cancel` gets through), per-connection outbound queue
    with bounded flush batching, subscription ordering, release and lag on
    both ends (ADR 0006), in-memory transport for tests, and `Framed`
    (length-delimited frames over any byte stream: SSH stdio and the
    node's local socket, PROTOCOL.md version 3). The `Welcome`'s
    `NodeInfo` is read from a watch, so a draining node says so. A
    connection the server closes still answers the calls already running
    (within the close timeout) and writes every frame it took off its
    queue, so `node.stop{now}` gets its `ack`; a client whose `Hello`
    cannot be sent still reports a waiting `rejected`.
  - `vessel-store`: owner-only data directory (Unix: 0700, an existing
    one tightened or refused if another user's; `node.id`, `token` and
    the event log with its `-wal`/`-shm` files 0600), SQLite event
    log with a batching writer thread, paged reads, restart recovery to
    `Interrupted`, retention, `user_version` migrations. Milestone 2:
    `NodeLock` (one node per data directory, an OS lock that cannot go
    stale, with the holder's pid, binary and version in `vesseld.owner`),
    the `StartLock` racing proxies share, and the `bin/`, `logs/` and
    socket paths. Milestone 3 (Track P): `node.key` (the node's secret key,
    32 bytes, 0600, created on first start and never replaced; its bytes
    are wiped from memory when dropped), and the
    `devices` table (schema version 2) holding each paired device with only
    the SHA-256 of its credential, compared in constant time; a revoked
    device stays as a tombstone.
  - `vessel-client`: websocket connection with the bearer token, one typed
    method per call, gap-free `SessionStream` (`follow`). `wss://` is
    refused: no TLS stack in milestone 1 (ADR 0005 amendment).
    Milestone 2 (Track C): `Connector` (`WsConnector` here,
    the SSH one in `vessel-remote`), `follow_resuming`/`ResumingStream`
    (redials with 0.5–30 s jittered backoff and follows again from its
    cursor; never repeats a call), `Pinned` (refuses a node whose id is
    not the pinned one, so a reconnect cannot land on another machine),
    `NodeClient::stop_node` (protocol 3 only), the plain-data
    `NodeAddress`/`SshTarget`/`RemotePath`, and the node `book`
    (`<config_dir>/vessel/nodes.toml`, a project's `.vessel/config.toml`
    that may only pick one of the user's nodes, `resolve` for `--node`;
    edited with `toml_edit`, keeping comments).
    Milestone 3 (Track P, in progress): a third kind of book entry, a
    paired node (`key`, `relay`, `direct`, `credential = { file }`; exactly
    one of `ssh`, `url` and `key` per node, and a `key` node needs a
    credential and a route), as `NodeAddress::Secure(SecureTarget)`;
    `DeviceCredential` files (`<config_dir>/vessel/credentials/<node
    id>.secret`, directory 0700, file 0600, replaced atomically); and
    `pairing::pair`/`authenticate`, the device preamble over any frame
    transport, with typed refusals that say whether retrying can help;
    `secure::open`/`pair`, the handshake to the pinned key, the preamble
    and `hello` over any `vessel-secure` carrier, with errors that say
    whether retrying can help. The connector that dials routes comes with
    `vessel-relay`.
  - `vessel-remote` (milestone 2, Track C): `ssh` (the system
    client with fixed options, `ControlMaster` sharing on Unix, commands
    built from fixed words and single-quoted values, the last 64 stderr
    lines kept), `platform` (`uname -sm` through `sh -s`, four release
    targets; a local build matches by OS and architecture), `install`
    (one remote script: upload through stdin into a `.vesseld-*.tmp`,
    SHA-256 check, `tar -xzO` for release archives, run its `version`
    there, then `mv -f` onto `vesseld-<version>` or
    `vesseld-<version>-dev-<hash>`; then `cleanup`; what is installed is
    a `build::Build` with its `digest::Sha256Digest`), `release` (the
    archive and `SHA256SUMS` fetched locally with `gh release download`
    and checked), `provision` (which build a machine gets: `local`,
    `release`, or `auto`, shared by `node install` and the connector) and
    `connector` (`SshConnector`: `ssh … <data_dir>/bin/<binary> attach`,
    protocol frames over the child's stdio, `ProxyExit` codes as typed
    errors, exit 127 installs and retries once, 60 s handshake). Tested
    against a temporary home through `ssh` and `gh` stand-ins, and with
    the real `vesseld` through `fake-ssh` (Unix only).
  - Release workflow (`.github/workflows/release.yml`, milestone 2): on a
    `v<version>` tag matching the workspace version, it builds `vesseld`
    for the four targets (musl through `cargo-zigbuild`), packaged as
    `vesseld-<version>-<target>.tar.gz` with `SHA256SUMS`, and attaches
    them to the tag's release. Not run yet: no tag has been pushed.
    Signing waits for the owner's decision on public releases.
  - `vessel-node` (`vesseld`): axum `/health` and token-checked `/rpc`,
    the `Handler` routing to `sessions` (live runtime sessions plus stored
    ones; a session whose log is complete, final status included, is
    removed from the runtime with `SessionRuntime::remove` and served from
    the store; reading a live session serves records retention already
    pruned from the store but the ring still holds from the ring), `persist`
    (one task per session drains its durable channel into the store and
    publishes "durable through N"),
    `subscribe` (ring, then a durable-gated store gap fill, then live;
    `Lagged` when a subscriber falls behind), and an ordered shutdown in
    which subscribers still receive the final `Interrupted` status. Both
    modes run end to end (`tests/e2e`: `fake-tui` as `claude`, `fake-acp`
    as its ACP adapter), including restart, `Interrupted` and resume.
    Milestone 2, node side (ADR 0007, 0008): a local socket (Unix socket
    0600, or a named pipe on Windows) whose first frame must be the token;
    `--bind none` (no TCP listener); `vesseld attach`, the byte-pumping
    proxy SSH runs, which starts the node in the background through a login
    shell when none answers (start lock, readiness line, `ProxyExit` codes);
    `vesseld version` and `vesseld cleanup`; `node.stop` (`now`, or drain:
    refuse new sessions and stop once none is live); the node lock;
    `--agent-handshake-timeout` (seconds, default 60). Tested
    in process (`tests/e2e/local.rs`) and with real processes through
    `fake-ssh` (`tests/e2e/remote.rs`, Unix).
    Milestone 3 (Track P, in progress; ADR 0010): every connection has a
    principal (`Owner` for the token and the local socket, `Device` for a
    paired device), and every call is checked against its method's scope
    by `guard::Guarded`, generated from one row per method with the method
    taken from the request type. `device.invite` (single-use invitations
    in memory, by hash, at most a day, at most 64 waiting), `device.list`,
    `device.revoke` (closes the device's open connections: each device
    connection has its own RPC server whose shutdown token is registered
    under the device). `SecureGateway::accept` answers the Noise handshake
    with the node's key pair (`vessel-secure`, from `node.key`, which is
    created on start), then runs the device preamble
    (`devices::preamble`) and the protocol; a failed handshake goes back
    to the caller, which closes with 4401. `NodeInfo.public_key` carries
    the public key. `/secure` and relay data connections reach the gateway
    once `vessel-relay` brings the websocket carrier. `--relay`,
    `--relay-token-file` (checked at start) and `--public` feed pairing
    links.
  - `vessel-cli` (`vessel`): `node start`, `status`, `harnesses`,
    `sessions`, `run`, `attach`, `send`, `log`, `kill`; sessions named by
    id prefix; terminal attach in raw mode with `Ctrl-]` to detach, also
    under the kitty keyboard protocol and `modifyOtherKeys`, and on
    `SIGTERM`/`SIGHUP`/`SIGINT`; the terminal is reset on every way out;
    keystrokes go through one ordered sender task so output never waits on
    input (live resize on Unix, initial size only on Windows); structured
    attach as a conversation with numbered permission answers; `log` in
    record order across stdout and stderr. The local token file is sent
    only to a loopback `--node`, and a relative `--cwd` is resolved only
    for one. Tested through the binary (`tests/cli/`).
    Milestone 2 (Track C): `--node` is resolved through the
    node book (a name, then `ssh://`/`user@host`, then a websocket
    address; else the project's or the book's `default_node`, else this
    machine's node), and `vessel nodes add|list|show|remove|default` edits
    the book. A book node's id is pinned on first contact and a different
    node is refused. Attach (both modes) follows through a
    `ResumingStream`: a lost connection shows `[vessel] connection lost;
    reconnecting…`, then `[vessel] reconnected`, and the attach goes on
    without repeats or gaps; input lost with the connection is reported,
    not resent (tested through a TCP proxy the test cuts). `vessel node
    install` uploads a local build or this version's release to an SSH
    node; any command reaches an SSH node through `SshConnector`, which
    installs on first contact. `vessel node status` shows the version,
    serving or draining, and live sessions. `vessel node update` installs,
    then stops the node at once with no live session, lists them and
    changes nothing by default, drains with `--when-idle`, or stops with
    `--now` after confirming. End to end through `fake-ssh` and the real
    `vesseld` (`tests/remote/`, Unix): fresh install and start, run and
    attach, a changed pinned id refused, update side by side with
    refuse-then-drain, and an SSH drop during attach resumed. Besides
    `VESSEL_NODE`, `VESSEL_TOKEN` and `VESSEL_DATA_DIR` (milestone 1), the
    CLI reads `VESSEL_CONFIG_DIR` (where the node book lives),
    `VESSEL_SSH` (the `ssh` program) and `VESSEL_GH` (the GitHub CLI);
    tests set all three so they never touch the user's book, the real
    `ssh`, or GitHub (the README lists them).
    Milestone 3 (Track P, in progress): `vessel node pair [--label]
    [--ttl 10m]` (the link alone on stdout, a QR code from `qrcode` 0.14
    when stdout is a terminal, instructions on stderr), `vessel node
    devices` and `vessel node devices revoke ID-PREFIX`, over any
    connection with the `admin` scope; `nodes show` for a paired node, and
    `node status`, show the key's fingerprint. `nodes add --pair` and
    connecting to paired nodes wait for `SecureConnector`, which dials the
    routes with `vessel-relay`.
- Milestone 3, Track S (in progress, `backend/m3-secure`):
  - `vessel-secure` (2026-11-06): `initiate`/`respond` run Noise
    `NK_25519_ChaChaPoly_SHA256` (prologue `vessel/secure/1`, 48-byte
    messages with empty payloads, one 10 s deadline) over any `Carrier`
    (in memory, `u16` length-prefixed bytes; websockets come with
    `vessel-relay`). `SecureChannel` is a `FrameTransport`: frames are
    chunked canonically into records of at most 65,535 bytes, sealed only
    when the carrier can take them, and every size and chunk rule is
    checked before anything is buffered. Any tampering ends the channel.
    `NodeKeypair`, `routing_id`, `fingerprint`, and the relay's possession
    proof (prologue `vessel/relay-proof/1`). Tested with the two published
    NK vectors, tamper cases (corrupt, replay, reorder, drop, truncate,
    each chunk rule), a seeded fuzz of the parser, 16 MiB frames, and a
    `vessel-rpc` connection over the channel. The cipher crates are
    optimised in dev builds (`Cargo.toml` explains why). `try_respond`
    hands the carrier back after a failed handshake. `set_max_frame`
    changes the frame limit from the next announcement on, so the node can
    admit under 64 KiB and raise it after the preamble.
  - `vessel-relay` (2026-11-06): `serve` on plain HTTP behind a reverse
    proxy (`/v1/node`, `/v1/accept`, `/v1/connect`, `/health`), state in
    memory only. A node registers with the relay token (constant-time) and
    the possession proof, and replaces an earlier registration only after
    proving the key. A client is not read until the node's data connection
    arrives (no buffering), then both directions are forwarded a message at
    a time; a sender's close frame (a node's `4401`) goes through
    unchanged. Limits: one record per message (from the frame header), 64
    waiting clients per node, 10 s to accept, pings every 10 s and a drop
    after 30 s of silence, a per-address token bucket (IPv6 by `/64`; the
    address may come from a proxy header). `NodeLink` keeps a node
    registered (0.5-30 s jittered backoff, `LinkState` on a watch) and hands
    each client's data connection to the node as a `WsCarrier`; `dial` and
    `connect` are the client's. `respond` runs the node's handshake on a
    websocket and closes with `4401` when it fails; `relay_close` and
    `node_close` read the codes back. `vesseld relay --bind --token-file
    [--client-address-header]` runs it (README, "Running a relay"). Tested
    on loopback with real relays, raw protocol peers, a byte tap (no
    plaintext), a relay restart behind a forwarder, and
    `vessel-testkit`'s misbehaving relay (drop, replay, reorder, corrupt
    both ways end the channel; delay does not).
- Milestone 1 is merged on `backend/m1` (draft PR #1).
- Manual smoke on Linux (2026-11-05) with real binaries:
  - `vesseld` with the `vessel` CLI ran `codex --version` in terminal mode.
  - A structured Claude Code session (Haiku, `claude-agent-acp` 0.100.0)
    answered.
  - A `SIGTERM` stop left no adapter processes behind. After a restart the
    session showed `interrupted` with its full log.
  - `--resume` (ACP `session/load`) continued the conversation.
  - `/rpc` without a token returned 401.
- Harness environment: `vesseld` passes its own environment to harnesses.
  For Claude Code that means the `CLAUDE_CONFIG_DIR` of whoever started the
  node decides which account a session uses. Choosing this per session is
  the "harness profiles" item of a later milestone.

## Later

Peers, relay, SSH transport and self-update, sync, harness profiles, the
model router and the LSP bridge each get a plan and ADRs when they start. See
`ROADMAP.md`.
