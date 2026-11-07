# ADR 0015: A model routing proxy in the node

Status: accepted, 2026-11-07, with the coordinator's decisions on
`docs/plans/backend-m5.md` (milestone 5, ROADMAP Phase 3); being built.

## Context

Every harness should be able to use every model (VISION section 4, the
owner's fourth goal in `ORIGIN.ko.md`). Each harness speaks one vendor's
API to one vendor's endpoint: Claude Code the Anthropic Messages API,
Codex the `OpenAI` Responses API. Both let the endpoint be changed (Claude
Code with `ANTHROPIC_BASE_URL`, Codex with a configured model provider),
which is how opencodex points them at a translating proxy. Vessel's node
already launches harnesses with a profile's environment (ADR 0011), holds
secrets that never leave it (ADR 0012), gives every session a credential
of its own (ADR 0013), and records everything a session does in its event
log.

The constraints: provider keys must never reach a harness's environment,
a log, the event log or a client; where prompts are sent must be the
node owner's choice alone; a session must be able to use only its own
route; and the code must keep the workspace's rules (typed wire data, no
untyped JSON trees, one crate per job).

## Options considered

- Where it runs: inside the node; a separate process per node; one per
  session.
- How translation is organised: one API as the hub (opencodex translates
  everything through Responses, so an Anthropic-to-Anthropic call goes
  through Responses twice and loses thinking, tool error flags, cache
  markers and documents); a translator per pair of APIs; a neutral
  intermediate form with one translator in and out per API, and a byte
  passthrough when both sides speak the same API.
- How a harness is pointed at it: aliases the proxy maps to models; the
  proxy ignoring the requested model; the harness pinned to the route's
  real model ids.
- How a session authenticates: its agent credential; a token of its own;
  none on loopback (opencodex).

## Decision

1. The proxy runs **inside the node**: one listener on `127.0.0.1`, an
   ephemeral port handed to routed sessions at launch.
2. **A neutral intermediate form**, with fronts (the APIs harnesses speak:
   Anthropic Messages and `OpenAI` Responses in milestone 5) and upstreams
   (Anthropic, `OpenAI` Responses, `OpenAI`-compatible Chat Completions:
   `OpenRouter`, Ollama, vLLM; Gemini through compatible endpoints)
   translating into and out of it, and a **byte passthrough** when front
   and upstream speak the same API (only the authorization changes; the
   stream is read alongside for usage).
3. A new crate, **`vessel-llm`** (`-> proto`), owns the model APIs: Server-Sent
   Events, typed wire types, the intermediate form, translation and the
   upstream client. **`vessel-node -> llm`** is the proxy: tokens, routes,
   secrets, limits, usage. How each harness is pointed at it stays in
   `vessel-harness` (ADR 0001's amendment).
4. **Routes** are defined per node in `<data_dir>/models.toml`
   (owner-only, as is anything derived from it; checked on Unix, while on
   Windows the data directory's inherited ACL is relied on, as for the
   store's other files): providers (API, base URL,
   key as `{ secret = "NAME" }` from `secrets.toml`, capability flags) and
   named models (provider, model, small model). A profile names one with
   `model = "<route>"`. Upstream URLs come only from this file; the client
   follows no redirects and ignores the system's proxy settings.
5. The harness is **pinned to the route's real upstream model ids**:
   Claude Code through every model variable it reads (the main model, the
   default opus, sonnet and haiku models, the small and fast model), so
   its subagents and background calls land on the route's model or small
   model; Codex through `-c model`. The proxy refuses any other name,
   saying which name was asked for and which the route allows.
6. Each routed session gets **its own model token**: minted at launch,
   kept only as its hash, compared in constant time, refused once the
   session's end is visible and gone with it. A request without a token or
   with a wrong one gets 401 and logs no request content. The agent
   credential is not reused: it is kept out of the harness's own
   environment, and this token must be in it.
7. Codex reaches the proxy through a **custom model provider given with
   `-c`**, its token named by `env_key`.
8. **Thinking** passes through untouched on the same API. Across APIs,
   other providers' reasoning reaches Claude Code as thinking blocks whose
   signature is a Vessel envelope (`vsl1:`); before any call to an
   Anthropic upstream, every thinking block whose signature is not
   Anthropic's, envelopes included, is removed, also after a session
   moves to an Anthropic route or a profile on one.
9. The upstream client is **`reqwest`** (default features off, `rustls`
   with `ring`, the system's roots), with connect, first-byte and
   between-chunks timeouts and a bound on non-streaming bodies. It replaces
   writing an HTTP/1.1 and HTTP/2 client with pooling and streaming on
   `hyper-util`; its TLS is the stack the workspace already uses: reqwest's
   `rustls-no-provider` feature takes the config Vessel builds (`ring`,
   the roots from `rustls-native-certs`, ALPN for HTTP/2) whole, so no
   process-wide crypto provider is installed. Redirects are off and no
   proxy is used.
10. Every upstream call becomes a **`ModelCall` event** in the session's
    log (protocol 7): route, upstream model, API, token counts, stop
    reason, duration and an error class; never prompt or completion
    content, tool arguments or keys. `model.list` offers the routes; there
    is no aggregate usage row.
11. **Retries** happen only before the first byte reaches the harness;
    after that Claude Code gets a 529 `overloaded_error` with
    `Retry-After`, so its own backoff takes over.

## Why

The node already has what the proxy needs (sessions, secrets, the event
log), so a token maps straight to a route and usage straight into the
session that made it, with no second process or secret channel. The
neutral form needs N+M translators instead of N×M, keeps the common case
(each harness to its own vendor) exact through the passthrough, and has
the shape Zed's own provider layer uses. Pinning real model ids means
nothing is rewritten on a passthrough and a mismatch is an error, not a
silent substitution. A token per session, on top of loopback, keeps one
session (or any local process) from using another's route.

## Consequences

- A routed session's harness, and the commands it runs, can read its
  model token: a leak gives that session's model access until the session
  ends. Limits per session and per node bound what a leaked token or a
  looping agent can spend in parallel.
- A routed session depends on the node: the proxy's port changes when the
  node restarts, which interrupts sessions anyway.
- Some information does not survive translation between APIs (Anthropic
  cache markers toward `OpenAI`-style upstreams; thinking toward a
  provider that cannot replay it); same-API routes lose nothing.
- A native signature or encrypted reasoning names no provider, so two
  providers with the same API (Anthropic and a proxy in front of it)
  cannot tell each other's apart: after a move between them, the second
  refuses the first's, as it would without Vessel. Vessel's envelopes do
  name their provider.
- A session keeps the route it was launched with. Its token is minted by
  session id before the launch (it goes into the harness's environment)
  and opens nothing until the session has started; a profile move gets a
  second token for the agent it starts, and keeps whichever agent runs
  afterwards. A move between profiles of different routes, or between a
  route and none, is refused (`unsupported`): the session's record names
  its route, and Codex files a thread under its provider, so resuming it
  under another would not find it. Switching a live session's route is a
  follow-up. Decision 8's stripping does not rely on this: an operator may
  still edit `models.toml` and point a route at another API under a live
  session.
- Tool schemas pass through unchanged in milestone 5; a provider stricter
  than JSON Schema (Gemini's native API) needs rewriting later.
- Subscription logins are not upstreams: a profile using its account's
  own login keeps doing so without the proxy.
- The proxy is a new boundary holding provider keys; it gets an
  independent security review before the milestone is done (tokens,
  SSRF, key exposure, isolation between sessions, resource limits, log
  contents).
- The `vsl1:` envelope is a transport hint, not authenticated
  provenance: a harness can write any envelope, naming any provider.
  Vessel uses it only to decide where a reasoning block may be replayed,
  never sends one to a provider as that provider's own opaque field (not
  even unwrapped from another), and leaves authenticity to the provider,
  which checks its own signatures and encrypted content and refuses a
  forged one.

Amended after the security review (2026-11-07), which found three P1
and four P2 problems in the first build; each now has its regression
test (the plan's "Security", "After the review"):

- The proxy checks a request's headers (`Origin`, `Content-Encoding`,
  the token, `Host`, route, limits) before it reads any body, reads the
  body under a cap and a deadline, bounds its connections and header
  reads, and accepts only `127.0.0.1:<port>` and `localhost:<port>` as
  `Host`.
- Objects the proxy decides by (the request, messages, blocks, items)
  may not name a member twice, and a block it cannot read refuses the
  request.
- A provider's key is removed from everything the proxy writes from the
  provider, whatever the status: error bodies whole before they are
  shortened, inline errors in a 200 stream, failed responses.
- Every answer is bounded (32 MiB) as it is read, translated or not, and
  every write toward the harness has a deadline.
- A routed harness's own arguments may not choose another model,
  provider or configuration profile, and the route's arguments come
  last. What the harness reads from its own home (an account's settings,
  an administrator's managed configuration) is the account's.

Amended after the verification round on those fixes (2026-11-07):

- Nothing from the provider is classified before it is checked: every
  event's name and data, the data decoded as JSON with member names, and
  every error body that is JSON. An event holding the key is not
  written; the call ends there with a classified failure in the front's
  form. An error body that is not JSON shows only its status. Redaction
  covers the key as written and in any JSON spelling, not other
  encodings: it is against an echo, not against a provider set on
  sending its key back.
- A refusal closes its connection. A connection that has not shown a
  session's token is the first closed to make room when the proxy is
  full; a head cut off by its deadline is answered 408.
- A same-API stream that ends before its answer did is reported to the
  harness as an error and recorded as truncated.

Amended after the third round (2026-11-07):

- A provider gets no key or a distinctive one (at least 20 bytes, of at
  least ten different ones); a call with any other key is refused before
  it goes up, since the proxy looks for the key in every answer. An
  answer withheld for quoting it is an invalid request, not a transient
  failure.
- Text with a backslash that `serde_json` cannot decode is searched as
  text, unescaped leniently: redaction fails closed.
- A session proves at most 32 of the proxy's connections; past that its
  connections may be closed to make room like a tokenless one.
