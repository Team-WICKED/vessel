# ADR 0017: The web UI is a local bridge (`vessel ui`)

Status: accepted, 2026-11-07 (the coordinator, on milestone 7's plan,
`docs/plans/backend-m7.md`).

## Context

Roadmap Phase 5 starts with a web UI: everything the CLI does with the
fleet, seen and done in a browser. Nodes are reached in four ways:

- direct websocket with a token;
- SSH;
- a relay, end to end encrypted;
- a paired secure channel.

The credentials for all four live on the operator's machine:

- the book;
- token files;
- device credentials;
- SSH configuration.

A browser cannot speak Noise or SSH. Anything that hands it a node's
credentials makes every script on the page as powerful as the CLI.

## Options

1. **The node serves the UI** (`vesseld --web`). Each node would need a
   login for browsers as strong as pairing, and would expose the UI on
   its network. Every node would be a separate site, with no fleet view.
2. **A hosted UI that talks to nodes directly.** It is impossible without
   giving browsers credentials, and it puts a third party in the trust
   base.
3. **A local bridge.** `vessel ui` serves a single-page app on loopback and
   reaches every node in the book through `vessel-client`, as the CLI
   does. The browser authenticates to the bridge only.

## Decision

Option 3.

**Placement.**
- A new crate, `vessel-web`, owns:
  - the HTTP server;
  - the per-launch token;
  - the Host and Origin checks;
  - the security headers;
  - the embedded files;
  - the bridge: one shared connection per node, calls forwarded, session
    streams followed with resume.
- `vessel-cli` adds the `vessel ui` command. It implements `vessel-web`'s
  `NodeSource` seam with the book and its connectors, so the knowledge of
  tokens, SSH and credentials stays in one place.

**The bridge speaks the protocol's own types to the browser.**
- `Call`, `Reply`, `Outcome` and `EventRecord`, in a thin envelope.
- The browser's JSON is decoded into the typed `Call` at the bridge.
- `NodeClient::call_any` forwards it with the protocol-version gate of
  every typed call. The node enforces scopes as it does for the CLI.
- Connection-scoped methods (`session.subscribe`, `session.unsubscribe`)
  are refused; streams have frames of their own. So are the fleet's
  node-to-node methods (`fleet.report`, `fleet.relay`, `fleet.deliver`,
  `fleet.grant`, `fleet.callback`, `fleet.peers`), which only nodes and
  relays call. The list is an exhaustive `match` on `Method` in the
  bridge (`forwardable`): a new method does not compile until it is
  placed on one side.
- Each node has one connection, kept by a supervisor task that dials
  through the `NodeSource`'s connector, redials with the CLI's backoff
  after a loss, and pushes its state to every tab. Calls take the current
  connection; a node that is down fails them at once. Streams run
  `follow_resuming` through a connector that waits for the supervisor's
  next connection instead of dialling its own, so a stream resumes
  gap-free and an SSH node still has one `ssh` process.
- The bridge's frames are generated into TypeScript like the protocol's
  (`vessel-web`'s `ts` feature, `web/src/wire/bridge.ts`, importing from
  `proto.ts`).

**Authentication (security review amendment, 2026-11-07).**
- Each launch uses an independent random 128-bit `vessel-<id>.localhost`
  hostname, even if its TCP port is reused. A previously cached malicious
  **index** can ignore a real server's no-store header entirely; an existing
  service worker can intercept even a cache-busting path. A fresh origin
  closes both routes. SRI alone is insufficient for this initial-HTML case.
  The server allows exactly this launch hostname, plus the literal loopback
  names for diagnostics; it does not accept arbitrary `*.localhost` hosts.
  Clients must open the printed hostname, not substitute a literal address.
- `.localhost` is reserved for loopback by [RFC 6761, section 6.3](https://www.rfc-editor.org/rfc/rfc6761.html#section-6.3).
  Chrome/Edge/Firefox handle these names; Safari and some non-browser DNS
  clients do not ([browser compatibility](https://learn.microsoft.com/en-us/aspnet/core/test/localhost-tld#browser-compatibility)).
  The UI requires Chrome, Edge or Firefox on all three OSes; the browser
  tests exercise Chromium, and raw-socket Rust tests use literal loopback
  addresses. **Safari is not supported, and has no fallback** (decided
  2026-11-07): a literal-loopback opt-in would give up the defence against
  a port squatter's cached index or service worker on `127.0.0.1:<port>`,
  which SRI and ETags cannot cover for the initial HTML. The roadmap keeps
  a follow-up to revisit it (a signed bootstrap page, or Safari's own
  `localhost` port isolation).
- Each launch has independent random 256-bit bootstrap and session secrets,
  and a public random launch id. `LaunchToken` names the bootstrap code.
- The URL carries only `#bootstrap=`. The page removes it immediately, but
  browser history databases may retain it. The server consumes it atomically
  on the first valid proof, including across simultaneous connections. The
  independent session key goes into the tab's `sessionStorage`, never
  `localStorage`; a reload works, a fresh context cannot reuse the URL. A
  lost bootstrap welcome requires a new UI launch rather than making the
  one-time code reusable. Closing the UI invalidates both secrets.
- Every websocket starts with `challenge { launch, nonce }` from the server.
  The browser sends `hello { mode, nonce, proof }`. HMAC-SHA256 uses the
  secret's ASCII hex as its key and the exact UTF-8 transcript
  `vessel-ui-v1\nclient\n<launch>\n<server nonce>\n<client nonce>\n`.
  Nonces and launch ids are canonical 64-character lowercase hex.
- The server's `welcome` proves the same key over
  `vessel-ui-v1\nserver\n<launch>\n<server nonce>\n<client nonce>\n<version>\n<session or empty>`.
  Only bootstrap welcomes include the independent session key. The browser
  opens no calls or subscriptions until it verifies the welcome, and stops
  on a bad proof, a changed launch or a 4401 close: those are refusals, and
  only they make the tab forget its session key. Five consecutive failed
  connections (closes, or a handshake that does not finish in 10 s) stop
  the redialling too, since a restarted server rejects the old random Host
  before a websocket exists and browsers hide that HTTP response; but the
  tab keeps its key and says it is disconnected, with a Retry button, and
  a reload also resumes. A restarted UI still fails the Host check or the
  proof, so keeping the key gives a stale tab nothing. (Before this, five
  transient closes deleted a valid key and no reload could recover it:
  M7 verification, finding 1.) Fresh random nonces and role separation
  prevent captured hellos/welcomes from authenticating another connection.
  No reusable key is sent by the browser during reconnection.
- `/api/status` accepts the issued session key as a bearer, never the URL
  code. The UI does not call this HTTP endpoint; it uses only the mutually
  authenticated websocket. Rust compares proofs/keys in constant time.
- **The URL is written to disk on Unix** for the browser handoff, because
  putting it in an opener/browser command line exposes it to process lists.
  The directory is created as 0700 and the file as 0600, atomically, under
  `XDG_RUNTIME_DIR` when set or the system temporary directory. Signal
  handlers are registered before creating the file; SIGINT/SIGTERM/SIGHUP
  drop it. SIGKILL/crashes cannot guarantee deletion. Windows fails closed
  on automatic opening: configurable temporary directories have no assured
  private ACL; the printed URL works without writing a secret handoff file.
- **No cookie.** Cookies ignore ports and would reach other local servers.
  Session storage supports reloads without persisting credentials across
  independent browsing sessions. It does not protect against malicious
  first-load code or a same-origin page opened in that same tab later.

**The server's defences.**
- It binds loopback only. `Host` must be one of the bound address's names
  with its port (DNS rebinding). Websocket upgrades require the matching
  `Origin`; HTTP API requests reject a foreign Origin, and allow a missing
  one for CLI callers with a bearer. No CORS headers are sent.
- Responses carry CSP, `nosniff`, no-referrer, frame denial, COOP
  `same-origin` and CORP `same-origin`. `index.html` is `no-store`; all other
  assets are `no-cache` with content SHA-256 ETags, not immutable.
- The build emits SHA-384 SRI on every script and stylesheet referenced by
  the index. It rejects JS/CSS chunks outside that integrity graph: a future
  code-splitting change must extend the integrity policy first. This matters
  because a port squatter could previously have seeded the cache with an
  attacker-chosen immutable response; revalidation headers alone cannot
  retroactively evict it. SRI may block loading until that cache is cleared,
  but it prevents executing the poisoned asset. Fresh origins additionally
  protect the initial HTML and service-worker boundary. Malicious code already
  executing in a live launch's origin is still outside this protocol's protection.
- Admission is counted from TCP accept: 128 sockets overall, 32 not yet
  authenticated. Counted I/O preserves both permits through HTTP upgrades;
  authentication releases only the pending permit. HTTP headers have a 5 s
  deadline and a 16 KiB buffer bound. Ordinary HTTP replies close the socket.
  Websocket upgrade and hello each have a 5 s deadline, with a 4 KiB frame
  **and message** bound before hello. After mutual proof the parser permits
  4 MiB, preserving large prompts. Pre-welcome read-ahead is discarded at
  that parser boundary: clients must wait for welcome before sending calls.
  Shutdown cancels upgraded sockets as well as HTTP tasks and node links.
- The implementation reuses workspace `hyper`, `hyper-util`, `tower`,
  `tokio-tungstenite`, `hmac` and `sha2`. Axum's socket wrapper cannot change
  parser limits between authentication phases; the raw upgrade preserves
  the small pre-auth limit without shrinking legitimate authenticated calls.
- Registry entries are admitted only after reading the book, and removed
  entries cancel their supervisors and streams. Listing or using the book
  refreshes the registry; invalid names do not consume its 256-node budget.
- Ordinary calls/follows/retries dial only book entries or this machine.
  **`Op::Pair` is the deliberate exception:** the user supplies a pairing
  link, inspects its fingerprint, then pairs; the connector dials the address
  from that link before the new entry exists in the book.

**Front end.**
- React 19 with TanStack Router (typed routes), Vite, TypeScript strict,
  ESLint `strict-type-checked` with `no-explicit-any`.
- Small stores fed by the bridge's pushes, without a query cache.
- Markdown with raw HTML disabled. Only absolute `http://` and `https://`
  URLs become anchors, always with `target="_blank"` and
  `rel="noopener noreferrer"`; relative and other-scheme links are text.
  A terminal's OSC 8 hyperlinks follow the same policy (`data/links.ts`):
  http(s) only, after a confirm that shows the URL, in a new tab opened
  with `noopener,noreferrer`; the UI tab never navigates.
- xterm output never becomes session input. The narrow `terminal-input`
  adapter uses pinned xterm 6.0's internal synchronous `onUserInput` signal
  to distinguish real keys/paste/IME from parser-generated replies, including
  replay and live output. Public `onData` alone merges both. A query denylist
  would miss future escape sequences; disabling all input during async
  replay would lose real typing. This private API dependency must pass the
  real-terminal browser regression before xterm upgrades.
- Pairing links/QR are absent from the DOM until explicitly shown. Copying,
  blur/visibility change and a 30 s deadline hide them. CI uploads only
  deliberate screenshots of the hidden state; no traces, videos, HTML
  reports or automatic failure screenshots, and tests never log links.
- npm.

**Model routing (milestone 5, merged 2026-11-07).**
- `model.list` passes the bridge like any read call, and each node's
  routes load with its harnesses and profiles; a node older than
  protocol 7 shows none rather than failing its other reads.
- A route reaches a session only through a profile naming it, since
  `session.create` has no route field and a routed harness is pinned to
  its route's models. So New session's Model field lists the harness
  default and the node's routes grouped by provider (DESIGN.md §6.2), and
  picking a route picks a profile of that harness on it; picking a routed
  profile shows its route. A route no profile of the harness names is
  listed, disabled, as `vessel models` lists it. The move-profile dialog
  offers only profiles on the session's own route, which is all the node
  accepts.
- A `model_call` record is a timeline row (route, model, tokens, how it
  ended, how long) and is summed in the inspector; a terminal session's
  calls are summed in the terminal's frame. Words and numbers follow
  `vessel log`. The record holds no prompt, completion or key, and the
  page adds none.

**Wire types.**
- `ts-rs` derives, behind an optional `ts` feature of `vessel-proto`
  (no change to the wire), generate TypeScript for every wire type.
- The output is committed, and CI regenerates it and fails on any
  difference.
- It holds no `any`. Exceptions mapped to `unknown` are listed under
  "Consequences".

**The bundle.**
- `vessel-web`'s `build.rs` embeds `web/dist` when it exists, and a
  placeholder page that says how to build the UI when it does not.
- CI's web job and the release build the bundle first and set
  `VESSEL_WEB_REQUIRE_BUNDLE=1`, which makes a missing bundle an error.
- Nothing built is committed.
- Releases ship `vessel`, with the UI embedded, beside `vesseld`.

**Fonts** (the operator's rule for every site of ours):
- Pretendard comes first in every `font-family`, ahead of any generic
  family. Gulim, Dotum and Batang are never used.
- Terminal and code: Consolas, then a real monospace font, then
  Pretendard, then `monospace`.
- Pretendard ships in the bundle, and a check fails the build on any
  violation.

## Why

- **The trust base stays where it is.** Credentials never leave the
  process that already holds them for the CLI. The browser is one more
  client of a loopback server, and the server accepts it only with a
  secret it was handed on the operator's own terminal.
- **One fleet view.** Every way of reaching a node (direct, SSH, relay,
  paired) works on day one, because the bridge reuses the client.
- **The protocol is the API.** Generated types keep the browser honest
  with the Rust contract. There is no second set of view-model endpoints
  to keep in step with it.
- **Rust-only builds stay sane.** Only the web job and the release need
  Node.

## Consequences

- `vessel-cli` depends on `vessel-web`, and `vessel-web` on
  `vessel-client` and `vessel-proto` (ADR 0001 and `ARCHITECTURE.md`
  updated).
- A `cargo build` without a web bundle gives a placeholder page in place
  of the UI. That is fine for the Rust gate and for development of other
  crates.
- The UI cannot be reached from another machine. That is a follow-up
  (`vesseld --web`, or an SSH forward), not part of this decision.
- Types that are `unknown` in the generated TypeScript:
  - `RawJson`, the protocol's opaque passthrough (AGENTS.md section 2):
    never inspected by Vessel, so never typed.
- Adding a protocol method needs one line in the bridge: whether a
  browser may call it, in an exhaustive `match` on `Method`, so the
  question cannot be skipped. `Call` and the generated types grow on
  their own.
