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
  are refused; streams have frames of their own.

**Authentication.**
- A fresh 256-bit token is minted per launch and never written to disk.
- It reaches the page in the URL fragment. The page keeps it in
  `localStorage` (per origin, port included) and removes the fragment
  with `history.replaceState`.
- It is sent as `Authorization: Bearer` and as the websocket's first
  frame, within a short deadline. Tokens are compared in constant time.
- **No cookie.** Cookies are scoped by host, not port, so one for
  `127.0.0.1` reaches every local web server the browser visits,
  another user's included.

**The server's defences.**
- It binds loopback only.
- `Host` must be one of the bound address's names with its port
  (DNS rebinding).
- `Origin` must match on the websocket and every API request; a
  missing `Origin` is refused.
- No CORS headers are ever sent.
- A strict Content Security Policy with no inline script.

**Front end.**
- React 19 with TanStack Router (typed routes), Vite, TypeScript strict,
  ESLint `strict-type-checked` with `no-explicit-any`.
- Small stores fed by the bridge's pushes, without a query cache.
- Markdown with raw HTML disabled.
- npm.

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
