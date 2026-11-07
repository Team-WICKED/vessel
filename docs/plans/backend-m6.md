# Milestone 6: IDE context through LSP

Status: Phase A accepted by the coordinator, 2026-11-07 (ADR 0016). Track M6 owns `vessel-lsp`, its
`vessel-testkit` fake, and the LSP integration in proto, node and MCP. M5 and
M7 share protocol 7; LSP rows append to the RPC table in separate commits.
Read `AGENTS.md`, VISION section 5, ORIGIN.ko, ARCHITECTURE and PROTOCOL first.

## Goal

An agent asks the node for symbols, locations, hover, call hierarchy and
diagnostics without an editor attached. The node runs a language server on
demand for a permitted workspace, serves saved disk contents, and exposes
bounded, typed, read-only results locally and through the fleet envelope.

## Out of scope

Editing, rename, code actions, execute-command, debugger support, installing
servers, downloading SDKs, editor unsaved buffers, a new search index, and
sandboxing a language server. No automatic trust in project executable
configuration. Model routing and UI belong to the other tracks.

## What exists to build on

- `vessel-proc::CommandSpec`, `spawn`, `Child`: process groups / Windows jobs,
  kill-on-drop, piped I/O and complete process-tree cleanup.
- `vessel-proto::Request`, the exhaustive RPC table, scope column and `since`;
  protocol negotiation already refuses newer methods on older connections.
- M4's `Principal`, session cwd, delegated origin and scope intersection.
  `fleet.forward`/`relay`/`deliver` already provide routing and revocation.
- `vessel-mcp` uses rmcp and maps tools onto typed client requests.
- `vessel-testkit::within` builds deterministic fakes before starting deadlines.
- Existing serde, tokio, thiserror and which in the dependency
  tree. Reuse these; ACP's newline framing cannot carry LSP Content-Length.

## Contract changes

Protocol 7, additive. All methods use Read, and are forwardable under M4's
intersection rule. The receiving node authorizes roots itself; a source
node's session cwd is not authority over a target's disk.

| Method / MCP tool | Input beyond workspace and page | Result |
| --- | --- | --- |
| `lsp.workspace_symbols` / `vessel_lsp_workspace_symbols` | language, query | symbol page |
| `lsp.document_symbols` / `vessel_lsp_document_symbols` | file | symbol page |
| `lsp.definition` / `vessel_lsp_definition` | file, position | location page |
| `lsp.references` / `vessel_lsp_references` | file, position, include_declaration | location page |
| `lsp.hover` / `vessel_lsp_hover` | file, position | bounded text and optional range |
| `lsp.implementations` / `vessel_lsp_implementations` | file, position | location page |
| `lsp.type_definition` / `vessel_lsp_type_definition` | file, position | location page |
| `lsp.incoming_calls` / `vessel_lsp_incoming_calls` | file, position | call page |
| `lsp.outgoing_calls` / `vessel_lsp_outgoing_calls` | file, position | call page |
| `lsp.diagnostics` / `vessel_lsp_diagnostics` | optional file, language | diagnostic page |

Wire types live in `vessel-proto::lsp`, never expose server JSON. Language,
severity, symbol kind, freshness and markup format are enums. Workspace and
file paths are validated types. Position has nonzero line and column (Unicode
scalar values, not bytes, grapheme clusters or UTF-16 units); ranges have an
exclusive end. The boundary converts both directions against the saved
snapshot, rejecting invalid positions and split surrogate pairs.

A page has items, next offset, truncation and freshness. Limit defaults to
50, at most 200; collect at most 2,000 items and 1 MiB per response. Stable
sorting precedes paging. Offset pagination is explicitly best effort across
edits, not a promise of a persistent snapshot. A page reports incomplete
server data separately from our own truncation. Unsupported capability is
`unsupported`, missing server `unavailable`, bad path/position
`invalid_request`, unauthorized root `forbidden`. Empty is not an error.

## Crates / seams

- New `vessel-lsp -> vessel-proto, vessel-proc`: `registry` (inert executable
  selection), `framing` (bounded Content-Length transport), `protocol`
  (typed LSP edge), `process` (correlation and lifecycle), `documents`
  (saved snapshots, sync, positions), `manager` (bounded workspace/language
  actors), `query` (typed conversion and page limits), `error`.
- `vessel-node -> vessel-lsp`: resolve principal roots before dispatch;
  config owns explicit roots, executable overrides and lifecycle limits.
  No wire request can choose a command or initialization options.
- `vessel-mcp`: typed arguments and schema; optional target node uses the
  existing forwarding path. `vessel-client` remains a generic typed caller.
- `vessel-testkit`: `fake-lsp`, with independent typed fixture messages.
  No production dependency on testkit. Document the new edges in ADR 0001.

Dependencies proposed: `notify` for native file events (instead of periodic
filesystem polling), `url` for correct platform file URI conversion, and a deliberately small serde LSP subset rather than
`lsp-types` wholesale: its initialization options, diagnostic data and call
hierarchy data use Value. Opaque call-hierarchy data is `RawJson` and is only
round-tripped back to the server. No field inspection of opaque data.
Reconsider using Value-free leaf types if that measurably reduces code.

## Data flow

1. MCP caller authenticates as its session. Read scope and target authority
   are checked, locally or at the final fleet destination.
2. Canonicalize the requested root and file under permitted roots, reject
   non-file URIs, escaping symlinks and non-regular files. A local agent may
   use its session cwd; a remote agent may use only destination-configured
   roots (or an explicitly authorized destination session in a later API).
3. Manager chooses `(canonical workspace, language)`, accounts the server to
   the caller's allowed root for fair slot sharing, and a trusted server command:
   rust-analyzer, typescript-language-server --stdio, basedpyright-langserver
   --stdio (fallback pyright-langserver), gopls, clangd. PATH is supplied by
   the binary, not read implicitly by library tests. Overrides are node-only.
4. Lazy spawn, initialize minimal capabilities with UTF-16, initialized.
   One server actor owns its process, pending replies, documents and progress.
   Queues are bounded; saturation returns unavailable, never an unbounded
   pending map. Drain stderr without publishing arbitrary server text.
5. Open a bounded UTF-8 saved document on demand. Watch native file events;
   reread before each query as a correctness backstop for lost events. Send
   didOpen / didChange (respecting full or incremental sync) / didClose.
6. Wait for known progress tokens within a deadline, then query. Standard LSP
   has no universal 'indexing complete': reply with unknown/busy/ready status
   from observed progress, never infer complete workspace coverage from
   silence. Push diagnostics are a cache with explicit coverage/freshness;
   pull diagnostics are used only when advertised.
7. Normalize and filter every server location to authorized roots, bound
   strings and result counts, convert positions against saved text, sort,
   page and return. Never open a server-selected path before checking it.
8. Idle actors send shutdown then exit, with a kill-and-reap deadline.
   Crashes fail pending requests and invalidate cached state; next demand
   restarts after exponential backoff. Cancellation flows from node shutdown.

## Steps

1. Plan and grounding; three-command baseline gate; commit and push.
2. Independent foundations: strict framing and Unicode position conversion,
   inert registry, deterministic fake and transport tests.
3. Separate contract commit: typed LSP requests/results, append-only RPC
   rows, protocol 7 and exhaustive dispatch stubs / roundtrip tests.
4. Process actor: initialize, server requests, notifications, correlation,
   cancellation, malformed/oversized frame handling and process cleanup.
5. Document snapshots, native watcher, progress/diagnostic cache, idle/crash
   lifecycle and node-owned registry limits. Default maximum 4 servers,
   5-minute idle, 30-second request deadline, 1..30-second restart backoff;
   initialization may need a separate longer deadline for large projects.
6. Queries and limits, node root authorization, local/fleet RPC integration.
7. MCP tool surface and end-to-end fake tests; manual real-server smoke;
   docs, full local gate and three-platform CI before coordinator merge.

Every push runs fmt check, workspace clippy with warnings denied, and the
workspace tests. Commits explain the constraint or failure they address.
Build artifacts use the worktree's preconfigured external target directory.

## Security

Read-only describes RPC capabilities, not an OS sandbox: installed LSP servers
may read imports outside the root, load plugins, spawn tools or write caches.
Use only operator-trusted executables/workspaces; do not claim confinement of
the child process. Disable build scripts/proc macros/check-on-save in the Rust
preset where supported; do not send edit/execute commands or applyEdit.
Request-scoped output filtering is still mandatory for shared server caches.
Reject server workspace/applyEdit; answer configuration/workspace-folder and
progress requests with typed responses; unsupported requests receive a typed
JSON-RPC error. Do not advertise dynamic features we cannot honor.

All file reads, watcher events, diagnostic related locations, symbol locations
and call hierarchy results cross the same confinement boundary. No arbitrary
URI scheme, UNC network access or relative traversal. Bound header size,
payload size, files, documents, progress tokens, pending calls, diagnostic
cache and string sizes. Timeouts must not leave pending IDs or process trees.
Tests use temporary homes and fake executables, never real accounts/harnesses.
No credentials, private paths or host details in docs/fixtures/logs.

## Done criteria

- Fake-driven tests cover initialize/initialized, serialized correlated
  replies with notifications interleaved, server requests, UTF-16 emoji/combining/CRLF conversion, sync and
  deletion, diagnostics/progress, cancellation, EOF, malformed/oversized
  headers/bodies, crash/backoff, idle and process-tree cleanup.
- Each RPC roundtrips; older protocols refuse before dispatch. Read-only
  principals succeed only within permitted roots, denied calls never spawn
  or read. Test symlink escape, outside-root results, remote agent origins,
  absent root grants, revoked grants and paging/truncation.
- MCP test actually calls the tools against a fake-driven node; all tools
  return typed bounded content with useful unsupported/missing-server errors.
- Local three-command gate passes; Linux/macOS/Windows CI passes.
- `docs/smoke/m6-lsp.py` starts an isolated node and MCP connection, then asks
  rust-analyzer about this repository: workspace/document symbols,
  definition/reference/hover, implementations/type definition, call hierarchy
  and diagnostics. Modify only a temporary copied fixture to check resync.
  Record tool outputs and incomplete/unsupported results honestly. Script
  fails with an actionable prerequisite message if rust-analyzer is absent.
- Optional real-server integration probes detect PATH availability and state
  a skip reason if absent; they are not evidence for every server preset.

## Open decisions (accepted by the coordinator, 2026-11-07)

All seven recommendations below are accepted. The coordinator additionally
requires an operator-only full-analysis opt-in per configured root, never
on the wire. A local cwd gains this opt-in only by matching that configured
root. Conservative Rust defaults prevent a remotely forwarded Read call from
executing build scripts or proc macros; the accuracy cost is unresolved
unexpanded derives. Every answer must identify covered files, observation
time and whether indexing completion was actually observed.


1. **LSP types.** Recommend a minimal typed serde edge: lsp-types exposes
   Value in key structures, incompatible with our wire/domain rule. Alternative:
   use its safe leaf types plus wrappers, at the cost of dual representations.
2. **Position columns.** Recommend 1-based Unicode scalar columns and strict
   UTF-16 conversion; alternatives are 1-based UTF-16 (surprising for agents)
   or bytes (language/tool dependent). State the convention in every schema.
3. **Remote roots.** Recommend explicit destination-configured roots for
   remote agents, with local session cwd as the local implicit root. Never
   trust an origin-provided cwd. Alternative: require an owned destination
   session ID, which complicates simple repository lookups.
4. **Diagnostics completeness.** Recommend cached push + advertised pull,
   and explicit freshness/coverage. Alternative: recursively open every file,
   costly and still no universal indexing guarantee. Never claim that an
   empty cache is a clean workspace.
5. **Saved disk / editor overlays.** Recommend saved files only for M6;
   alternative: an editor-buffer ownership protocol now, which couples M7.
6. **Trust and defaults.** Recommend no server installs, operator-owned
   presets, bounded native watchers and conservative Rust execution settings.
   Alternative: full editor defaults including build scripts/proc macros,
   giving better semantic coverage but executing more repository code.
7. **Paging.** Recommend bounded deterministic offset pages with freshness;
   alternative: principal-bound expiring snapshot cursors, stronger consistency
   but another cache and lifecycle to secure. Revisit if consumers need it.

## Grounding

Read 2026-11-07, ideas only; no GPL code copied or linked into Vessel.
Zed source inspected at commit `654efae441bc8eb117d0042bb28cd7cf3570bdc9`:

- `crates/lsp/src/lsp.rs`: LanguageServer owns correlation IDs, handlers,
  capabilities and I/O; initialize records capabilities before initialized;
  shutdown sends shutdown/exit and drains with deadlines. Adopt the lifecycle
  separation, implement with Vessel actors, bounded queues and vessel-proc.
- `crates/project/src/lsp_store.rs`: server seeds include worktree, binary
  settings and toolchain; start is lazy and worktree trust precedes launch.
  Documents retain versioned snapshots for diagnostics and UTF-16 locations;
  work-done progress and server-specific diagnostics readiness are distinct.
  Adopt explicit root identity and snapshots; do not assume silence is ready.
- Zed's editor entities, dynamic handlers, shared locks and extension adapters
  are inappropriate dependencies for a headless node. Keep the transport
  separate from registry, saved-document state and node authorization.

Primary references: [LSP 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/),
[lsp-types InitializeParams](https://docs.rs/lsp-types/0.97.0/lsp_types/struct.InitializeParams.html),
[Zed source](https://github.com/zed-industries/zed/tree/654efae441bc8eb117d0042bb28cd7cf3570bdc9/crates/lsp).


## Implementation record (2026-11-07)

The seven decisions were accepted, with later conservative execution
requirements recorded in ADR 0016. Implemented: ten protocol 7 Read rows,
local/fleet root policy, MCP tools, typed stdio runtime, bounded lazy actors,
native saved-file watches, push/pull diagnostics, explicit context metadata,
UTF-16 conversion, sorted paging, crash backoff and idle/shutdown cleanup.
`--lsp-config` supplies explicit operator roots, executable overrides and
trusted auxiliary tool/cache paths. The process-level env-clear primitive is
its own commit because other tracks share vessel-proc.

A deliberate refinement: one query at a time per server, with eight queued,
while different servers run independently. This binds each query to one
saved version and avoids an unnecessary concurrent pending-request map;
notifications and server requests remain interleaved with response waits.

Manual smoke: rust-analyzer 1.98.1, through an isolated session's MCP
credential, querying this repository's `to_utf16` (no real harness account).
All ten tools returned successfully. Document symbols 15; workspace search
1; definition 1; references 7; hover nonempty; incoming calls 3; outgoing
calls 1; implementations/type-definition empty for this function; file
cached diagnostics 3. None claimed complete workspace diagnostics. A first
syntax-only attempt was rejected as insufficient; the readiness fix is in
ADR 0016. The script is `docs/smoke/m6-lsp.py` and the ordinary test gate
ignores the real-server test with its prerequisite reason.

Other real server binaries were not exercised: their conservative launch
policies are covered with fake repository traps and primary-source review,
not claimed as real-server interoperability proof. Editor overlays,
installers, editing/code-action tools and a process sandbox remain out of
scope. Local and three-platform gate results are recorded after the final
push; an early baseline run timed out during unrelated host I/O saturation,
and no existing timeout was changed.

### Local gate

Final local gate on 2026-11-07, code commit `8fefe2d`:
`cargo fmt --all -- --check` passed;
`cargo clippy --workspace --all-targets -- -D warnings` passed;
`cargo test --workspace -- --test-threads=4` passed: 763 passed, 0 failed,
1 intentionally ignored real-server test (passed separately as recorded
above). Builds used four jobs and the worktree's external target directory.
The explicit forwarding allowlist test was updated to pin exactly the ten
approved LSP rows. No existing test timeout was relaxed.

Three-platform CI runs on the track push; the coordinator merges only after
Linux, macOS and Windows pass on the submitted head. CI run details are in
the track handoff, rather than claiming platform execution from Linux alone.


## Hardening round (2026-11-07)

An independent review of the conservative-mode claim found one high and
several lower issues; regression tests reproduce each before the fix.

| Finding | Fix | Proof |
| --- | --- | --- |
| High: rustup proxy honoured a child `path` toolchain (reproduced, rust-analyzer 1.98.1) | `toolchain`: absolute rustc/cargo resolved once, `RUSTUP_TOOLCHAIN` pinned, conservative only; `policy`: `linkedProjects` = checked Cargo projects, every level to `/` checked, discovery off, `unavailable` without a project | `real_rust_analyzer::a_child_rust_toolchain_never_runs_a_repository_binary` (ignored, real server; failed before: the repository rustc ran), `policy::child_cargo_project_configuration_is_refused_before_spawn`, `launch::conservative_rust_launches_with_pinned_tools_and_linked_projects`, `toolchain` and `registry` unit tests |
| Every server ran with the repository as cwd; `TMPDIR` was the home | node-owned `<data>/lsp/cwd` and `<data>/lsp/tmp` | `launch::conservative_rust_launches_with_pinned_tools_and_linked_projects` |
| Medium (Windows): pyright finds `python` by name | neutral cwd; `python_interpreter` answered as `python.pythonPath`; Windows conservative Python refused without it | `launch::python_gets_the_pinned_interpreter` (the refusal branch runs on the Windows CI runner) |
| No eviction of open documents | LRU `didClose`, watch reference counts | `the_least_recently_used_document_makes_room_for_a_new_one` (failed before) |
| Slots keyed by requested subdirectory | per-allowed-root fairness with parked admissions | `subdirectories_of_one_root_cannot_lock_out_another_root` (node e2e, failed before), `a_root_holding_every_slot_yields_one_to_another_root` |
| Symlink existence oracle | one `Forbidden` answer for missing/escaping/non-regular | `missing_and_escaping_symlinks_get_the_same_answer` (failed before), node e2e |
| FIFO swapped in before open | non-blocking open, `fstat` on the handle | `a_fifo_is_refused_without_blocking` (failed before: blocked) |
| TypeScript plugin options | `plugins: []` sent explicitly, so no plugin loads from the workspace (only ones installed beside the pinned tsserver can); child tsconfig gap in ADR 0016 | `launch::typescript_receives_no_plugin_locations` |

Deliberate deviation: the review suggested keying servers by the allowed
root. Servers stay keyed by the requested workspace and are *accounted* to
the allowed root instead (ADR 0016, "Review hardening"): a broad allowed
root such as a session cwd of a home directory would otherwise become the
analysis root. Full-analysis roots keep rustup's own toolchain selection,
because full analysis permits project configuration.

`vessel-lsp` gains the Unix `nix` dependency (`fs` feature, already in the
workspace) for `O_NONBLOCK`. `vessel-testkit` gains `rust_toolchain` (a
stand-in toolchain directory) and `fake-lsp --record-launch`/`python-config`.

### Hardening gate

Local gate on 2026-11-07, code commit `2edc7a9`: `cargo fmt --all -- --check`
passed; `cargo clippy --workspace --all-targets -- -D warnings` passed;
`cargo test --workspace -- --test-threads=4` passed: 777 passed, 0 failed,
4 ignored real-server tests. Those ran separately against rust-analyzer
1.98.1 and passed: the three `real_rust_analyzer` regressions and the MCP
smoke (`docs/smoke/m6-lsp.py`'s test), whose counts match the earlier
record (document symbols 15, workspace 1, definition 1, references 7, hover
nonempty, incoming 3, outgoing 1, cached diagnostics 3). Three-platform CI
results are in the track handoff.


## Verification round (2026-11-07)

A second review at `10d64e4` confirmed the round above and the decisions
on slot keying, full mode, root scope and lazy resolution, and found three
P2 issues (each test failed before its fix) and two inaccurate claims:

| Finding | Fix | Proof |
| --- | --- | --- |
| An explicit `rust_toolchain` pin inside another configured root ran that root's tools (reproduced with rust-analyzer 1.98.1) | a pin is unverified, like a linked/path default: outside every configured root | `real_rust_analyzer::review_explicit_path_toolchain_cannot_live_in_another_root` (ignored, real server), `launch::an_explicit_toolchain_pin_inside_another_configured_root_is_refused`, `toolchain::pinned::an_explicit_pin_is_unverified` |
| A burst from one root retired an equal root's slot: retiring slots and their waiting admissions were not counted | a retiring slot is reserved for its admission and counts toward that root; the freed slot goes to it first; parking room is checked before retiring | `review_parked_replacements_do_not_steal_an_equal_roots_slot`, `no_server_is_retired_for_an_admission_that_cannot_wait` |
| A byte-budget eviction during a bulk refresh made the refresh fail and restart a healthy server | the pass skips entries evicted earlier in it | `review_byte_eviction_during_refresh_keeps_server_healthy` |
| ADR: "project files do not outrank the client" | rewritten: scoped `rust-analyzer.toml` can outrank client settings in 1.98.1, but the execution paths ignore scoped config; re-verify when the pinned version moves | `real_rust_analyzer::review_rust_layout_variations` (ignored; nine layouts, nothing runs) |
| TypeScript plugin claim | "no plugin from the workspace; only plugins installed beside the pinned tsserver can load" | documentation |

Local gate on 2026-11-07 at `5f7cf56`: `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets -- -D warnings` passed;
`cargo test --workspace -- --test-threads=4`: 782 passed, 0 failed, 6
ignored real-server tests. Those passed separately against rust-analyzer
1.98.1: the five `real_rust_analyzer` tests (including the pin regression
and the nine-layout matrix) and the MCP smoke, with unchanged counts.
