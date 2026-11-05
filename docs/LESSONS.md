# Lessons from Zed and Paseo

Read 2026-11-05 from `zed-industries/zed` (main) and `getpaseo/paseo` (main).
Ideas only: Zed is GPL-3 and Paseo is a separate product. Paths are relative
to each repository so the claims can be checked.

## What we take

| From | Idea | Where it lands in Vessel |
| --- | --- | --- |
| Zed `proto`/`rpc` | Each request type names its response type (`RequestMessage::Response`); one macro table generates the plumbing | `vessel-proto::rpc` (`Request`, `rpc_methods!`) |
| Zed `proto` | Wire `ErrorCode` enum next to a message, so clients can branch on errors | `vessel-proto::ErrorCode` |
| Zed `agent_servers` | Drive Claude Code, Codex, Gemini, OpenCode through ACP (JSON-RPC over stdio), via adapters where the harness does not speak it | Structured session mode, `vessel-agent` (ADR 0003) |
| Zed `acp_thread` | Tool calls as a status machine; permission requests as first-class pending objects | `vessel-proto::agent` |
| Zed `remote` | Thin `proxy` process that bridges SSH stdio to a long-lived daemon; exit codes as a typed side channel | Phase 1 SSH transport (`vesseld attach`) |
| Zed `remote` | Versioned binaries side by side, download on the remote first, atomic rename, delete unused old versions | Phase 1 "fast update" of `vesseld` |
| Zed `remote` | Sequence numbers and acks for resume after a drop | `EventSeq` cursors (ADR 0004) |
| Zed `lsp` | Cancel-on-drop requests, per-request timeout, three-way result, a fake server for tests | Phase 4 LSP bridge |
| Zed `settings` | All-`Option` "content" layers merged into a resolved struct | Node config (Phase 2) |
| Zed tests | Fakes everywhere (FakeFs, fake language server, fake ACP agent), deterministic scheduling, injected disconnects | `vessel-testkit`, `tokio::time::pause` |
| Paseo `server/agent` | Two-level stream: lifecycle events plus seq-numbered timeline rows; a closed set of tool-call details | `SessionEvent` / `AgentEvent` |
| Paseo `provider-registry` | A provider "extends" another with different env (a second Claude account is `CLAUDE_CONFIG_DIR`) | Harness profiles (Phase 2) |
| Paseo `protocol` | Hello carries protocol version and capabilities; one generic error reply; live stream plus authoritative paged fetch | `Hello`/`Welcome`, `session.read` |
| Paseo relay | Stable node identity, key-pinned pairing URL, semantic permissions per principal, grant intersection for routed calls | Phase 1 peers |
| Paseo plugins | Out-of-process plugins over versioned JSON-RPC; `subject.operation.before/after` hook names | Later plugin layer |

## What we avoid

| Seen in | Problem | Vessel rule |
| --- | --- | --- |
| Zed `rpc` | Handlers keyed by `TypeId`, `downcast().unwrap()` | Static dispatch from one table; exhaustive `match` |
| Zed `rpc` | Handlers `bail!` strings that arrive as `Internal` | Typed errors mapped to `ErrorCode` once |
| Zed `remote` | Unbounded channels and replay buffers | Every channel bounded, overflow policy stated |
| Zed | God files (`acp_thread.rs` 25.9k lines, `lsp_store.rs` 16k) | One job per module; size is a smell |
| Zed `agent_servers` | Two ACP versions supported at once (~19k lines) | Pin one ACP version behind `vessel-agent` |
| Paseo `server/session.ts` | 9k-line class with a 170-arm switch | Small per-domain handlers behind the generated `Handler` |
| Paseo | Timeline types defined three times and drifting | Wire types only in `vessel-proto` |
| Paseo | `unknown` bags (`[key: string]: unknown`, `z.unknown()`) | No `Value`; `RawJson` only for opaque passthrough |
| Paseo | Random timeline epoch per daemon run; restart invalidates every cursor | Durable, gap-free `EventSeq` per session |
| Paseo | Resume re-reads the whole provider transcript into memory | Vessel keeps its own paged log |
| Paseo | Provider names switched on in generic code | Harness knowledge only in `vessel-harness` |
| Paseo | One bearer token per daemon run; caller identity from a query parameter | Token checked on connect; per-agent principals before agents call the node |
| Paseo | Harness processes die with the daemon, re-prompted on restart | Sessions marked `Interrupted`; structured ones resume through ACP `session/load` |
| Paseo | Two crypto modes kept alive for compatibility | One transport security story per phase |
