# ADR 0016: read-only language context from saved files

Date: 2026-11-07. Status: accepted by the coordinator; implementation staged
in `docs/plans/backend-m6.md`.

## Context

Agents need the symbols, call graph and diagnostics an IDE knows even when
no editor is connected. Read-scoped calls can cross the fleet, so starting a
language server must not silently enable repository build scripts or expand
the caller's filesystem authority.

## Options considered

1. Reuse an editor's buffers and server processes: couples correctness to an
   editor being attached, and needs buffer ownership and synchronization.
2. Import lsp-types throughout the domain: its aggregate InitializeParams,
   diagnostic data and call-hierarchy data contain serde_json::Value, contrary
   to Vessel's typed-domain contract.
3. A headless, typed subset of LSP, isolated behind vessel-lsp, using saved
   disk snapshots and the existing process-group runtime.

## Decision and why

Choose option 3. Use typed serde edge structures and ignore unknown fields;
opaque server data that must be returned unchanged remains RawJson. No
untyped result trees, arbitrary LSP methods or edit/execute tools enter the
wire contract. Native filesystem events wake document synchronization;
queries recheck saved text to tolerate lost events. The transport has fixed
header/body budgets and fails closed on malformed framing.

The node owns root authorization. Local sessions may query their cwd and
its subdirectories; delegated calls may query only configured destination
roots. Canonicalize requests and results, including diagnostic related
locations, before reading. Server outputs do not grant authority. This is
API confinement, not an OS sandbox around a trusted executable.

Presets disable build scripts, proc macros and check-on-save by default.
Full analysis requires an operator-only opt-in for a configured root, never
an RPC argument, including when the caller is a local session. No installers,
downloads or executable workspace-trust prompts are provided by Vessel.

Agent positions count lines and Unicode scalar columns from one, while LSP
positions count lines and UTF-16 units from zero. Conversion uses the saved
snapshot and rejects invalid columns and split surrogate pairs. Combining
marks count separately; CRLF is one line ending. The final empty line exists.

Results use bounded, sorted offset pages. Every answer identifies coverage,
observation time and indexing state; a silent diagnostics stream is unknown,
not a clean workspace. Use cached push diagnostics and capability-advertised
pull diagnostics. No universal LSP indexing-complete signal exists.

## Consequences

- Unexpanded derives and disabled build scripts reduce Rust analysis accuracy.
  A trusted server can still read imports or write caches: this is not a
  process sandbox, and configured executables/workspaces must be trusted.
- Saved-file context works without editor integration, but unsaved changes
  are intentionally absent. Overlays need a later protocol.
- Offset pages may shift between edits. Snapshot cursors could strengthen
  consistency later, at the cost of a principal-bound expiring cache.
- LSP compatibility is limited to the implemented subset. Unknown capabilities
  are ignored and unsupported queries return explicit errors.
- `vessel-lsp` owns transport/process/document state; `vessel-node` owns
  policy; `vessel-proto` owns agent-facing types; MCP is only an adapter.
- Zed's lifecycle, worktree keys, snapshots and progress separation informed
  the design. No GPL source was copied; grounding is in the milestone plan.

## Explicit child environments

`vessel-proc::CommandSpec::env_clear` is opt-in, with the inherited default
unchanged for harnesses. Both piped and PTY runners honor it. Removing a list
of known variables is insufficient: a future loader/runtime variable could
still enter a server. LSP commands start empty, then receive a filtered PATH,
an explicit home, locale, temporary-directory variables and the small
per-language policy set. Windows receives the node-supplied `SystemRoot`;
clearing the environment must not make platform DLL/runtime lookup depend
on a repository path. A process-crate child test enumerates its environment
and verifies that only explicitly supplied variables remain.

## Conservative execution policy (coordinator additions)

A Read-scoped caller must not turn normal analysis into repository code
execution. Operator-selected binaries and the explicit operator home remain
trusted. Full analysis is an operator-only per-root choice, never an RPC
argument. It does not restore arbitrary inherited environment or downloads.

| Server | What could execute | Conservative policy | Full analysis |
| --- | --- | --- | --- |
| rust-analyzer | `build.rs`, proc macros, Cargo wrappers/rustc/runners/aliases, forced `[env]` loader injection, a repository-selected rustup toolchain, projects the server discovers itself | Build scripts, proc macros and check-on-save off; offline Cargo; `RUSTUP_AUTO_INSTALL=0`; wrappers empty. **Toolchain:** resolved once per node to absolute binaries, never through a rustup proxy (from the operator pin, else rustup's *default* via `rustup default` and `rustup which --toolchain`, run in the node-owned empty directory, else a plain `rustc`/`cargo` on PATH); `RUSTC`/`CARGO` are those binaries, the toolchain's `bin` is first on PATH and `RUSTUP_TOOLCHAIN` names the toolchain directory, so no `rust-toolchain(.toml)` is consulted. A linked or path default toolchain must lie outside every configured root; any toolchain must lie outside the served root. **Projects:** `linkedProjects` lists exactly the Cargo projects the policy checked (the nearest manifest at or above the root, else manifests one directory below it), which turns the server's own discovery off; a root with none is `unavailable` for Rust. Every directory from the root and from each project up to `/` is checked: Cargo configs there accept only inert `build.target-dir/jobs/incremental` and `profile.*.debug`; `rust-analyzer.toml`, `(.)rust-project.json`, other settings, includes, symlinked manifests and path/named toolchain files are refused before launch and before queries. Operator-home Cargo config remains trusted. | Enables build scripts, proc macros and check-on-save; permits project configuration, including a project toolchain file honoured by the rustup proxies on PATH and the server's own project discovery. |
| typescript-language-server | Workspace `node_modules/typescript/tsserver.js`, plugins and automatic npm type acquisition | Trusted global/operator `tsserver.js` pinned in initialization; ATA off; `plugins` sent explicitly empty, so tsserver gets no `--globalPlugins`/`--pluginProbeLocations` and (never passed) no `--allowLocalPluginLoads`: a tsconfig plugin then resolves only beside the trusted engine. Plugin/extends/project-reference configuration at the root and its parents is refused. A `tsconfig.json` in a *child* directory is not checked (tsserver reads the nearest one per file); with the settings above its plugins still cannot load from the repository, and extends/references there are inert JSON. Relative/empty/repository PATH entries never select tools. | May use the workspace engine and project configuration; ATA remains off. |
| gopls | A downloaded Go toolchain, dependency fetching, cgo toolchains | `GOTOOLCHAIN=local`, `GOFLAGS=-mod=readonly`, `GOPROXY=off`, `GOSUMDB=off`, `GOENV=off`, `GOVCS=*:off`, `CGO_ENABLED=0`. The exposed queries never invoke `go generate`, test or run. Its package discovery uses trusted `go list`/`go env`, which do not run package initializers. | Allows cgo; download and generate/run restrictions remain. |
| clangd | Compiler-driver queries from compilation databases, executable project configuration | Empty `--query-driver` list, reject operator arguments that would add entries in conservative mode, `--enable-config=false`; no code-action/execute-command path. | Operator command arguments and project clangd configuration may apply. |
| pyright / basedpyright | Python used to discover search paths, a configured venv/interpreter, startup imports | No interpreter/venv/extends overrides: only the supported inert JSON/TOML settings are accepted. `PYTHONSAFEPATH=1`, `PYTHONNOUSERSITE=1`, no `PYTHONPATH`; startup-shadow files at the root are refused. An operator `python_interpreter` is answered as `python.pythonPath`; without one the trusted PATH selects the interpreter, except on Windows, where Node's spawn search can look in the working directory first: there conservative Python is `unavailable` until the operator pins an interpreter (not exercised on a Windows host yet). | Allows project interpreter/venv configuration. |

All servers receive `CommandSpec::env_clear`, an explicit filtered PATH,
locale, the operator home, Windows `SystemRoot` when supplied, and run with
a node-owned empty directory (`<data dir>/lsp/cwd`) as their working
directory, never the repository; `TMPDIR`/`TMP`/`TEMP` point at
`<data dir>/lsp/tmp`, never the account home. No agent credentials,
`NODE_OPTIONS`, `LD_PRELOAD` or other daemon environment survives.
Repository configuration is rechecked before a query; refusals say
`unavailable: project configuration can run code; the operator can enable
full analysis for this root`. Conservative parsing rejects comments or
unsupported config syntax rather than guessing that it is harmless.

Full analysis follows the operator's roots, not the request: a root
configured with `full_analysis: true` makes every workspace below it full,
including local session cwds and fleet callers' workspaces nested inside it.
Configure full analysis only on the directories whose build tools you trust.

These are application checks, **not an OS sandbox**. A malicious process
running as the same OS account can race filesystem changes, replace trusted
executables or alter operator-owned settings. Server bugs and native compiler
parsers are also outside this boundary. Install only trusted tools, keep
operator settings outside writable repositories, and use OS isolation for
untrusted actors. A full-analysis root deliberately allows its build tools.

Grounding (primary sources, read 2026-11-07):
[rust-analyzer configuration](https://rust-analyzer.github.io/book/configuration.html),
[TypeScript server configuration](https://github.com/typescript-language-server/typescript-language-server/blob/master/docs/configuration.md),
[gopls settings](https://go.dev/gopls/settings),
[clangd configuration](https://clangd.llvm.org/config),
[Pyright interpreter execution](https://github.com/microsoft/pyright/blob/main/packages/pyright-internal/src/common/fullAccessHost.ts).
Pyright does execute Python for search-path discovery; saying it never
executes an interpreter would be incorrect.


## Rust readiness and saved project changes

The real-server smoke exposed a distinction a fake cannot: disabling Cargo
reloads before initial loading leaves rust-analyzer with document syntax but
no usable crate graph. The client advertises its typed `serverStatus` extension,
allows the one preflighted initial load, waits for quiescence within its
startup deadline, then disables background Cargo reloads before serving a
query. Config/manifest metadata changes cause a policy recheck and a new
server generation, so a changed project cannot silently reuse the old graph.
LSP ContentModified/ServerCancelled reads retry at most three times within
the whole-query deadline. No sleep loop guesses readiness.

The smoke uses an already-installed analyzer and toolchain with rust-src,
plus an explicitly selected trusted offline Cargo cache, under an isolated
server/agent home. It neither installs components nor uses real harness
accounts. Missing rust-src/cache can reduce analysis; offline does not mean
returning syntax-only results should be reported as full semantic coverage.


## Review hardening (2026-11-07)

An independent review checked the central claim that conservative mode
runs no repository code. It reproduced, with rust-analyzer 1.98.1, a root
without its own `Cargo.toml` whose `sub/Cargo.toml` rust-analyzer found by
itself: it then ran the rustup proxy with cwd `sub`, which honoured a
`path` toolchain inside the repository. Two layers now close it, each
sufficient alone: no rustup proxy is ever relied on (absolute tools and
`RUSTUP_TOOLCHAIN`, `toolchain` module), and the server loads only the
projects the policy checked (`linkedProjects`, `policy` module). The
review's other findings and what replaced them:

- **Server slots.** Four subdirectories of one allowed root took every slot.
  Slots are now accounted to the allowed root (a configured root or the
  session cwd): when all are taken, a root holding fewer servers than the
  busiest root takes over that root's least recently used server, otherwise
  a root recycles its own; the request waits for the slot instead of being
  refused. Servers still analyse the requested workspace, not the allowed
  root: rooting one server at a broad allowed root (a home directory as a
  session cwd) would make pyright/gopls index the whole tree and put
  user-installed tools inside "the root".
- **Open documents** are closed least-recently-used first (`didClose`),
  so a long-lived server keeps serving new files.
- **Paths** that are missing, escaping or not regular files get one answer,
  so a symlink inside a root does not reveal whether its outside target
  exists. Saved files open non-blocking and are checked with `fstat` on the
  open handle, so a FIFO swapped in after the check cannot park a worker.
- `nix` (already a workspace dependency on Unix) supplies `O_NONBLOCK` to
  `vessel-lsp`, rather than a hand-written per-platform constant.

Residual gaps: member crates' `rust-analyzer.toml` below a linked project
are not checked (the review observed rust-analyzer 1.98.1 not letting a
project file outrank the client's settings, and the project's own file is
refused); projects deeper than one directory below a manifest-less root are
not linked; the Windows interpreter rule is covered by CI only, not by a
real pyright run on Windows.
