# Working on Vessel

Read `README.md`, `docs/ARCHITECTURE.md` and the plan you were assigned
(`docs/plans/`) first. `docs/ORIGIN.ko.md` is the owner's original intent; when
a design question is unclear, that file wins.

Code quality is the first priority, ahead of speed and ahead of feature count.
A smaller feature set with a clean structure beats a larger one that has to be
rewritten. Every rule below is enforced in review; "it compiles" is not the bar.

## 1. Gate before claiming anything works

Run all three, in this order, and paste the result summary in your report:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly these on Linux, Windows and macOS for every push to `main`
and `backend/**` and every pull request (the repository is public, so
hosted runners cost nothing).

Changes under `web/` (the web UI, ADR 0017) add its checks, which CI runs
on Linux: in `web/`, `npm run lint`, `npm run typecheck`, `npm test`,
`npm run build`, then `npm run e2e` against the Rust binaries. Changes to
wire types also regenerate the TypeScript declarations: `VESSEL_WRITE_TS=1
cargo test -p vessel-proto -p vessel-web --features ts --lib typescript`,
and commit `web/src/wire/`.
CI does not replace the local gate: run it before pushing, and report both.
Code that only builds on Linux must be behind `cfg(unix)` with a working (or
explicitly erroring) Windows path.

## 2. Types: no "any", no strings pretending to be types

The goal is that the compiler, not a reviewer, catches mistakes.

- **No untyped data in domain or wire types.** Forbidden: `serde_json::Value`,
  `Box<dyn Any>`/`downcast`, `HashMap<String, Value>`, `Vec<u8>` carrying
  structured data, and `String` fields that hold one of a known set of values.
- **The one exception is opaque passthrough.** Payloads Vessel forwards but
  never inspects (for example a tool's arbitrary input arguments) use the
  `RawJson` newtype from `vessel-proto` (a `Box<serde_json::value::RawValue>`):
  no parse into a tree, no field access. If code needs to branch on a field,
  that field gets a typed struct.
- **Parse at the edge, once.** External formats (harness JSON streams, CLI
  output, files) are decoded into typed structs inside the crate that owns that
  format (`vessel-harness` for harness output, `vessel-store` for files). The
  rest of the code only sees typed values. Unknown event kinds decode into an
  explicit `Unknown { kind: String }`-style variant that is logged, never into
  a `Value`.
- **Newtypes for every identifier and unit**: `NodeId`, `SessionId`,
  `EventSeq`, `ToolCallId`, byte counts, durations. Never pass a bare `u64` or
  `String` where a newtype exists. Constructors validate ("parse, don't
  validate"); make invalid states unrepresentable instead of checking later.
- **Closed sets are enums, matched exhaustively.** No `_ =>` arm on our own
  enums (the lint is on); adding a variant must break every place that has to
  care.
- **Static dispatch by default.** Generics or an enum for a known, closed set
  (harness kinds, session modes). `dyn Trait` only at a real open extension
  point, with a comment saying why. No trait with a single implementation
  "for testing"; use a fake behind a generic parameter or a test-only module.
- **Typed RPC.** Every request type names its response type through the
  `Request` trait in `vessel-proto`. Dispatch is an exhaustive `match` on the
  generated method enum, so an unhandled method is a compile error, not a
  runtime 404.

## 3. Modules: one job each, one direction of dependency

Crate map and allowed dependency direction (arrows point at what a crate may
depend on). Do not add an edge without updating `docs/ARCHITECTURE.md`.

```
vessel-proto    (wire types, ids, Request trait, Handler; no I/O, no tokio)
vessel-rpc      -> proto           (frames over any transport, correlation)
vessel-proc     (command specs, process groups, kill tree)
vessel-harness  -> proto, proc     (one module per harness: discovery, launch)
vessel-pty      -> proc            (terminal sessions in a PTY)
vessel-agent    -> proto, proc     (ACP client; ACP types never leave it)
vessel-session  -> proto, harness, pty, agent   (session actors, seq, fan-out)
vessel-store    -> proto           (data dir, identity, token, SQLite log)
vessel-node     -> proto, rpc, harness, session, store   (daemon assembly)
vessel-client   -> proto, rpc      (typed client; no clap, no printing)
vessel-web      -> proto, client      (`vessel ui`: loopback server, bridge)
vessel-cli      -> proto, client, node, web   (the `vessel` binary)
vessel-testkit  (fake harness binaries; dev-dependency only)
```

ADR 0001 records why; `docs/plans/backend-m1.md` says who owns which crate.

- No cycles, no reaching into another crate's internals, no "utils" or
  "common" grab-bag crate or module. If two crates need the same thing, it
  belongs to the lower crate whose job it is.
- Visibility: private by default, `pub(crate)` next, `pub` only for the
  crate's deliberate API. Every `pub` item is documented.
- Split modules by responsibility, not by size. As a smell, a file over ~400
  lines or a function over ~60 lines needs a reason. A module's top doc comment
  says what it owns and what it must not do.
- Wire types live only in `vessel-proto`. A client never depends on
  `vessel-node` internals to talk to a node.
- Harness-specific knowledge (argv, env, output format, resume flags) lives only
  in `vessel-harness`, one module per harness. Nothing outside that crate may
  match on a harness to decide behaviour; ask the harness module instead.

## 4. Efficiency

- Do not clone what you can borrow. Data fanned out to many readers is
  `Arc<str>`/`bytes::Bytes`/`Arc<T>`, not cloned `String`s and `Vec`s.
- Every channel is bounded, and the code states what happens when it is full
  (block, drop oldest, or disconnect the slow reader and let it resync by
  sequence number).
- No blocking calls on async worker threads. Blocking file or PTY I/O goes
  through `spawn_blocking` or a dedicated thread.
- No polling loops or `sleep` for coordination; wait on a channel, a
  `watch`, a `Notify` or a cancellation token.
- Hot paths (output pumps, event fan-out, log append) allocate once per event
  at most. Batch disk writes; do not `fsync` per line.
- Measure before optimising anything that is not one of the rules above.

## 5. Concurrency and failure

- A long-lived stateful thing (a session, a log writer) is a task that owns
  its state. Others hold a cheap handle (channel sender plus `watch`/broadcast
  receivers). No `Arc<Mutex<HashMap<..>>>` god objects.
- Never hold a lock across `.await` unless it is a `tokio` lock and a comment
  says why it must be.
- Cancellation flows down through `CancellationToken` children: node shutdown
  cancels sessions; a session cancels its pumps.
- No `unwrap`/`expect`/`panic`/indexing/raw arithmetic outside tests (the lints
  enforce it). Use checked or saturating arithmetic and say which and why.
  A misbehaving session or client must never take the node down.
- Child processes run in their own process group and are killed as a group,
  so a harness's own children (MCP servers, shells) do not outlive it.

## 6. Errors

- Each library crate has its own `thiserror` enum. No `anyhow` in libraries,
  no `Box<dyn Error>` in signatures, no `String` errors.
- Errors that cross the wire map to `vessel-proto`'s `ErrorCode` enum plus a
  human message. Map once, at the boundary.
- `map_err` keeps the source (`#[source]`); never discard it.

## 7. Logging and output

- Logging goes through `tracing` with structured fields
  (`tracing::info!(session = %id, "...")`), not formatted strings.
- The CLI writes to a locked stdout handle, never `println!`.
- Never log secrets (tokens, credentials, environment values).

## 8. Tests

- Decoders are tested against recorded fixtures under `tests/fixtures/`
  (real harness output, trimmed, with secrets removed).
- Process-level tests use a small fake harness binary built from the
  workspace, never the real `claude`/`codex`, and never the network.
- A node started in a test gets a temporary home and no harness home
  variables (`HarnessEnv::new(temp)` in `Config::new`); library code never
  reads the user's home or `CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`HOME` from the
  process. Only the binaries do. A test that must change the environment
  runs a child process: the environment is shared by every test in it.
- RPC is tested by round-tripping every method through encode/decode and
  through an in-memory transport.
- Tests are deterministic: no wall-clock sleeps to wait for something; wait on
  the event.
- A test's safety-net timeout (which only turns a hang into a failure) is
  `vessel_testkit::within`: it builds the fakes first and starts the clock
  after. The first fake build of a run can take a minute on a cold runner.
- Inside it, a wait that could hang gets a deadline of its own that names it
  (the node e2e tests' `support::step`, and their output readers, which show
  what they read): a hang on one CI runner then says where it was. The node
  e2e tests also keep the node's debug logs, printed only for a failing test.

## 9. Dependencies

- Prefer std and the crates already in the workspace. A new dependency needs a
  line in the plan or ADR saying what it replaces and why it is worth it.
- Versions live in `[workspace.dependencies]`; crates use `.workspace = true`.

## 10. Documentation (part of done)

- Comments explain why, not what: the intent, the constraint, the rejected
  alternative, and what must stay true when someone changes it.
- Non-trivial decisions get an ADR in `docs/adr/NNNN-title.md` with:
  context, options considered, decision, why, consequences.
- Update `docs/ARCHITECTURE.md`, the crate's module docs and `README.md` in the
  same change when behaviour or structure changes. Docs that disagree with the
  code mean the work is not finished.
- Lints are strict and deliberate. Do not add `#[allow]`; if a lint truly does
  not apply, use `#[expect(..., reason = "...")]`.

## 11. Working as one of several agents

- Stay inside the crates your plan assigns you. If you need a change in a crate
  you do not own (most often `vessel-proto`), stop and ask the coordinator
  instead of editing it; the proto contract is shared.
- Work in your own git worktree and branch. Commit in small, reviewable steps
  with messages that say why. Do not push, do not rewrite shared history.
- Authorship: every commit's author and committer is the maintainer,
  Ingwannu, with the identity the repository's git config sets.
  No `Co-Authored-By` trailers and no tool attribution ("Generated with ...")
  in commit messages, pull request text or comments. The history was
  rewritten on 2026-11-07 to remove them; commit hashes quoted in older
  notes were updated to the rewritten ones.
- Branches: tracks push to their own `backend/<milestone>-<track>` branch;
  the coordinator merges them into the milestone's `backend/m<N>` after the
  local gate. A milestone reaches `main` only by a pull request from its
  branch, once its gate is green, CI is green on all three platforms and
  its plan's manual smokes have passed; it is merged with a merge commit,
  never squashed, so each track's commits and review fixes stay in the
  history. Nobody pushes to `main` directly. Tags `v*` start a release
  (`.github/workflows/release.yml`): only for a release.
- Report: what you built, the three-command gate result, what you deferred
  and why, and any contract change you need.
- The repository is public (ADR 0014): every commit, doc, test fixture and
  CI log can be read by anyone. Never put credentials, customer data,
  internal host names or addresses, or private repository paths in any of
  them; describe the operator's setup in general terms instead.
