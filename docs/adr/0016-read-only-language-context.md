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
