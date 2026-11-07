# Milestone 7: the web UI

Status: drafted 2026-11-07 by Track M7 (web); the coordinator settled
every open decision the same day as recommended ("Decisions" below), and
ADR 0017 records them. Implementation on `backend/m7-web`. Builds on
`main` with milestones 1 to 4. It is independent of milestones 5 (model
routing) and 6 (IDE context), except for the model list, which is stubbed
until M5's `model.list` exists.

Read first: `AGENTS.md`, `docs/VISION.md` ("Order of attack", item 6:
editors are clients), `docs/ROADMAP.md` Phase 5, ADR 0001 (crates and
their edges), ADRs 0009 and 0010 (secure channels, devices) and
`docs/LESSONS.md`. "Grounding" at the end summarises what Paseo's app and
Zed's remote and agent UIs do, with paths in their repositories.

## Goal

Roadmap Phase 5, first item. Everything the CLI does with the fleet can be
seen and done in a browser, on the operator's own machine:

```bash
vessel ui                       # serves the UI on 127.0.0.1 and opens the browser
vessel ui --port 7080 --no-open # prints the URL to open instead
```

The screens:

1. **Fleet overview.** Every node in the book, with:
   - how it is reached (direct, SSH, relay, paired) and whether it is
     connected now;
   - its harnesses and profiles;
   - its live sessions, with counts.
2. **Sessions of a node.**
   - Filters: status, mode, harness, profile, creator.
   - Who created each session: the owner, a device or an agent. Sessions
     agents started are shown as a tree under their parent session.
3. **A session.**
   - **Terminal sessions** attach in xterm.js over the session's event
     stream, replay from the start, resume after a drop, and send input
     and resizes.
   - **Structured sessions** show the conversation timeline: messages,
     thoughts, tool calls with their diffs, the plan, and permission
     requests that can be answered. A prompt box starts a turn, and the
     turn can be cancelled.
   - **Actions:** kill; copy to another profile or node; move to another
     profile (structured sessions only, as `session.move_profile` allows).
4. **New session:** harness, profile, mode, model and working directory.
   The model is free text until M5's `model.list` exists.
5. **Fleet:**
   - the home and its members, with each member's applied generation;
   - conflicts;
   - nodes forgotten or revoked.
6. **Devices and pairing:**
   - devices paired with a node, and revoking one;
   - inviting a new device, with the link and its QR code;
   - adding a node to the book from a pairing link (the CLI's
     `vessel nodes add --pair`).
7. **Settings:** the book (add, remove, set the default node) and the UI's
   own preferences (theme).

The browser never holds a node's credentials and never speaks Noise. The
local `vessel ui` process does, with the same book, tokens and device
credentials the CLI uses.

## Out of scope

- **Serving the UI from the node itself** (`vesseld --web`). A node would
  need a way to log a browser in that is as strong as a paired device, and
  it would expose the UI to the node's network. It is a follow-up (ROADMAP),
  once this UI is settled. Today `vessel ui` on a laptop reaches every node
  the book reaches.
- **Reaching `vessel ui` from another machine.** It binds loopback only. A
  phone or a second computer pairs with the nodes directly, or uses SSH
  port forwarding.
- **Editing files.** No file browser, no editor, no git views beyond the
  diffs inside tool calls; that is editor integration (Phase 5's next item).
- **Browsing the node's file system** for the working directory. There is
  no protocol method for it. The field is free text, with the cwds of
  recent sessions offered.
- **Model routing and its model list** (M5): a stub until `model.list`
  lands.
- **Translations.** The strings are English, kept in one module so a
  translation can follow.
- **A mobile-first layout.** It works on a narrow window; a phone app is
  later.

## What exists

- **Protocol 6 covers every screen.** Mapped to methods:

  | Screen | Methods |
  | --- | --- |
  | Fleet overview | `node.info`, `harness.list`, `profile.list`, `session.list` per node |
  | Sessions, sessions as a tree | `session.list` (`created_by: Creator`; `Creator::Agent { node, session }` names the parent) |
  | Session | `session.get`, `session.read` (pages), `session.subscribe` (through the client's gap-free follow), `session.send` (terminal input and resize, prompt, cancel, permission answer), `session.kill`, `session.move_profile`, `session.import` (copy, and move to another node as import then kill, as the CLI does) |
  | New session | `harness.list`, `profile.list`, `session.create` |
  | Fleet | `fleet.nodes`, `fleet.members` (admin), `fleet.forget` (admin) |
  | Devices | `device.list`, `device.invite`, `device.revoke` |
  | Settings | the book (`vessel_client::book`), client side |

- **`vessel-client`** has:
  - `NodeClient` with typed calls;
  - connectors for websocket, secure (paired, direct or relayed) and, through `vessel-remote`, SSH;
  - `Pinned`, which accepts only the node the book recorded;
  - `follow_resuming`: a session stream that redials through its connector with backoff and stays gap-free by sequence number;
  - the node book.
- **`vessel-cli`** turns a book entry into a connector (`connect.rs`:
  `NodeConnector`, tokens, credentials). It pairs with a node from a link
  as a client (`nodes add --pair`) and renders QR codes with the
  workspace's `qrcode` crate.
- **`vessel-testkit`** provides `fake-tui` and `fake-acp`, and runs
  `vesseld` for tests outside `vessel-node`.
- **What is missing:**
  - generated TypeScript for the wire types;
  - a way to forward an untyped `Call` through a `NodeClient` (only typed calls exist);
  - `model.list` (M5);
  - any web code at all.

## Contract changes

None to the protocol: the UI speaks protocol 6 through the bridge, and no
node changes. Two changes to shared crates, both approved (decisions 2 and
5), each in commits of its own so they merge cleanly against M5 and M6,
which add protocol rows of their own:

1. **`vessel-proto`, an optional feature `ts`.** It adds `ts-rs` derives on
   every wire type, behind `cfg_attr(feature = "ts", ...)`, so nothing
   changes for builds without it.
   - A test under the feature writes the TypeScript into `web/src/wire/proto/`.
   - CI regenerates it and fails on any difference.
   - Overrides for types serde writes as text (keys, base64 bytes, ids)
     sit next to the types.
   - The generated files are committed. They contain no `any`; a type that
     would map to `unknown` is fixed at its source, or the exception is
     recorded in ADR 0017 (`RawJson`, opaque by design, is the expected
     one).
2. **`vessel-client` (and the `vessel-rpc` client under it), one method:**
   `NodeClient::call_any(Call) -> Result<Reply, CallError>`. It sends an
   already-built `Call` and returns the `Reply` as it came.
   - Scopes stay the node's to enforce, unchanged.
   - The browser's JSON is decoded into the typed `Call` at the bridge,
     never into an untyped tree.
   - The protocol-version gate (`NotInProtocol`) applies as it does to
     every typed call.

## Crates and seams

```
vessel-cli ──> vessel-web ──> vessel-client, vessel-proto
     │                             (axum, tokio; ts-rs as a dev-dependency)
     └── implements vessel-web's `NodeSource` with the book and its connectors
web/  (TypeScript, Vite)  ── built into web/dist, embedded by vessel-web's build.rs
```

- **`vessel-web`** (new library crate). It owns:
  - the HTTP server, loopback only;
  - the per-launch token;
  - the Host and Origin checks;
  - the security headers;
  - the embedded static files;
  - the bridge's websocket and its frames (`vessel_web::wire`);
  - one connection per node, shared by every browser tab, with its status;
  - the session streams it forwards.

  It must not:
  - resolve book entries into connectors itself (that is the CLI's
    knowledge: tokens, SSH, credentials);
  - interpret the protocol beyond routing.
- **The seam is `NodeSource`**, a trait in `vessel-web` that lists the
  book's nodes and gives a `Connector` for one. `vessel-cli` implements it
  with `nodes::resolve` and `NodeConnector`. The bridge's tests implement
  it with in-process nodes. It is generic, not `dyn`: one production
  implementation and test fakes behind a type parameter (AGENTS.md section 2).
- **`vessel-cli`** gains:
  - `vessel ui`: its flags, opening the browser with the platform's command, and printing the URL;
  - the bridge-only operations that need the CLI's knowledge (pairing from a link, editing the book), through `NodeSource`.
- **`web/`** holds:
  - `src/wire/` (generated, not edited);
  - `src/bridge/` (the websocket client: typed calls, follows, reconnect);
  - `src/data/` (stores: nodes, sessions, timelines; the timeline reducer; the terminal adapter);
  - `src/routes/` (screens);
  - `src/ui/` (components, which the design spec fills in);
  - `src/strings.ts`;
  - `e2e/` (Playwright, screenshots).
- **Docs:**
  - ADR 0001 and `docs/ARCHITECTURE.md` gain the crate and its edges;
  - ADR 0017 records the decisions below (0015 and 0016 are M5's and
    M6's);
  - `README.md` gains a "Web UI" section.

## Data flow

```
browser tab ──ws /api/ws (JSON frames)──> vessel ui (bridge)
                                            ├─ node A: NodeClient (direct / SSH / relay / paired)
                                            ├─ node B: NodeClient ...
                                            └─ streams: follow_resuming per (tab, session)
```

1. **Load.** The page loads `index.html` and its assets without
   authentication: they hold no secrets. The script reads `#token=` from
   the URL fragment and keeps it in `localStorage` for this origin. Origin
   includes the port, so one launch's token is not another's. The script
   then removes the fragment from the address bar.
2. **Connect.** The page opens `/api/ws`. The upgrade must carry the exact
   Origin. The first frame must be `hello { token }` within 5 s.
3. **Calls.** `call { id, node, call }`, where `call` is the protocol's own
   `Call`, goes to that node's shared connection. The bridge dials it on
   first use and keeps it. The answer comes back as
   `reply { id, outcome }`, with the protocol's own `Outcome`.
   - Connection-scoped methods (`session.subscribe`, `session.unsubscribe`)
     are refused; streams have their own frames.
   - Everything else is the node's to authorize, as it is for the CLI.
   - A call in flight when a node connection drops fails as
     `disconnected` and is never retried. As in `ResumingStream`, a
     `session.send` or `session.kill` must not run twice.
4. **Streams.** `follow { stream, node, session, from }` starts a
   `follow_resuming` on that node's connector. Its records come as
   `event { stream, record }`. Losing and regaining the node come as
   `stream { stream, state: reconnecting | resumed | ended }`.
   `unfollow { stream }`, or the tab closing, stops it.
5. **Node status.** The bridge pushes `node { node, status }`:
   `connecting`, `connected`, `reconnecting` (with the error), `failed`.
   The UI shows the status per node, as Zed's title bar does.
6. **Reconnecting the tab.** If the tab's websocket drops (laptop sleep), it
   reconnects, sends `hello` again, and re-follows every open session from
   the last sequence it applied. Records are gap-free and never repeated,
   so the stores just continue.
7. **Terminal.**
   - The view replays the session's terminal records from the start, as
     `vessel attach` does. Retention bounds how much that is.
   - It then follows live.
   - Input is `session.send { Terminal { Data } }`. A resize is sent only
     by a tab that has focus and has typed, so a tab that only watches
     never resizes the harness (Paseo's rule).
8. **Structured timeline.**
   - A reducer turns `AgentEvent`s into entries with stable ids:
     - user and assistant messages, chunks merged by turn;
     - thoughts;
     - tool calls upserted by `ToolCallId`;
     - the latest plan;
     - permission requests by `PermissionId`, pending until resolved.
   - The reducer is pure TypeScript, unit tested with event sequences
     recorded from `fake-acp` through the Rust side, so the two languages
     are tested against the same records.
   - Long timelines render the last turns and page older ones in with
     `session.read`. As in Paseo, the stream is for speed and the read is
     for correctness.

## Steps

Each step ends with the local gate (Rust gate, plus the web checks once
they exist) and green CI on Linux, macOS and Windows.

1. **B1: the plan and ADR 0017.**
2. **B2: `vessel-web`'s server.**
   - Loopback bind, per-launch token, the Host/Origin checks, the headers,
     embedded files with a placeholder page when no bundle was built.
   - `vessel ui` in the CLI.
   - Rust tests:
     - a wrong Host and a cross-origin upgrade are refused;
     - an unauthenticated websocket is closed;
     - files are served with the right types;
     - a non-loopback bind is refused.
3. **B3: `web/` scaffold and CI.**
   - Vite, React, TypeScript strict.
   - ESLint (`strict-type-checked`, `no-explicit-any`, no unsafe access).
   - Prettier, vitest.
   - A font check over the CSS (below).
   - A web job in CI on Linux: install, lint, type-check, unit tests,
     build; then the Rust build embeds the bundle.
4. **B4: generated types.**
   - The bridge's frames with `ts-rs` in `vessel-web`.
   - `vessel-proto`'s `ts` feature, once approved.
   - The CI drift check.
5. **B5: the bridge.**
   - `NodeSource`, the node registry and its status pushes, call
     forwarding, follows with resume.
   - Rust tests against real nodes in-process: a call round trip, a
     refused method, a stream that resumes across a dropped node
     connection, two tabs sharing one node connection.
6. **B6: the data layer in TypeScript.**
   - The bridge client with reconnect and typed calls
     (`call(node, "session.create", params)` returns that method's result
     type, derived from the generated `Call` and `Reply` unions).
   - The stores, the timeline reducer and the terminal adapter.
   - vitest tests, including the reducer against the recorded `fake-acp`
     events.
7. **B7: the screens, plain.**
   - Routes and working, unstyled screens, until the design spec arrives.
   - Playwright end to end against a real `vesseld` with the testkit fakes,
     on Linux in CI:
     - the fleet overview lists the node;
     - a terminal session runs `fake-tui` and echoes keys;
     - a structured session runs `fake-acp` through a turn with a
       permission request answered;
     - a session is killed;
     - a device is invited and revoked.
   - A Playwright project `screens` that captures every main screen into
     `web/artifacts/screens/` (untracked).
8. **B8: the design.** Apply the design agent's spec: components, layout,
   theme. The screenshots are taken again for the operator's report.
9. **B9: closing.**
   - `README.md`, `docs/ARCHITECTURE.md`, `docs/ROADMAP.md`.
   - The manual smokes below.
   - The release change, if decision 3b is taken.

## Fonts (operator's rule for every site of ours)

- Every `font-family` lists Pretendard before any generic family. Never
  bare `monospace` or `serif` first, and never Gulim, Dotum or Batang: on
  Korean Windows those generic families are drawn as bitmap fonts.
- Body: `Pretendard, -apple-system, BlinkMacSystemFont, "Segoe UI",
  sans-serif`.
- Terminal and code: `Consolas, "SFMono-Regular", Menlo, "DejaVu Sans
  Mono", Pretendard, monospace`. That is a real monospace font first,
  then Pretendard, then `monospace`.
- xterm.js takes its font family as an option. That string follows the
  same rule.
- Pretendard ships with the bundle (its OFL licence allows it), so it
  never depends on what the machine has installed.
- A web check fails the build on any `font-family` in our CSS or code that
  breaks the rule.

## Security

The bridge holds every credential the user has. Anyone who can drive it
can drive the fleet. So:

1. **Loopback only.** It binds `127.0.0.1` by default, or `[::1]`, and
   refuses any other address. Remote use is an SSH forward, out of scope.
2. **A per-launch token.**
   - 256 random bits, base64url, minted at start and never written to
     disk.
   - It is given in the URL fragment, which is not sent to servers, not
     logged, and not in `Referer`.
   - It is required on every API request (`Authorization: Bearer`) and as
     the websocket's first frame.
   - Compared in constant time.
3. **DNS rebinding.** Every request's `Host` must be exactly
   `127.0.0.1:PORT`, `localhost:PORT` or `[::1]:PORT`, else `421`.
4. **Cross-origin.**
   - The websocket upgrade and every API request must carry
     `Origin: http://<one of those hosts>:PORT`; a missing `Origin` is
     refused (browsers always send it on a websocket).
   - No CORS headers are sent, ever.
   - With the token in a header and no cookie, a cross-site page cannot
     make an authenticated request at all.
5. **Headers on every response:**
   - `Content-Security-Policy: default-src 'self'; connect-src 'self';
     img-src 'self' data:; style-src 'self'; object-src 'none';
     base-uri 'none'; frame-ancestors 'none'`;
   - `X-Content-Type-Options: nosniff`;
   - `Referrer-Policy: no-referrer`;
   - `Cache-Control: no-store` on `index.html`.

   The CSP has no inline script; the build emits none.
6. **Untrusted content.**
   - What agents and terminals print is input from whatever the agent
     read.
   - Messages render as Markdown with raw HTML disabled.
   - Links get `rel="noopener noreferrer"` and only `http(s)` schemes.
   - Terminal output goes to xterm.js, which draws text, not HTML.
   - Nothing from a node reaches `innerHTML`; the lint rule
     `react/no-danger` is on.
7. **Secrets.**
   - The browser never sees a node token, a device credential or a secret
     value.
   - `secret.list` returns names only.
   - Setting a secret sends the value once and never shows it.
8. **Other users of the machine** can reach the port, but without the
   token they get only the static files. Paseo's local daemon, by contrast,
   accepts anyone on loopback by default (Grounding).

## Done criteria

- **`vessel ui` works on Linux, macOS and Windows.** The bridge's Rust tests
  pass on all three in CI. The web job (lint, type-check, unit tests,
  build, Playwright end to end) passes on Linux.
- **Every screen works against the testkit fakes:** fleet overview, sessions
  (with an agent-created tree), a terminal session, a structured session
  with a permission answered, new session, fleet, devices and pairing,
  settings.
- **Strict TypeScript with no `any`**, enforced by lint.
- **Generated types**, checked against drift in CI.
- **The font rule**, enforced by a check.
- **Screenshots of every main screen**, from `npm run screens`, in
  `web/artifacts/screens/` (untracked), for the operator's report.
- **Manual smokes**, by the coordinator, with scripts in `docs/smoke/`
  where they help:
  1. **The real book.** The fleet overview shows each kind of node (local,
     SSH, relayed or paired).
     - A real terminal harness, attached in the browser, keeps working
       through a dropped network: it says it is reconnecting, then
       resumes with no lost or repeated output.
     - A structured session answers a permission request and finishes a
       turn.
  2. **Pairing.**
     - Invite a device from the Devices screen and pair a second machine
       with the QR's link.
     - Revoke it, and its connection closes.
  3. **Profiles.** Move a structured session to another profile from the
     session screen; the conversation continues.
  4. **Security, by hand:**
     - a request with a wrong `Host`, a page on another origin opening the
       websocket, and a stale token from a previous launch are all
       refused;
     - another OS user on the machine gets only the static files.

## Decisions

Settled by the coordinator on 2026-11-07, all as recommended below
(ADR 0017), with these additions:

- 2: the generated TypeScript is committed and CI fails on any
  difference; it holds no `any`, and every `unknown` is fixed at its
  source or recorded in the ADR.
- 3b: `vessel` is released too, with the UI embedded.
- 4: a fresh token per launch; the fragment is removed with
  `history.replaceState` right after it is read; tokens are compared in
  constant time; a websocket whose first frame is not a valid token
  within a short deadline is refused.
- 5: as in "Contract changes" above.

The options as weighed:

1. **React or Svelte.**
   - **Recommendation: React 19, with TanStack Router.**
     - The largest set of tested parts for what we need: xterm.js
       wrappers, virtual lists, Markdown without HTML, accessible
       primitives.
     - The most predictable output from the agents that write and review
       this code.
     - Strict TypeScript with mature lint rules (`react-hooks`,
       `no-danger`).
     - Paseo, whose ideas we borrow, is React.
     - TanStack Router types route parameters, so a link to a missing
       route or a wrong parameter is a compile error (the same goal as
       AGENTS.md section 2).
   - **Svelte 5**: smaller bundles and less boilerplate, but a smaller
     ecosystem for the pieces above. The bundle size hardly matters for a
     page served from loopback.
2. **How the TypeScript types are generated.**
   - **Recommendation: `ts-rs`** behind `vessel-proto`'s `ts` feature (a
     contract change; needs your yes).
     - It is derive-based, follows `serde` attributes (tags, renames,
       flatten), and is the most used and maintained of the options.
     - A Rust test exports the files, and CI fails on drift.
     - Two things need care: `u64` fields become `number` rather than
       `bigint` (our sequence numbers and sizes stay far below 2^53), and
       optional fields that serde skips are marked optional.
   - **`specta`**: similar, but its 2.x line has been in release candidates
     for a long time.
   - **`schemars` plus a JSON Schema to TypeScript tool**: two generators
     and a Node step in the Rust gate.
   - **`serde-reflection`**: no derives at all, but it cannot represent
     internally tagged enums, which the protocol uses throughout.
   - **Hand-written types**: drift.
3. **How the bundle is built and embedded.**
   - **Recommendation:**
     - `vessel-web`'s `build.rs` embeds `web/dist` when it exists, and a
       placeholder page that says how to build the UI when it does not.
     - CI's web job and the release build the bundle first, and set
       `VESSEL_WEB_REQUIRE_BUNDLE=1`, which turns a missing bundle into a
       build error.
     - So `cargo build` and the Rust gate never need Node, and nothing
       built is committed.
   - **Committing `web/dist`**: `cargo install` from git gets the real UI
     without Node, at the cost of minified diffs in every UI change and a
     CI check that the committed bundle matches a fresh build.
   - **Serving files from a directory at run time**: two things to
     install.
   - **3b.** Releases ship only `vesseld` today. **Recommendation:**
     - release `vessel` too, with the UI embedded, for the three
       platforms;
     - meanwhile, the README says to run `npm ci && npm run build` in
       `web/` before `cargo install` from a checkout.
4. **The token's home in the browser.**
   - **Recommendation:** the URL fragment, then `localStorage`, sent as a
     header and as the websocket's first frame. **No cookie.**
   - Cookies are scoped by host, not by port. A cookie for `127.0.0.1`
     goes to every server on `127.0.0.1`, so another local web server
     the browser visits, another user's for instance, would receive it.
   - `localStorage` is per origin, port included. It survives a reload
     and is shared by the launch's tabs. A token left there after the
     process exits is useless: the next launch mints another.
   - **A cookie with `SameSite=Strict` and a per-port name**: it still
     leaks to other local ports.
5. **What the bridge speaks to the browser.**
   - **Recommendation:** the protocol's own `Call`, `Reply`, `Outcome` and
     `EventRecord`, in a thin envelope: `call`, `reply`, `follow`, `event`,
     `stream`, `node`.
     - Every method is there for the UI at once.
     - The types come straight from `vessel-proto`.
     - The node keeps authorizing everything.
   - **Implementation:** forward an untyped `Call` with a new
     `NodeClient::call_any` (a small change in `vessel-client` and
     `vessel-rpc`, needs your yes). Without it, an exhaustive `match`
     over `Call` in the bridge.
   - **A UI-specific API with view models**: the UI is less coupled to the
     protocol, but every screen needs a second type and a second
     endpoint.
6. **Where the server lives.**
   - **Recommendation:** a new crate, `vessel-web`, used by `vessel-cli`'s
     `vessel ui`.
     - It can be tested on its own.
     - It owns its `build.rs` and its dependencies (axum is already in the
       workspace).
     - The CLI keeps the knowledge of how book entries become connections.
   - **A module of `vessel-cli`**: one crate fewer, but the HTTP server, the
     embedding and the bridge would sit in the crate that assembles
     everything else.
7. **Data layer libraries.**
   - **Recommendation:** no TanStack Query. A small store per domain
     (`useSyncExternalStore`), fed by the bridge's pushes and by calls.
     - The bridge already pushes live data.
     - When data is fresh is ours to decide, from sequence numbers, not a
       cache's timer.
   - **TanStack Query**: retries and cache timing that fight the push
     model.
8. **Messages.**
   - **Recommendation:** Markdown with `react-markdown` and `remark-gfm`,
     raw HTML off.
   - **Plain text**: safe, but the agents write Markdown.
9. **QR codes.**
   - **Recommendation:** the bridge draws the invitation's QR as SVG with
     the workspace's `qrcode` crate, which needs its `svg` feature. The
     web side needs no QR dependency.
   - **A small JavaScript QR library**.
10. **Package manager.**
    - **Recommendation:** npm. It ships with Node, and its lockfile is
      checked in.
    - **pnpm**: faster, but one more tool on every runner and machine.
11. **`vesseld --web`.** Recommendation: a ROADMAP follow-up, not this
    milestone (Out of scope).

## Grounding

Ideas only. Paseo's app is read for its approach; Zed is GPL, and nothing
is copied from either. Paths are in each project's own repository.

**Paseo's app** (`packages/app`, `packages/server`, `packages/protocol`):

- **Stack.**
  - One Expo/React Native app, also built for the web (Metro, Expo Router).
  - State in zustand stores plus TanStack Query for git queries.
  - One websocket to the daemon: JSON frames, plus binary terminal frames.
- **Types.** Zod schemas in a shared package (`packages/protocol/src/messages.ts`, about 7,900 lines), with validators generated per message. Our source of truth is Rust, so we generate TypeScript from it instead.
- **Serving.** The daemon serves the web build:
  - `packages/server/src/server/web-ui.ts`, pre-compressed by `scripts/build-daemon-web-ui.mjs`;
  - immutable caching for hashed files and `no-store` on `index.html`;
  - a connection hint injected into `index.html`.
- **Live data** (`docs/timeline-sync.md`):
  - "stream for speed, fetch for correctness", with epochs and sequence numbers and catch-up after a gap;
  - an honest "Reconnecting" and "Updating" state;
  - assistant text coalesced every 60 ms;
  - about 100 rows mounted on the web, older history paged backward.
- **Terminal.**
  - xterm.js with the fit, WebGL, search and web-links addons.
  - Replay on reconnect, deduplicated by revision.
  - One client owns the PTY size, claimed on focus or input
    (`docs/terminal-performance.md`).
- **Permissions.** Requests are cards pinned above the timeline
  (`agent-stream/view.tsx`). Finished turns collapse.
- **Tests.**
  - Vitest next to the code; Playwright end to end against a real daemon
    on a random port (`e2e/support/global-setup.ts`).
  - Fakes injected instead of mocks or JSDOM (`docs/testing.md`).
  - Screenshots as QA evidence, not baselines.
- **What we avoid:**
  - no password by default, so anyone on loopback has full control;
  - websocket upgrades accepted without an `Origin`;
  - a hosted origin allowed by default;
  - CORS that reflects the origin with credentials;
  - passwords kept in plain text in browser storage;
  - 7,000-line client and protocol files.
  - The Host allowlist (`server/hostnames.ts`) is worth copying, made
    stricter.

**Zed** (`crates/remote`, `crates/acp_thread`, `crates/agent_ui`, `crates/terminal_view`):

- **Connection state.** A detailed internal state (heartbeat missed,
  reconnecting, failed, exhausted) behind a small public one the UI shows.
  It appears as a dot and tooltip in the title bar, and a modal with
  "Reconnect" when it gives up (`remote/src/remote_client.rs`,
  `title_bar/src/title_bar.rs`, `recent_projects/src/disconnected_overlay.rs`).
  We keep a small public node status too.
- **Resuming.** Messages carry ids and acknowledgements, and the unacked
  ones are replayed after a reconnect. We need none of it: our state lives
  in the node, and every record has a durable sequence number, so resuming
  is a read from a cursor (as `vessel_client::ResumingStream` already does).
- **The ACP thread** (`acp_thread/src/acp_thread.rs`):
  - one ordered list of entries (user message, assistant message chunks,
    tool call), with tool calls upserted by id;
  - patches that tell "omitted" from "null";
  - the plan replaced whole on each update;
  - permission requests kept apart from tool calls and linked by id,
    answered by option id after checking the option was offered;
  - a pending prompt that may be "accepted or not, unknown" after a
    disconnect.
- **What to avoid from Zed:**
  - entries addressed by index, which breaks when one is removed: we use
    stable ids;
  - two protocol versions mixed in one thread.
- **Terminal.** Zed's remote terminals die with the SSH link, and only the
  working directory is restored. Vessel's sessions run on the node, so the
  view replays and resumes. "Snapshot then deltas" and buffering output
  that arrives before the view exists both apply.
