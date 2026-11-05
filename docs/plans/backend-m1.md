# Backend milestone 1: one node, done properly

Status: implemented on `backend/m1` (2026-11-05), draft PR #1; review fixes in progress. Owner: coordinator (the agent that
wrote this plan). Two implementation tracks run in parallel in separate git
worktrees.

Read first: `AGENTS.md` (rules), `docs/ORIGIN.ko.md` (intent),
`docs/LESSONS.md` (what we take from Zed and Paseo), ADRs 0001-0005.

## Goal

Finish Phase 0 of the roadmap on a structure that Phases 1-4 can build on
without rewrites:

- a single `vesseld` runs Claude Code, Codex, Gemini and OpenCode in
  **terminal** mode (real TUI in a PTY) and **structured** mode (ACP);
- any number of clients attach, detach and resume by sequence number,
  including after a node restart;
- one typed RPC connection (protocol 2) with token auth;
- a typed client library and a CLI that can interactively attach to a
  terminal session and chat with a structured one.

Out of scope for M1 (each is a later plan): peers, relay, SSH transport and
binary self-update, fleet sync, harness profiles (`CLAUDE_CONFIG_DIR`
variants), model routing, LSP bridge, a session host that survives node
upgrades, web UI.

## The contract (already on the integration branch)

`vessel-proto` is written and frozen for M1. Tracks must not edit it. If a
track needs a change, it stops and asks the coordinator, who changes it on
`backend/m1` and tells both tracks to rebase.

Key types: `SessionSpec`/`SessionMode`, `SessionStatus`, `SessionInfo`,
`SessionInput`, `SessionEvent`/`EventRecord`/`EventSeq`, `AgentEvent` and its
parts, `TerminalBytes`/`TerminalInput`/`TerminalSize`, `Harness`/
`StructuredSupport`, `ErrorCode`/`WireError`, and in `rpc`: the
`Request` trait, `Call`/`Reply`/`Method`, `Handler` + `dispatch`,
`ClientFrame`/`ServerFrame`, `Hello`/`Welcome`.

## Branches and worktrees

- Integration branch: `backend/m1` (coordinator merges here).
- Track R (runtime): branch `backend/m1-runtime`.
- Track S (spine): branch `backend/m1-spine`.
- Each track rebases onto `backend/m1` when the coordinator says so. Never
  push; never force-update `backend/m1`.

## Track R: runtime (agent "오오푸스2", provider `claude-alt`)

Owns: `vessel-proc`, `vessel-harness`, `vessel-pty`, `vessel-agent`,
`vessel-session`, `vessel-testkit`.

### R1. `vessel-proc`: process primitives
- `CommandSpec { program: PathBuf, args: Vec<OsString>, env: Vec<(OsString,
  EnvValue)>, cwd: Option<PathBuf> }` where `EnvValue` is `Set(OsString)` or
  `Remove`. Pure data, built by `vessel-harness`.
- Spawn with piped stdio in a new process group (`process_group(0)` on Unix);
  `ProcessGroup::kill(signal)` kills the whole group (Unix via `nix`; on
  Windows kill the child and document the gap). Map `ExitStatus` to
  `Option<i32>`.
- Tests: spawn the testkit binary, kill the group, check its child is gone.

### R2. `vessel-testkit`: fakes
- `fake-tui`: echoes stdin to stdout with a prompt, prints its window size on
  `SIGWINCH` (Unix), exits on `exit`. Spawns a grandchild that sleeps, to
  prove group kill.
- `fake-acp`: a tiny ACP agent over stdio (use the ACP crate's agent side):
  `initialize`, `session/new`, `session/load`, `session/prompt` that streams
  a message chunk, a thought, a tool call with an update, a permission
  request, and ends the turn; honours `session/cancel`.
- Dev-dependency only; never shipped in `vesseld`.

### R3. `vessel-harness`: knowledge of each harness
- One module per `HarnessKind` (`claude.rs`, `codex.rs`, `gemini.rs`,
  `opencode.rs`) implementing the same private shape:
  binary name; `terminal_command(args, cwd) -> CommandSpec`;
  `structured_command(cwd) -> Result<CommandSpec, StructuredUnavailable>`
  (Claude: `claude-agent-acp` on `PATH`; Codex: `codex-acp` on `PATH`;
  Gemini: `gemini --experimental-acp`; OpenCode: `opencode acp`); and
  structured-mode environment requirements.
- Public API: `discover() -> Vec<Harness>` (PATH lookup plus a version probe
  with a timeout, probes run concurrently); `launch(kind, &SessionSpec) ->
  Result<CommandSpec, HarnessError>`.
- Nothing outside this crate matches on `HarnessKind` to decide behaviour.
- Overridable lookup (an explicit `PATH`-like search list) so tests can point
  at testkit binaries.

### R4. `vessel-pty`: terminal sessions
- Use `portable-pty` (cross-platform, includes Windows ConPTY). Its blocking
  reader and writer run on dedicated threads that talk to async code over
  bounded channels.
- API: `PtyProcess::spawn(CommandSpec, TerminalSize) -> (PtyProcess,
  PtyOutput)`, `write(Bytes)`, `resize(TerminalSize)`, `kill()`, `wait()`.
  Output is `Bytes` chunks.
- Tests with `fake-tui`: echo, resize, group kill, exit code.

### R5. `vessel-agent`: ACP client
- Pin `agent-client-protocol` (5.x) in the workspace. ACP types never leave
  this crate.
- Modules: `transport` (spawn via `vessel-proc`, stdio JSON-RPC
  connection), `mapper` (pure functions ACP -> `vessel_proto::AgentEvent`,
  fixture tests), `session` (actor: initialize, `session/new` or
  `session/load`, prompt, cancel, permission round-trip).
- Client capabilities: declare no `fs` and no `terminal` capability, so
  agents use their own tools (Vessel does not re-implement them).
- Output: `AgentEvent`s on a bounded channel; harness stderr lines as
  `AgentEvent::HarnessLog`.
- Permission requests: the actor holds a pending map `PermissionId ->
  responder`, emits `PermissionRequested`, and resolves on the first answer;
  on cancel or exit it resolves all as `Cancelled` and emits
  `PermissionResolved` for each.

### R6. `vessel-session`: the runtime the node uses

This API is the seam with Track S. Implement it exactly. If a signature has
to change, tell the coordinator first.

```rust
pub struct RuntimeConfig {
    pub harness_lookup: vessel_harness::Lookup, // where binaries are found
    pub ring_capacity: NonZeroUsize,     // in-memory records per session (cache)
    pub durable_capacity: NonZeroUsize,  // bound of the durable channel
}

pub struct SessionRuntime { /* private */ }
impl SessionRuntime {
    pub fn new(config: RuntimeConfig, shutdown: CancellationToken) -> Self;
    /// Start a session. Status events are part of the stream: the first
    /// record is `Status{Starting}`.
    pub async fn create(&self, spec: SessionSpec) -> Result<Created, SessionError>;
    pub fn get(&self, id: SessionId) -> Option<SessionHandle>;
    pub fn list(&self) -> Vec<SessionInfo>;            // live and ended, this run
    /// Kill everything and wait until every session published its final status.
    pub async fn shutdown(&self);
}

pub struct Created {
    pub handle: SessionHandle,
    /// Every record, in order, exactly once. The node must drain it into the
    /// store. Bounded: if the node stops draining, the session applies
    /// backpressure (pumps pause) rather than dropping records.
    pub durable: tokio::sync::mpsc::Receiver<Arc<EventRecord>>,
}

#[derive(Clone)]
pub struct SessionHandle { /* private */ }
impl SessionHandle {
    pub fn id(&self) -> SessionId;
    pub fn info(&self) -> SessionInfo;                       // cheap snapshot
    pub fn watch_info(&self) -> tokio::sync::watch::Receiver<SessionInfo>;
    pub async fn send(&self, input: SessionInput) -> Result<(), SessionError>;
    pub fn kill(&self);
    /// Live records from `from`. Fails with `Gap { oldest_cached }` when
    /// `from` is older than the in-memory ring (the node then reads
    /// `from..oldest_cached` from the store and calls `follow` again), and
    /// with `Ahead { next }` when `from` is past the end of the log.
    pub fn follow(&self, from: EventSeq) -> Result<Follow, FollowError>;
}

pub struct Follow {
    pub cached: Vec<Arc<EventRecord>>,                 // from..next, from the ring
    pub live: tokio::sync::broadcast::Receiver<Arc<EventRecord>>, // after that
}

pub enum FollowError {
    Gap { oldest_cached: EventSeq },
    Ahead { next: EventSeq },
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError { /* typed variants */ }
impl SessionError { pub fn code(&self) -> vessel_proto::ErrorCode; }
```

- Records are allocated once as `Arc<EventRecord>` and shared by the
  durable channel, the ring and every follower (amended 2026-11-05 after the
  R checkpoint review; no per-follower deep clones).
- One actor task per session owns the process (PTY or ACP), assigns
  `EventSeq`, appends to the ring, sends to `durable`, then broadcasts.
- `SessionInput` that does not match the mode returns `Unsupported`.
- On node shutdown: kill all, publish final statuses, close `durable`.

### R7. Track R done when
- Gate passes; every crate has module docs with the Why; fixtures for the
  ACP mapper; process-level tests use testkit binaries only.
- A manual smoke (documented in the report, not in CI): one structured
  `claude-code` session through `claude-agent-acp` with a cheap model, if the
  adapter can be installed (`npm i -g @agentclientprotocol/claude-agent-acp`);
  one terminal `codex --version`.

## Track S: spine (agent "오오푸스3", provider `claude-alalt`)

Owns: `vessel-rpc`, `vessel-store`, `vessel-node`, `vessel-client`,
`vessel-cli`. Start with S1-S3, which do not need Track R. Re-add
`vessel-node` and `vessel-cli` to the workspace (remove the `exclude`) only
when they compile against protocol 2.

### S1. `vessel-rpc`: frames over any transport
- `FrameTransport`: anything that is `Stream<Item = Result<String, E>> +
  Sink<String>`, so websocket, SSH stdio (later) and in-memory pipes plug in.
  Static dispatch (generic), no boxing on the hot path.
- Server: `serve<H: Handler>(transport, handler, context)` does the
  `Hello`/`Welcome` negotiation (`ProtocolRange::negotiate`), runs calls
  concurrently with a bound on in-flight calls, and writes frames through one
  bounded outbound queue. It gives the handler context an `Outbound` handle
  so subscriptions can push `Event`/`SubscriptionEnded` frames. `Cancel`
  aborts the call's task.
- Client: `Connection::connect(transport, ClientInfo) -> (Connection,
  Events)`; `call<R: Request>(&self, R) -> Result<R::Response, CallError>`;
  `Events` yields subscription frames. Correlation by `RequestId` with a
  pending map that fails all waiters on disconnect.
- Tests over an in-memory transport: every method round-trips; cancellation;
  protocol mismatch is rejected; disconnect fails pending calls.

### S2. `vessel-store`: data directory and event log (ADR 0004)
- `DataDir` layout: `node.id`, `token`, `vessel.db`. Node id and token
  creation as in ADR 0005 (0600 token, constant-time check helper).
- SQLite via `rusqlite` (bundled), WAL. One writer thread fed by a bounded
  channel of `WriteOp` (create session, append records, update info), which
  commits per batch. Reads (`read_events(session, from, limit)`,
  `list_sessions()`) on `spawn_blocking` with their own connection.
- Startup: mark sessions that were live as `Interrupted` and return them.
- Retention: per-session byte cap and an ended-session count cap (config).
- Migrations through `PRAGMA user_version`.
- Tests on a temp dir: append and read pages; restart marks `Interrupted`;
  retention prunes oldest.

### S3. `vessel-client`
- `NodeClient::connect(url, token, ClientInfo)`: websocket transport
  (`tokio-tungstenite`), then `vessel-rpc`'s `Connection`.
- Typed helpers for every method, and `follow(session, from) ->
  SessionStream` that handles `Lagged` by reading the gap with
  `session.read` and resubscribing. Callers see one gap-free stream.
- No clap, no printing.

### S4. `vessel-node`: assembly
- Modules: `config`, `auth` (bearer check on upgrade), `server` (axum:
  `GET /health`, websocket `/rpc`), `service` (the `Handler` impl, split
  into small per-domain modules: node, harness, session), `persist` (drains
  each `Created::durable` into the store), `subscribe` (follow + store gap
  fill + forwarding to the connection's `Outbound`, with `Lagged` on
  overflow), `shutdown`.
- Sessions from earlier runs come from the store and are listed with live
  ones; `session.read` serves them; `session.send` on them is `Conflict`.
- Until Track R lands `vessel-session`, build against the R6 signatures with
  the node's session service behind a module boundary, and integrate as soon
  as the coordinator merges R6.

### S5. `vessel-cli`
- Commands: `node start`, `status`, `harnesses`, `sessions`, `run <harness>
  [--structured] [-- args]`, `attach <id>`, `send <id> <text>`, `log <id>
  [--from N]`, `kill <id>`.
- `attach` on a terminal session: raw mode (`crossterm`), stdin bytes to
  `TerminalInput::Data`, window size changes to `Resize`, output bytes
  straight to stdout; a detach key (`Ctrl-]`) restores the terminal.
- `attach` on a structured session: renders `AgentEvent`s as text, reads
  prompts from stdin, answers permission requests interactively.
- Token from `--token`, `VESSEL_TOKEN`, or the local data dir.

### S6. Track S done when
- Gate passes; an end-to-end test starts a node in-process on a random port
  with a temp data dir, runs a testkit `fake-acp` structured session and a
  `fake-tui` terminal session through `vessel-client`, kills the node,
  restarts it, and reads the full log back with `Interrupted` status.
- `docs/ARCHITECTURE.md` and `README.md` describe what exists.

## Coordinator duties

- Merge order: R1-R3 early (the node needs `discover`), then R6 as soon as it
  compiles with a PTY path, then the rest. Rebase notices go to both tracks.
- Review every merge against `AGENTS.md`: untyped data, unbounded channels,
  wildcard matches, missing Why-docs, files that grew a second job.
- Keep `docs/` in sync with what is merged.

## After M1 (not started)

- M2: SSH stdio transport (`vesseld attach`), versioned side-by-side
  `vesseld` install and update (Zed's remote server model), peer list.
- M3: relay on an existing overlay (tsnet or WireGuard), connected-hosts
  routing with grant intersection.
- M4: harness profiles (Paseo `extends`; today the node's own environment,
  including `CLAUDE_CONFIG_DIR`, decides the account), fleet sync,
  `vessel init`.
- M5: model routing proxy; LSP-to-MCP bridge.
