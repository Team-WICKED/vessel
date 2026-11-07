# Backend milestone 5: model routing

Status: accepted (2026-11-07, coordinator's decisions below), Track M5;
ADR 0015. ROADMAP Phase 3,
VISION section 4: "every harness can use every model, through a routing
proxy the node runs locally". Protocol 7 is shared by milestones 5 to 7;
this plan's rows are appended at the end of the table with `since 7`.

## Goal

A session launched with a profile that names a model route talks to that
route's model, whichever harness it runs, through a proxy inside the node:

- The node runs one loopback listener. A routed session gets the
  listener's address and a bearer token of its own in its environment (or
  its configuration overrides), and its harness speaks its usual API to
  it: Claude Code the Anthropic Messages API (`ANTHROPIC_BASE_URL`), Codex
  the OpenAI Responses API (a `model_provider`).
- The proxy finds the session's route from the token, calls the route's
  upstream (Anthropic, OpenAI, or an OpenAI-compatible endpoint such as
  OpenRouter, a local Ollama or vLLM) with the provider key from the node's
  secret store, and translates both ways where the harness's API and the
  upstream's differ: requests, streamed responses, tool calls, thinking,
  images, errors and token usage.
- Every upstream call is recorded in the session's event log (route,
  model, tokens, stop reason), so clients show the model in use and what
  it cost in tokens; `model.list` gives pickers the node's routes.
- Provider keys never reach a harness's environment, a log, the event log
  or the wire to a client; upstream URLs come only from the node owner's
  configuration.

A profile with no route is launched exactly as today: the proxy is only in
the path when the owner asks for it.

## Out of scope

- Harnesses other than Claude Code and Codex as proxy clients (Gemini CLI,
  OpenCode). Their configuration allows it (OpenCode takes an
  OpenAI-compatible provider; Gemini CLI a base URL); they need a Chat
  Completions or Gemini front, after M5 (open decision 3).
- Gemini's native API as an upstream (Gemini is reachable through
  OpenRouter, or Google's OpenAI-compatible endpoint, as a
  `chat-completions` provider).
- Subscription logins as upstreams (a Claude or ChatGPT account's OAuth).
  opencodex does this by imitating first-party clients' headers and
  identity; that is a terms-of-service risk Vessel does not take. A
  profile that uses its account's own login keeps doing so without the
  proxy.
- Failover, load balancing or "combos" across providers, key pools, and
  money cost (prices). Tokens are recorded; a price table can come later.
- Model routes synced across a fleet. The routes file is per node; its
  shape follows fleet MCP definitions (references to secrets by name), so
  moving it into `fleet.toml` later is mechanical (open decision 5).
- Embeddings, image generation, realtime audio, the Responses WebSocket
  transport (refused with 426, so Codex falls back to HTTP), remote
  compaction (`/v1/responses/compact`).
- Rewriting tool JSON schemas for strict providers (Gemini's keyword
  subset, xAI's root `oneOf`): schemas pass through as opaque JSON
  (`RawJson`) in M5; a provider that refuses them is a known limit.

## What exists to build on

- Profiles (ADR 0011, `vessel-harness::profile`): `profiles.toml` gives a
  harness environment changes and terminal arguments, applied in one place
  (`launch::with_profile`) for terminal and structured sessions alike (ACP
  adapters inherit the environment).
- The Codex harness already passes configuration as `-c key=value`
  overrides (its MCP servers, `harnesses/codex.rs`), with values in Codex's
  own environment, under names Codex keeps from the commands it runs.
- Secrets (M4, ADR 0012): `<data_dir>/secrets.toml`, owner-only, values
  set through stdin only, resolved by name in memory
  (`fleet::secrets`), never read back over the wire.
- Agent credentials (ADR 0013, `agents`): a 256-bit secret per session,
  kept only as its hash, refused once the session's end is visible,
  removed with the session. The proxy token copies this pattern.
- Session actors publish every event (`vessel-session::publish`); a
  session event has a protocol version and a stand-in for older clients
  (`SessionEvent::ProfileChanged`, protocol 5).
- The node's HTTP stack: `axum`/`hyper` for its listener, `rustls` with
  `ring` and the system roots (`vessel-relay::tls`) for outgoing TLS.
- Structured sessions pick a model through ACP config options
  (`vessel-agent::model`); a routed structured session keeps that choice
  to the route's model.

## Contract changes (protocol 7)

Shared with M6 and M7: rows go at the end of the method table with
`since 7`, each in its own commit, so the coordinator resolves merges.

- New types in `vessel-proto`:
  - `ModelRouteName`: a route's name in `models.toml` (the profile-name
    rules: lowercase letters, digits, `-`, `_`, `.`).
  - `ModelApi`: `anthropic_messages | openai_responses |
    openai_chat_completions`, the wire API of a provider or of a harness.
  - `TokenUsage { input, cache_read, cache_write, output, reasoning }`,
    each a `TokenCount` newtype; `input` excludes cached tokens and
    `reasoning` is part of `output` (the convention is fixed here, and each
    API is converted to it at its edge).
  - `ModelStop`: `end_turn | tool_use | max_tokens | stop_sequence |
    refusal | pause | error`.
- `ProfileInfo.model: Option<ModelRouteName>`: the route a profile names
  (additive and optional, so no version gate; older clients ignore it).
- `SessionInfo.model: Option<SessionModel { route, model }>`: the route a
  session was launched with and its upstream model id (additive, optional).
- `SessionEvent::ModelCall(ModelCall { route, model, api, usage, stop,
  failure: Option<ModelFailure>, duration_ms })`, `since 7`, one per
  upstream call: metadata only, never prompt or completion content, tool
  arguments or keys. `ModelFailure` is a closed set of error classes
  (`rate_limited`, `overloaded`, `upstream_error`, `refused_request`,
  `timeout`, `truncated`, `unauthorized`), not the upstream's message.
  Older clients get a `HarnessLog` stand-in line at the same position
  ("vessel: model call to <model>: <in> in, <out> out"), as
  `ProfileChanged` does.
- Method `model.list` (read) → `Vec<ModelRoute { name, provider:
  ProviderLabel, api: ModelApi, model, small_model? }>`: the routes this
  node offers, for pickers and `vessel models`. No base URL, header or
  secret name crosses the wire.
- No `model.usage` row (open decision 11): usage is in each session's log,
  and clients sum it.
- No new error code: a session naming a missing route fails to launch with
  `invalid_request` and a message naming the route, as an unknown profile
  does.

## Crates (ADR 0001)

One new library crate, and one new edge, recorded in ADR 0001 and
`docs/ARCHITECTURE.md` with the plan's acceptance:

```
vessel-llm      -> proto            (model APIs: wire types, SSE, translation,
                                     upstream calls; no node, no session)
vessel-node     -> llm              (the proxy: listener, tokens, routes,
                                     secrets, usage into sessions)
```

`vessel-llm` owns everything about model APIs and nothing about Vessel's
sessions: it can be tested alone against recorded streams and a fake
upstream. The node owns who may call it and with what route. The harness
crate owns how each harness is pointed at it. The name is short and says
what it is about; `vessel-model` would collide with "data model", and
`vessel-route` with relay routes (`RouteUrl`).

## Seams

### `vessel-llm` (new)

- `sse`: an incremental Server-Sent Events decoder and encoder: CRLF and
  LF, comments, multi-line `data:`, any chunk boundary, the last event
  dispatched at end of stream even without its blank line, a bound on a
  single event's size. Pure, no I/O.
- `wire::{anthropic, responses, chat}`: typed request, response and
  stream-event structs for the three APIs, decoded once at the edge
  (AGENTS.md section 2). Fields Vessel never reads (a tool's input schema,
  a tool call's arguments, provider-specific extras it forwards) are
  `RawJson`. Unknown content blocks and stream events decode to explicit
  `Unknown { kind }` variants: kept on a same-API passthrough, dropped and
  logged when translating.
- `ir`: the neutral request (system, messages of typed parts: text, image,
  thinking with an opaque signature, tool call, tool result; tools; tool
  choice; sampling; thinking effort; max tokens; stop sequences) and the
  neutral stream (`Started`, `TextDelta`, `ThinkingDelta`,
  `ThinkingSignature`, `ToolCallStart { id, name }`, `ToolArgsDelta`,
  `ToolCallEnd`, `Usage`, `Stop`, `Error`, `Keepalive`).
- `front::{anthropic, responses}`: request in (front wire to IR), stream
  out (IR events to the front's SSE, one terminal event), errors out (the
  front's error body and status).
- `upstream::{anthropic, responses, chat}`: request out (IR to the
  provider's wire), stream in (the provider's SSE to IR events), and the
  provider's quirks as typed capability flags on the provider's
  configuration (parallel tool calls, `max_tokens` field name, reasoning
  field), never guessed from the model's name.
- `passthrough`: front and upstream speak the same API (Anthropic to
  Anthropic, Responses to Responses): the request and response bytes go
  through untranslated; only the authorization changes, and the stream is
  read alongside for usage and the stop reason.
- `client`: the HTTP client (open decision 10): base URL from
  configuration only, fixed paths per API, no redirects, timeouts for
  headers and for silence, retries before the first byte only (open
  decision 12).
- `usage`: each API's usage to `TokenUsage`, once, at its edge.

### `vessel-node`: `models`

- `models.toml` (`<data_dir>/models.toml`, owner-only like
  `profiles.toml`, read at start and on `profile.reload`):

  ```toml
  [providers.openrouter]
  api = "openai-chat-completions"
  base_url = "https://openrouter.ai/api/v1"
  key = { secret = "OPENROUTER_API_KEY" }      # in secrets.toml
  parallel_tool_calls = true                   # capability flags, optional

  [providers.anthropic]
  api = "anthropic-messages"
  base_url = "https://api.anthropic.com"
  key = { secret = "ANTHROPIC_API_KEY" }

  [providers.local]
  api = "openai-chat-completions"
  base_url = "http://127.0.0.1:11434/v1"       # Ollama; no key

  [models.qwen-coder]
  provider = "openrouter"
  model = "qwen/qwen3-coder"
  small_model = "qwen/qwen3-coder-flash"       # optional: the harness's fast model
  max_output_tokens = 32000                    # optional default
  ```

- The listener: `127.0.0.1:0`, started with the node, its port handed to
  routed sessions at launch; paths `/v1/messages`,
  `/v1/messages/count_tokens`, `/v1/responses`, `/v1/models`; everything
  else a JSON 404; a WebSocket upgrade 426.
- Tokens: one per routed session, minted at launch with the agent
  credential's rules (open decision 7), kept as its hash, refused once the
  session's end is visible, gone with the session; a token maps to the
  session's route.
- Usage: each finished upstream call becomes a `ModelCall` event in the
  session's log through its handle.
- Limits: a request body cap, in-flight calls per session and per node.

### `vessel-harness`: pointing a harness at the proxy

A descriptor field per harness, `model_proxy`, owns the knowledge (the
harness rule, AGENTS.md section 3): given the proxy's base URL, the
session's token and the route's models, it returns the environment to set
and remove and the arguments to add.

- Claude Code (Anthropic Messages front): `ANTHROPIC_BASE_URL` (no `/v1`),
  `ANTHROPIC_AUTH_TOKEN` (the session token), `ANTHROPIC_API_KEY` removed
  (both set makes Claude Code warn; an inherited key would win over the
  route), `ANTHROPIC_MODEL` and the `ANTHROPIC_DEFAULT_*_MODEL` /
  `ANTHROPIC_SMALL_FAST_MODEL` variables pinned to the route's model and
  small model, so it asks for nothing else.
- Codex (Responses front): `-c model_provider=vessel` and `-c
  model_providers.vessel={name, base_url, wire_api="responses",
  env_key="VESSEL_MODEL_SECRET", requires_openai_auth=false}`, `-c
  model=<route model>`, the token in Codex's environment under a name
  Codex keeps from the commands it runs (open decision 8).
- Gemini CLI, OpenCode: `None` in M5; a routed profile for them is refused
  at reload with a message.

`Profile` gains `model: Option<ModelRouteName>` (`model = "qwen-coder"` in
`profiles.toml`); the node checks the route exists and fits the harness
when it reads both files.

### `vessel-session`

`SessionHandle::model_call(ModelCall)` hands a record to the actor, which
publishes it like any event (bounded channel; a full one drops the record
and logs, never blocks the proxy).

### `vessel-testkit`

- A fake upstream: a loopback server that answers each API's endpoint by
  replaying a recorded stream fixture, records what it was asked, and can
  fail on cue (a status, a drop mid-stream, silence).
- The fake harnesses gain one command: call the model endpoint their
  environment names, with their token, and report what came back, so the
  end-to-end tests check the real wiring without a real harness.

### `vessel-cli`

`vessel models` (the node's routes), `profiles` showing each profile's
route, and `vessel log` showing model calls.

## Data flow

```
harness (Claude Code)            node                                 upstream
  POST /v1/messages ─────────►  listener (127.0.0.1:P)
  Authorization: Bearer <tok>     token -> session -> route
                                  route -> provider (api, base_url, key)
                                  same API? ── yes ─► passthrough ───► Anthropic
                                       │ no                             │
                                  front::anthropic -> IR                │
                                  IR -> upstream::chat ───────────────► OpenRouter
                                  SSE in -> IR events -> front SSE out ◄┘
  ◄────────────────────────────── message_start .. message_stop
                                  usage -> SessionHandle::model_call
                                     -> event log (ModelCall) -> clients
```

The key is read from the secret store for each call (in memory), put in
the upstream request's authorization header, and nowhere else.

## Translation rules (what each pair must get right)

Learned largely from opencodex's edge cases (Grounding); Vessel's own
design where it differs.

- **Streams.** Exactly one terminal event per stream. An upstream that
  ends without its terminal marker (`message_stop`, `[DONE]` or a finish
  reason, `response.completed`) is a truncation and reaches the harness as
  an error, never as success. Keepalives on silence toward the harness
  (Anthropic `ping`, Responses heartbeat) and a separate watchdog on the
  upstream's own silence. An inline error inside a 200 stream is terminal.
- **Tool calls.** Ids pass through unchanged in both directions (`toolu_`
  and `call_` are both just strings); an id is minted only when a provider
  omits one, unique per session (not per stream, which opencodex gets
  wrong), and changed only where a provider restricts the charset or
  length, by a deterministic one-to-one mapping applied to the call and its
  result. Chat Completions fragments are assembled by index, then id;
  arguments that end empty become `{}`. History is repaired so every call
  is followed by its result (a missing one becomes an error result), and
  text after a `tool_use` moves before it for Anthropic.
- **Thinking** (open decision 9). Same API: untouched, except what
  decision 9 forbids: a passthrough to Anthropic drops every thinking block
  whose signature is an envelope or empty, a passthrough to Responses every
  reasoning item whose encrypted content is an envelope, and a request whose
  last tool-calling turn would then not start with thinking goes without
  `thinking` (Anthropic refuses it otherwise). Anthropic thinking sent to a
  non-Anthropic upstream is dropped (its signature means nothing there).
  Reasoning from another provider reaches the harness in its own opaque
  field (a thinking block's signature for Claude Code, a reasoning item's
  `encrypted_content` for Codex) as a Vessel envelope,
  `vsl1:<provider>:<kind>:<payload>`, so it can be replayed to the provider
  that made it and to no other; an envelope is never sent to any provider.
  `max_tokens` is sized above the thinking budget, and sampling parameters
  and forced tool choices are dropped when thinking is on, per the
  provider's flags.
- **Images and documents.** Base64 and URL images map between the APIs;
  an image inside a tool result becomes a text marker where the upstream
  takes none. Documents an upstream cannot take are refused with a clear
  error, not silently turned into a title (opencodex's choice).
- **System prompts.** One leading system or instructions block, including
  Claude Code's `role: "system"` messages; an Anthropic history ends on a
  user turn.
- **Cache control.** Same API: untouched. Across APIs: Anthropic markers
  are dropped toward OpenAI-style upstreams (which cache by prefix); cache
  tokens are reported back in the front's own fields.
- **Errors.** Each front gets its own error body and status. Toward Claude
  Code a transient upstream failure (429, 5xx, 529, a drop) is a 529
  `overloaded_error` with `Retry-After`, so Claude Code's own backoff
  takes over; toward Codex, the Responses error shape and codes it
  recognizes. Upstream error bodies are truncated and redacted before they
  reach the harness.
- **Usage and stop reasons** are mapped completely in both directions
  (`end_turn`/`stop`, `tool_use`/`tool_calls`, `max_tokens`/`length`,
  `refusal`/`content_filter`, `pause_turn`, `stop_sequence`); the
  Responses usage objects strict clients need are always present.
- **Codex specifics.** Request bodies may be compressed; unknown `/v1/*`
  paths get a JSON 404; `/v1/models` answers in the shape the caller asks
  for.

## Steps (Phase B)

Each step ends with the local gate and a push; protocol rows in their own
commits.

1. **B1 `vessel-llm` skeleton and SSE** (no open decision): the crate, the
   decoder and encoder, tests over recorded streams split at every byte
   boundary (the cheap fuzz: every split must decode to the same events),
   a bound on event size.
2. **B2 wire types** (no open decision): typed Anthropic Messages,
   Responses and Chat Completions requests and stream events, decoded from
   recorded fixtures under `crates/vessel-llm/tests/fixtures/` (trimmed,
   secrets removed), `Unknown` variants included.
3. **B3 IR and translators** (open decisions 2, 9): fronts and upstreams to
   and from the IR; the translation matrix as table-driven tests (each
   fixture through each pair), and the passthrough.
4. **B4 upstream client** (open decisions 10, 12): HTTP, TLS, timeouts,
   retries, error classification; the testkit fake upstream.
5. **B5 protocol 7 rows** (own commits): the types, `ProfileInfo.model`,
   `SessionInfo.model`, `SessionEvent::ModelCall` with its stand-in,
   `model.list`.
6. **B6 the node's proxy** (open decisions 5, 6, 7): `models.toml`, the
   listener, tokens, routing, secrets, limits, `ModelCall` events.
7. **B7 harness wiring** (open decision 8): Claude Code and Codex
   descriptors, `Profile.model`, launch; Codex's resume and profile moves
   checked with a routed profile. As built: `vessel_harness::ModelProxy`
   and each descriptor's `model_proxy` renderer, applied after the
   profile's own changes (a routed profile setting one of the route's
   variables is refused when `profiles.toml` is read). Claude Code also
   loses `CLAUDE_CODE_USE_BEDROCK`, `_VERTEX` and `_FOUNDRY` (they would
   win over the base URL) and pins `CLAUDE_CODE_SUBAGENT_MODEL`. Codex's
   provider is `vessel` in both modes: `codex-acp` takes the same `-c`
   overrides. The node mints the token by session id before the launch,
   binds it when the session starts, and gives a move a second token (as
   agent credentials, `agents`). A session keeps its route: a move to a
   profile of another route, or between a route and none, is refused
   (`unsupported`; see "Follow-ups"). A profile naming a route
   `models.toml` lacks is a warning at start and reload, and its sessions
   are refused with the route named.
8. **B8 CLI and end to end:** `vessel models`, routes in `profiles`, model
   calls in `log`; e2e: a routed session's fake harness calls the proxy,
   the fake upstream answers, the event lands in the log; the canary
   (below); the manual smoke script. As built: the fakes answer `model
   <text>` with one call where their environment and arguments point
   (`vessel_testkit::model`), so the e2e covers Claude Code in both modes
   and Codex structured through a Chat Completions route; the canary
   (`vessel-cli`'s `canary.rs`, its second test) plants the provider key,
   has a provider refuse while quoting it, and treats every session's
   model token as a canary too; `docs/smoke/model-route.sh` runs S1, and
   with `--fake` checks itself against `fake-provider`.
9. **B9 docs:** ADR 0015 (the proxy), ADR 0001 (the edge), PROTOCOL v7,
   ARCHITECTURE, README (a "Models" section), ROADMAP.

## Security

- **Who may call.** The listener binds `127.0.0.1` only. Every request
  needs a session's token (`Authorization: Bearer`, or `x-api-key` for the
  Anthropic front), compared by hash; a token dies with its session. A
  request with an `Origin` header is refused (harnesses send none; a
  browser page aimed at the port does), on top of the token.
- **What a token gives.** Model calls on that session's route, nothing
  else: no other route, no node method. It is in the harness's
  environment, so the commands the agent runs may see it (Claude Code
  passes its environment on; Codex keeps names ending in `_SECRET`,
  `_KEY`, `_TOKEN` from its commands): a leak gives that session's model
  access until the session ends. Limits per session and per node bound
  what a leaked token or a looping agent can spend in parallel.
- **Keys.** Provider keys live only in `secrets.toml` and are referred to
  by name in `models.toml`; they are read in memory per call, put in the
  upstream's authorization header, and never in a harness's environment,
  a log, an event, an error message or a reply. A routed session has
  `ANTHROPIC_API_KEY` (and Codex's key variable) removed, so no inherited
  key is used instead.
- **No SSRF.** Upstream base URLs come only from `models.toml`, which only
  the node's owner writes; the path is fixed per API; the client follows no
  redirects. Nothing in a request chooses where the proxy connects.
- **What is logged.** Route, model, status, token counts, latency, never a
  prompt, a completion, a header or a body. Upstream error bodies are
  truncated and passed through a redaction of key-shaped strings before
  the harness sees them.
- **The canary** (as M4's): a random provider key in `secrets.toml` must
  not appear in any harness's environment (the fakes report theirs), the
  node's tracing, the event log, CLI output or the bytes the proxy sends a
  harness; the fake upstream confirms it did receive it.

## Done criteria

- Claude Code and Codex sessions on a routed profile work through the
  proxy in terminal and structured mode, with streaming, tool calls and
  usage, against the fake upstream in CI on all three systems.
- The translation matrix (each front to each upstream, plus passthrough)
  passes over recorded streams; the SSE decoder survives every split.
- A routed session's log has a `ModelCall` per upstream call; `vessel log`
  and protocol 6 clients see the stand-in.
- The canary passes; the gate is green; docs match the code.

### Manual smokes

- **S1** (`docs/smoke/model-route.sh`, run by the operator with a real key):
  a throwaway node in a temporary home, the key read from standard input
  into `secrets.toml` (never an argument, never printed), a route to a
  non-Anthropic model on an OpenAI-compatible provider, a Claude Code
  profile naming it. A structured Claude Code session is asked to create
  and then edit a file (tool calls, streaming, a second turn); the script
  checks the file, prints the session's model calls (tokens, never
  content), and greps the node's log and the session's environment report
  for the key.
- **S2** (optional, with an Anthropic key): Codex through an Anthropic
  route, one turn with a tool call.

## Follow-ups

- **Switching a live session's model route** (a move between profiles of
  different routes, or between a route and none). Refused in milestone 5:
  the session's record names its route, and Codex files each thread under
  its model provider (`vessel` for every routed Codex session, its own
  `openai` otherwise), so a thread resumed under another provider is not
  found. Claude Code does not file conversations by provider, so it could
  come first: only the record (`SessionInfo.model`, a new event saying the
  route changed) and the token's route would need to follow. The
  passthrough's stripping of foreign thinking (decision 9) stays either
  way, since an operator may point a route at another API under a live
  session by editing `models.toml`.

## Decisions (coordinator, 2026-11-07)

All twelve recommendations below are accepted; ADR 0015 records them, and
the crate `vessel-llm` with the edge `vessel-node -> llm` (ADR 0001).
Conditions added:

- Routes (5): `models.toml` is owner-only, and so is anything derived
  from it.
- Model names (6): pin every model slot Claude Code reads (the main
  model, the default opus, sonnet and haiku models, the small and fast
  model), so subagents and background calls land on the route's `model`
  or `small_model`; a refusal names the model asked for and those the
  route allows.
- Token (7): dies with the session, compared in constant time, stored
  only as a hash; a request with no token or a wrong one gets 401 and
  logs no request content.
- Thinking (9): a `vsl1:`-signed thinking block never reaches Anthropic,
  also after the session moves to an Anthropic route or a profile on one:
  every thinking block whose signature is not Anthropic's is removed
  before any Anthropic upstream call. Tested for the move and for mixed
  history.
- HTTP client (10): connect, first-byte and between-chunks timeouts, and a
  bound on non-streaming response bodies.
- Usage (11): the event carries route, upstream model, API, token counts,
  stop reason, duration and an error class; nothing else.
- The proxy holds provider keys: once Phase B is feature-complete it gets
  the independent security review (token handling, SSRF, key exposure,
  smuggling between sessions, resource limits, log contents).
- Protocol rows `since 7` in their own commits at the table's end; M6 adds
  `lsp.*` and M7 a `ts` feature in `vessel-proto` and
  `NodeClient::call_any`, to merge against.

## Open decisions (as drafted, kept for the record)

1. **Where the proxy runs.**
   (a) In the node, one listener for all routed sessions; (b) a separate
   process per node (`vesseld proxy`); (c) one per session.
   **Recommended: (a).** The node already has the sessions, the secrets
   and the event log; a token maps straight to a session's route and its
   usage straight into its log. (b) adds a process to supervise and a
   second secret channel; (c) a port per session for nothing.
2. **How translation is organised.**
   (a) A neutral IR, one translator per API in and out, plus a byte
   passthrough when both sides speak the same API; (b) one API as the hub,
   as opencodex does with Responses (Anthropic to Anthropic goes through
   Responses twice and loses thinking, `is_error`, cache markers and
   documents); (c) a translator per pair (N×M).
   **Recommended: (a).** N+M translators, no double translation, and the
   common case (Claude Code to Anthropic, Codex to OpenAI) is exact.
3. **Front APIs in M5.**
   (a) Anthropic Messages and OpenAI Responses (Claude Code, Codex);
   (b) also Chat Completions now (OpenCode, others).
   **Recommended: (a)**; Chat Completions as a front comes with the first
   harness that needs it (its translator to and from the IR exists
   already, as an upstream).
4. **Upstreams in M5.**
   (a) Anthropic, OpenAI Responses, OpenAI-compatible Chat Completions
   (OpenRouter, Ollama, vLLM, LM Studio); (b) also Gemini's native API.
   **Recommended: (a)**; Gemini through OpenRouter or Google's
   OpenAI-compatible endpoint. Native Gemini needs schema rewriting and
   its own id rules (opencodex's largest adapter).
5. **Where routes are defined.**
   (a) A node-local `models.toml` (providers and named models, keys by
   secret name), profiles naming a model; (b) routes in the home's
   `fleet.toml`, synced like MCP servers, keys per node.
   **Recommended: (a) now**, in the same shape as fleet MCP definitions so
   (b) is a later move of a table. A route's provider and base URL decide
   where prompts go, which the owner should choose per machine first.
6. **Which model names the harness asks for.**
   (a) The route's upstream model ids, pinned through the harness's
   environment or overrides; the proxy refuses any other name for that
   session; (b) Vessel aliases (`vessel-<route>`) mapped by the proxy;
   (c) the proxy ignores the requested name and always uses the route's.
   **Recommended: (a).** Nothing to rewrite on a passthrough (the body
   goes through as is), the harness shows the real model, and a request
   for anything else is an error, not a silent substitution. (c) hides
   mistakes; (b) adds a naming layer. Where a harness rejects a name's
   shape (Codex allows one `/`), its module maps it.
7. **The session's token.**
   (a) A token of its own per routed session; (b) the agent credential.
   **Recommended: (a).** The agent credential is kept out of the
   harness's own environment on purpose (ADR 0013); the model token has to
   be in it. Different exposure, different secret.
8. **How Codex reaches the proxy.**
   (a) A custom provider through `-c` (`model_provider = "vessel"`,
   `env_key` naming the token), Codex's documented way; (b) the root
   `openai_base_url` key with the token as `OPENAI_API_KEY`, as opencodex
   does, which keeps the `openai` provider (thread history) but sends a
   ChatGPT login's token instead when the account has one.
   **Recommended: (a)**, verified in B7 against session resume and profile
   moves (Codex files threads under the provider). Structured Codex goes
   through `codex-acp`, which embeds Codex: if it takes no overrides, a
   routed structured Codex session is refused with a clear message until
   it does, and terminal Codex works.
9. **Thinking across APIs.**
   (a) Pass through on the same API; across, drop Anthropic thinking
   toward others, and carry other providers' reasoning to Claude Code as
   thinking with a Vessel envelope signature, never sent to Anthropic;
   (b) drop thinking whenever the APIs differ; (c) fake signatures, as
   opencodex does (which a later switch to Anthropic would forward).
   **Recommended: (a)**; (b) as the fallback for a provider whose
   reasoning cannot be replayed.
10. **The HTTP client.**
    (a) `reqwest` with default features off, `rustls` with `ring`, the
    system roots, streaming bodies; (b) `hyper-util`'s client with
    `hyper-rustls`.
    **Recommended: (a)**, a new workspace dependency recorded in the plan
    and ADR 0015: pooling, timeouts and streaming without writing a client;
    redirects off and the system proxy variables ignored (a node's
    outgoing calls should not depend on the harness's proxy settings).
11. **Usage on the wire.**
    (a) A `ModelCall` event per upstream call in the session's log, no
    `model.usage` row; (b) also an aggregate `model.usage` (per session or
    per route).
    **Recommended: (a).** Every client that wants totals has the log; an
    aggregate row is worth it when a UI shows usage across sessions (Phase
    5), and can be added then.
12. **Retries.**
    (a) Before the first byte reaches the harness only: connection resets
    on a fresh connection, and 429, 5xx and 529 honouring `Retry-After`,
    at most 3 attempts and 30 s, none after a slow attempt; then let the
    harness's own backoff work (529 to Claude Code); (b) no retries in the
    proxy.
    **Recommended: (a).** A dropped pooled connection is the proxy's
    problem, not the harness's; anything longer belongs to the harness,
    which shows it to the person.

## Grounding

Read 2026-11-07, read-only. Ideas and edge cases only; no code is copied.

### opencodex (MIT, commit 34688f6, v2.7.41)

A TypeScript (Bun) proxy that lets Claude Code, Codex and others use any
provider. What Vessel takes from it is its catalogue of edge cases (the
"Translation rules" above credit it): SSE truncation and the single
terminal event; parallel Chat Completions tool fragments; empty
arguments; history repair around tool rounds; Anthropic `max_tokens`
against the thinking budget, the budget and adaptive thinking shapes, and
sampling parameters with thinking; cumulative Anthropic usage
(`message_delta` replaces, not adds); stop reasons; 529 with
`Retry-After` toward Claude Code; retries only before the first byte;
keepalives and a stall watchdog; Claude Code's `?beta=true`, `[1m]`
suffixes, `role: "system"` messages and `count_tokens`; Codex's compressed
bodies, 426 for WebSockets and JSON 404s; `ANTHROPIC_BASE_URL` without
`/v1` and exactly one of the two auth variables.

What Vessel does differently, and why: a neutral IR with a same-API
passthrough instead of Responses as the hub (its double translation loses
thinking, `is_error`, cache markers and documents on Anthropic routes); no
fake thinking signatures; a token per session on loopback (opencodex
admits any local process); no imitation of first-party clients for
subscription logins; error classification on typed codes rather than
message text where the API gives them; minted ids unique per session.

### Paseo (commit e9f897a, ideas only)

No model proxy; provider keys go into the harness's environment from its
configuration, and a client can read them back (Vessel's secrets never
leave the node). Useful: Codex is pointed at a namespaced custom provider
(`wire_api = "responses"`, `requires_openai_auth = false`), and Codex files
threads under the provider, so resume must name the same one; inherited
`CLAUDECODE` variables break nested launches; Anthropic-only server tools
(web search) fail on other endpoints; per-turn usage (input, cached,
output, context used) as the UI wants it.

### Zed (commit 654efae, ideas only)

Its own agent calls providers directly through a neutral request and
event model (`Thinking { signature }`, `RedactedThinking`, `ToolUse {
is_input_complete }`, `UsageUpdate`, `Stop`), which is the shape of
Vessel's IR. Useful: explicit capability flags for OpenAI-compatible
endpoints instead of guessing; tool-call fragments matched by id and
index, failing on ambiguity rather than corrupting arguments; usage
updates treated as snapshots; one error classification with
`Retry-After` and a bounded backoff; keys keyed by the URL they belong to;
custom headers that may not override `Authorization`. External ACP agents
get their model through an ACP config option, as Vessel's structured
sessions do.
