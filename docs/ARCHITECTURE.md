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
| `vessel-proto` | Wire contract: ids, sessions, events, agent timeline, errors, typed RPC table; keys, devices, pairing links, relay control (milestone 3); validated names, profiles, creators, agent credentials, fleet manifests and MCP server definitions (milestone 4). No I/O. | - |
| `vessel-rpc` | Moves `ClientFrame`/`ServerFrame` over any frame transport; handshake, correlation, concurrency limits, generic over `Handler`. From protocol 5 it holds both sides to the negotiated version: a call newer than the connection (`Call::since`) is refused by the client before sending and by the server before dispatch, and outgoing frames are adapted to an older client where they are encoded (`ServerFrame::for_protocol`). | proto |
| `vessel-proc` | Command specs, spawning in a process group, killing the group. | - |
| `vessel-harness` | Everything harness-specific, one module per harness: discovery, version probe, terminal and ACP launch commands; since milestone 4 harness profiles and `profiles.toml`, each harness's home and login check, a session's files and linking them between homes, and MCP servers for terminal sessions. | proto, proc |
| `vessel-pty` | Runs a command in a PTY; byte I/O and resize. | proc |
| `vessel-agent` | ACP client: drives a harness or adapter over stdio and maps ACP updates to `AgentEvent`s; gives MCP servers in `session/new` and `session/load` (milestone 4). | proto, proc |
| `vessel-session` | Session actors: owns the process, assigns `EventSeq`, keeps a ring cache, broadcasts, hands every record to the node for persistence. | proto, harness, pty, agent |
| `vessel-store` | Data directory (`node.id`, `token`, `vessel.db`, `profiles.toml`, `mcp/`), SQLite event log (schema 3: sessions' current profile and creator), retention, restart recovery. | proto |
| `vessel-node` | `vesseld`: config, auth, HTTP/websocket server, `Handler` implementation, persistence and subscription plumbing; devices, scopes and the secure gateway, and `vesseld relay` (milestone 3); harness profiles (`profiles`: `profiles.toml`, `profile.list`/`reload`, what `session.create` launches with, what `session.move_profile` moves to) (milestone 4). | proto, rpc, harness, session, store, secure, relay |
| `vessel-client` | Typed async client and gap-free session streams; the node book; pairing and secure connections over direct and relay routes (milestone 3). | proto, rpc, secure, relay |
| `vessel-remote` | Remote nodes over SSH (milestone 2): the `ssh` runner, platform detection, installing `vesseld` by upload, `cleanup`; the SSH connector. | proto, rpc, client |
| `vessel-secure` | End-to-end secure channel (milestone 3, ADR 0009): Noise NK through `snow`, the node key pair, routing ids and fingerprints, the record layer, `SecureChannel` (a frame transport over any binary-message carrier), the relay's possession proof. | proto |
| `vessel-relay` | The relay (milestone 3, ADR 0009): `serve` (registration with token and possession proof, splicing, limits, close codes), the node's `NodeLink`, the client's `dial`/`connect`, websocket carriers (`WsCarrier`, `AxumCarrier`) and the node's `respond` (closing with `4401`), TLS for `wss://` (rustls with `ring`). | proto, secure |
| `vessel-fleet` | Fleet bundles (milestone 4, ADR 0012): the home's sources into a manifest and a content-addressed blob store; plan and apply into harness homes with recorded hashes (`managed.json`), local edits kept with the incoming version beside them, symlinks never followed, targets linked to the sources left alone; skills named by `SKILL.md`; MCP secret references resolved at launch. | proto, harness |
| `vessel-cli` | `vessel` binary: commands, interactive attach. | proto, client, node, remote |
| `vessel-testkit` | Fake harnesses (`fake-tui`, `fake-acp`) and `fake-ssh` for tests; with the `relay` feature, a relay that misbehaves on command. The fakes report the environment, arguments, MCP servers and loaded session id they got (milestone 4). | - |

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
   structured mode (`vessel-agent`). The command carries the session's
   profile (variables set and removed; terminal arguments) and, for a
   terminal session, its MCP servers in the harness's launch-time form; a
   structured session gets its MCP servers in `session/new` or
   `session/load` instead (ADRs 0011, 0012).
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
    Milestone 3 (Track P): a third kind of book entry, a paired node
    (`key`, `relay`, `direct`, `credential = { file }`; exactly one of
    `ssh`, `url` and `key` per node, and a `key` node needs a credential
    and a route), as `NodeAddress::Secure(SecureTarget)`;
    `DeviceCredential` files (`<config_dir>/vessel/credentials/<node
    id>.secret`: the node id and key from the pairing link, the device
    id and secret; directory 0700, file 0600, replaced atomically through
    a randomly named temporary file), bound to their node: `check` refuses
    another key or id before anything is dialled, so a book entry naming
    someone else's key cannot draw the secret out; `pairing::pair`
    (checks that the node pinned is the link's) and `authenticate`, the
    device preamble over any frame transport. `secure::SecureConnector`
    is the paired node's `Connector`: direct routes first (3 s dial
    timeout), then relays (10 s), each followed by the handshake to the
    pinned key, the preamble and `hello`. Before the handshake completes
    a failure is the route's, and the next route is tried; after it, the
    node's answer holds for every route, so a refusal ends the search.
    `4401` (a wrong key), a refused `Revoked` or `UnknownDevice`, and
    tampering are final; a network failure, `4404`, `4408` and `4429`
    are transient; all routes failed is final if any failure was. Carriers
    come from a `Dialer`: `WsDialer` (`vessel_relay::connect` to a direct
    route's `/secure`, `vessel_relay::dial` through a relay, close codes
    through `node_close`/`relay_close`, TLS from the system roots), or
    in-memory ones in tests. `pair_over` pairs over the link's routes and
    stops once a channel is up.
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
    under the device; pairing registers the connection before it stores
    the device). `NodeService`'s calls are inherent methods, so only
    `Guarded` can be served. `SecureGateway::accept` answers the Noise
    handshake with the node's key pair (`vessel-secure`, from `node.key`,
    which is created on start) through `vessel_relay::respond`, which
    closes with 4401 when it fails, then runs the device preamble
    (`devices::preamble`) and the protocol; at most 64 channels may be in
    their handshake or preamble at once, and their frames are limited to
    64 KiB until the preamble is answered. `NodeInfo.public_key` carries the
    public key. Carriers arrive from `/secure` on the node's listener (an
    `AxumCarrier`, messages capped at one record, pinged every 10 s and
    dropped after 30 s of silence, `Config::keepalive`) and from one `NodeLink`
    per `--relay` (registering with `--relay-token-file`'s token, TLS for
    `wss://` from the system roots; `Node::relay_links` reports their
    state). `--public` adds direct routes to pairing links. Device labels
    lose control and invisible formatting characters.
  - `vessel-cli` (`vessel`): `node start`, `status`, `harnesses`,
    `sessions`, `run`, `attach`, `send`, `log`, `kill` (and, since
    milestone 4, `profiles` and `session move-profile`); sessions named by
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
    Milestone 3 (Track P): `vessel node pair [--label] [--ttl 10m]` (the
    link alone on stdout, a QR code from `qrcode` 0.14 when stdout is a
    terminal, instructions on stderr), `vessel node devices` and `vessel
    node devices revoke ID-PREFIX`, over any connection with the `admin`
    scope; `nodes show` for a paired node, and `node status`, show the
    key's fingerprint. `vessel nodes add NAME --pair` reads the link from
    standard input only (argv is visible to other users and kept in shell
    history, so `--pair LINK` is refused as exposed), shows the node id,
    key fingerprint, routes and expiry, confirms at a terminal (`--yes`
    otherwise), pairs, and writes the credential and then the book entry.
    Every command reaches a paired node by name through
    `Pinned<SecureConnector<WsDialer>>`, and an attach resumes over the
    routes as it does over SSH. Tested end to end through an in-process
    relay (`tests/cli/devices.rs`).
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
    `connect` are the client's. One `Keepalive` (a ping every 10 s, a drop
    after 30 s of silence) covers the relay's connections and direct ones:
    a node's `AxumCarrier` on `/secure` pings its client from inside the
    carrier (from the reading side when nothing is being written, never
    taking the writing task's waker), and every client carrier, relayed or
    direct, fails after the same silence. `respond` runs the node's handshake on a
    websocket and closes with `4401` when it fails; `relay_close` and
    `node_close` read the codes back. `vesseld relay --bind --token-file
    [--client-address-header]` runs it (README, "Running a relay"). Tested
    on loopback with real relays, raw protocol peers, a byte tap (no
    plaintext), a relay restart behind a forwarder, and
    `vessel-testkit`'s misbehaving relay (drop, replay, reorder, corrupt
    both ways end the channel; delay does not).
- Milestone 4, Track F (in progress, `backend/m4-fleet`):
  - `vessel-fleet` (F1, 2026-11-06): `bundle` turns the home's sources
    (an instructions file, a skills directory, `skip` globs, MCP
    definitions) into a `Manifest` and a content-addressed `BlobStore`
    (`<root>/<2 hex>/<sha256>`, owner-only, written under temporary names,
    chunked reads for `fleet.blob`, hash-checked writers for members).
    Bundle paths are `instructions.md` and `skills/<name>/...`, `<name>`
    being the `name` in the skill's `SKILL.md` (that one field, read
    strictly). The generation rises only when files or MCP servers change.
    Symlinks in the sources are followed only inside the skills directory;
    dot directories there (Codex's `.system`) are never bundled, and no
    skill name starts with `.`.
    `plan` decides per place in each `Target` (a harness home with its
    instruction file and skills directory names, from
    `vessel_harness::layout`; Gemini CLI takes no skills): write, adopt identical
    content, keep a local edit (or a file Vessel never wrote) with the
    incoming version beside it (`.vessel-incoming`) as a conflict, remove
    what left the bundle if unchanged, skip places in or linked to the
    sources and symlinks below the root (never followed). `apply` checks
    each place again, creates directories without crossing symlinks,
    writes atomically and records hashes in `Managed` (`managed.json`).
    `resolve` fills secret references and names every missing secret.
    Tested over temporary trees, including the operator's linked layout
    (Unix).
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
  node decides which account a session uses. Milestone 4 Track H (below)
  makes this per session.
- Milestone 4 Track H, the harness side (2026-11-06, branch
  `backend/m4-profiles`; the node wiring follows the protocol 5 contract):
  - `vessel-harness`: `profile` (`Profile`, the implicit default profile per
    harness, `parse_profiles` for `profiles.toml` text, typed errors that
    name the profile and key and never quote values); `home` (`home`,
    `login_check`); `sessions` (`session_files` per the descriptor's
    layout, `link_session`: hard links, never `memory/`, never through a
    symbolic link, conflicts found before anything is linked); `mcp`
    (`McpFile`, the owner-only per-session file, `WrittenMcpFile` removes
    it on drop). `launch(lookup, spec, profile, mcp)` returns `Launch`
    (command, MCP file, servers the harness cannot take); it takes the
    directory for the MCP file (the node's own, ADR 0012; no shared
    fallback) and checks the profile against `spec.profile`. `layout(harness)` says where
    shared instructions and skills live in a home (for fleet sync). Descriptors now
    carry the default profile name, home, login check, session finder and
    terminal MCP form. No MCP value is ever an argument: Claude Code reads
    an owner-only `--mcp-config=` file, Codex gets `-c mcp_servers.<name>`
    overrides that name variables set in its environment.
  - `vessel-agent`: `AgentSetup::mcp_servers`, converted to ACP in `mcp`;
    HTTP servers only when the agent advertises them.
  - `vessel-session`: `create_with(spec, LaunchOptions { profile,
    mcp_servers, mcp_dir, created_by })`; `create` is the defaults.
    `move_profile(id, ProfileMove)` (H2, `moving`): a structured actor
    takes a `Control::Move` while it runs. Input is held (`Moving`,
    `conflict`) for the move's length; the driver's `turn_running` answers
    in order with the commands before it, so a prompt already sent
    counts. Mid-turn is `MoveError::TurnInProgress` unless forced (then
    the turn is cancelled, waited for up to 10 s); the files are linked
    while the old agent still runs; the agent is stopped (`SIGTERM`, then
    `SIGKILL` after 5 s) and the new one started with `session/load`, its
    replay dropped until it is ready. If it does not start, the previous
    command starts again on the same conversation and the move fails
    (`spawn_failed`) without `ProfileChanged`.
  - `vessel-agent`: `AgentInput::turn_running`, answered by the driver.
  - `vessel-node` (H1): the `profiles` module reads `profiles.toml` at
    start (a broken file stops the node) and on `profile.reload` (a broken
    one leaves the set in effect); `profiles::launch` turns a spec and the
    caller's principal into `LaunchOptions` (the profile resolved, an
    owner-only `<data_dir>/mcp`, `created_by`), and is where fleet sync's
    MCP servers will come in. The store's schema 3 keeps each session's
    current profile (following `ProfileChanged`) and its creator. The terminal actor holds the
    MCP file until the session ends.
  - `vessel-node` (H2): `profiles::moving::plan` resolves the target
    (none: the harness's default; unknown or another harness's:
    `invalid_request`) and the session's current profile (gone from
    `profiles.toml`: `conflict`), computes both homes with the node's
    environment, and runs the target's login check with its environment,
    bounded by 30 s (not logged in, or no answer: `conflict`). Terminal
    sessions are refused first (`unsupported`), before any harness runs.
    `Sessions::move_profile` moves live sessions; an ended one is
    `conflict`. MCP servers for the new agent come from the same seam as
    `session.create`'s.
  - `vessel-client`: `move_profile`; `vessel-cli`: `vessel session
    move-profile <session> <profile> [--force]`.

## Later

Peers, relay, SSH transport and self-update, sync, the
model router and the LSP bridge each get a plan and ADRs when they start. See
`ROADMAP.md`.
