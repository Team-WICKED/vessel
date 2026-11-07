# Vessel

An **Agentic Development Environment**: one lightweight node per machine that
runs your agent harnesses (Claude Code, Codex, Gemini, OpenCode), and clients
that reach every node as if it were local.

Vessel is not an editor. It is the layer underneath: the daemon, the wire
protocol, the relay, and the sync. Editors (Zed, VS Code, a future native
client) plug into it.

## Why

Today each machine is an island. The harness on your desktop cannot hand work
to the Linux box with the GPU, your skills and MCP servers are configured
separately everywhere, and remote access means SSH plus luck.

Vessel aims for:

1. **Lightweight.** Native Rust, single binary, no runtime to install.
2. **Harness-agnostic.** Wraps the CLIs you already use instead of replacing
   them. Remote feels local.
3. **A fleet, not a host.** First-run setup in minutes. Skills, MCP servers,
   memory and conversation history sync across machines. Every agent on every
   node can see and delegate to the others.
4. **Any model, any harness.** Route a harness to whatever model you like.
5. **IDE-grade context.** Symbols, references, diagnostics and call graphs,
   fed to the agent through LSP rather than locked inside one editor.

See [docs/VISION.md](docs/VISION.md) for the long version and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the pieces fit.

## Status

Milestones 1 to 4 are done, on `main`, and green on Linux, macOS and
Windows ([docs/ROADMAP.md](docs/ROADMAP.md) has the details and what is
next):

1. **One node, done properly.** Terminal sessions in a PTY and structured
   sessions over ACP, a durable log every client can follow and resume,
   a token on every connection, the `vessel` CLI.
2. **Any machine over SSH.** `vessel` installs and updates `vesseld` there
   itself; an attach survives a lost connection.
3. **Any machine behind NAT.** A relay that carries only end-to-end
   encrypted traffic, and devices paired with a node, each revocable.
4. **One person's machines as one fleet.** Harness profiles, instructions,
   skills and MCP servers shared from one home node, conversations and
   memory moved on request, first-run setup, and agents that start and
   drive sessions on any node of the fleet (below).

Being built: a web UI, `vessel ui` (milestone 7, on `backend/m7`; "Web UI"
below). Not yet: model routing, the LSP bridge.

## Try it

```bash
cargo build --release
./target/release/vessel init                # node, harnesses, profiles, logins; asks before changing anything
./target/release/vessel init --service      # and run the node as a per-user service (Linux, macOS)
./target/release/vessel init --join --yes < link.txt   # and join a fleet with the link `vessel fleet invite` printed
./target/release/vessel node start          # or run it in the foreground (or ./target/release/vesseld)
# in another terminal, on the same machine (the token is read from ~/.vessel):
./target/release/vessel status
./target/release/vessel run claude          # Claude Code's own TUI; Ctrl-] detaches
./target/release/vessel run claude --structured   # needs claude-agent-acp on PATH
./target/release/vessel sessions
./target/release/vessel attach 3f2a         # any unique prefix of a session id
./target/release/vessel log 3f2a > session.log
```

Sessions survive client disconnects; stopping the node marks live ones
`Interrupted`, and their logs stay readable after a restart.

### Another machine, over SSH

Any machine you can `ssh` to with a key works (Linux or macOS,
`x86_64` or `aarch64`); nothing needs to be installed or opened there
first.

```bash
./target/release/vessel nodes add gpu-box me@gpu-box --default
./target/release/vessel run claude          # installs vesseld, starts the node, attaches
./target/release/vessel --node gpu-box sessions
./target/release/vessel node status         # version, serving/draining, live sessions
./target/release/vessel node update --when-idle   # new vesseld once its sessions end
```

- The first contact uploads `vesseld` to `~/.vessel/bin/` on that machine:
  the build next to `vessel` (a development checkout, named by content
  hash), or this version's release (fetched with `gh`). `vessel node
  install --from local|release` does it explicitly.
- The node's id is recorded in `~/.config/vessel/nodes.toml`, and a
  different machine answering at that name later is refused.
- A project can pick one of your nodes with `.vessel/config.toml`
  (`default_node = "gpu-box"`), and may do nothing else.
- A dropped connection during `attach` reconnects by itself and continues
  where it was.

### Running a relay

A relay lets clients reach a node that has no public address. It only
splices connections: everything through it is end-to-end encrypted to the
node's key (ADR 0009), and it keeps nothing on disk.

```bash
head -c 32 /dev/urandom | base64 > relay.token && chmod 600 relay.token
./target/release/vesseld relay --bind 127.0.0.1:7078 --token-file relay.token
```

- Nodes that use the relay need the same token; clients need none.
- The relay speaks plain HTTP. For `wss://`, put it behind the HTTPS
  reverse proxy that already serves the machine, for example with nginx
  (the relay URL is then `wss://relay.example.com/vessel`):

  ```nginx
  # In the http block: connections per client address.
  limit_conn_zone $binary_remote_addr zone=vessel:10m;

  location /vessel/ {
      limit_conn vessel 16;
      proxy_pass http://127.0.0.1:7078/;
      proxy_http_version 1.1;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_set_header X-Real-IP $remote_addr;
  }
  ```

  and run the relay with `--client-address-header X-Real-IP
  --trusted-proxy 127.0.0.1`.
- Behind a proxy every connection comes from the proxy, so the
  per-client limits need the client's address from a header
  (`--client-address-header X-Real-IP`, or `CF-Connecting-IP` behind
  Cloudflare). The header is believed only from the peers named with
  `--trusted-proxy` (an address or a network in CIDR form, repeatable),
  and the relay refuses to start with a header and no trusted proxy;
  anyone else is counted by their own address, whatever they send. The
  proxy must overwrite the header with one bare address, IPv4
  (`192.0.2.7`) or IPv6 (`2001:db8::7`): a list (as `X-Forwarded-For`
  grows), a port, brackets or the header twice are answered `400`, and
  a request from the proxy without the header counts as the proxy
  itself, rate included. A trusted proxy's sockets are not
  counted per address at accept (they would all be the proxy's): the
  relay counts per client from the header once a request arrives, so
  the proxy should limit connections per client itself, as `limit_conn`
  does above.
- The relay pings every connection every 10 s, so a proxy's idle timeout
  of 60 s (nginx's default) is enough. A connection silent for 30 s, its
  pings unanswered, is dropped.
- `GET /health` answers `ok`.
- Limits, each a flag with a `VESSEL_RELAY_*` variable of the same name
  (table below). The defaults leave a fleet of tens of nodes plenty of
  room:

  | Flag | Limits | Default |
  | --- | --- | --- |
  | `--max-nodes` | nodes registered at once | 128 |
  | `--max-registering` | nodes still registering at once | 64 |
  | `--max-clients` | relayed connections at once, in all | 384 |
  | `--max-clients-per-node` | relayed connections to one node at once | 128 |
  | `--max-bytes-per-second` | bytes a second each way through one connection, after a one-second burst (0: no limit; at least 64 KiB) | 16 MiB |
  | `--max-connections` | sockets open at once, of every kind, counted from accept | 1000 |
  | `--max-unauthenticated` | of `--max-connections`, sockets that have not authenticated yet; the rest is kept for registered nodes and paired clients. Less than half of `--max-connections` | 400 |
  | `--max-connections-per-address` | sockets not authenticated yet from one address, counted from accept; not applied to a trusted proxy's sockets. 4 × it under half of `--max-unauthenticated` | 16 |
  | `--max-registering-per-address` | nodes still registering at once from one address. 4 × it under half of `--max-registering` | 7 |
  | `--reserved-registering` | of `--max-registering`, places kept for nodes that registered here before (at most half) | 16 |
  | `--max-clients-per-address` | relayed connections at once from one address. 4 × it under half of `--max-clients` | 16 |
  | `--max-node-clients-per-address` | relayed connections to one node at once from one address while the node admits them (their first 7 s). 4 × it under half of the 40 a node admits at once through one relay | 4 |
  | `--trusted-proxy` | a proxy whose `--client-address-header` is believed (above); repeatable | none |

  Per address means an IPv4 address, or an IPv6 `/64`; a `/56` may hold
  twice each per-address limit and a `/48` four times, so walking the
  prefixes of one allocation gains little. So that one `/48` never holds
  half of anything, the relay refuses to start unless four times each
  per-address limit is less than half of its pool, and
  `--max-unauthenticated` less than half of `--max-connections` (the
  rules in the table); the refusal names both flags, the largest value
  that would pass and the smallest pool that would. New connections from one
  address are rate-limited from accept (30 at once, then one a second;
  scaled the same way); a socket that authenticates gives its share of
  the rate back, so only sockets that never do spend it. A socket has
  authenticated once its node registered (token or ticket, and proof of
  its key), once the node's data connection was claimed, or, for a
  client, once its node has admitted it: the relay cannot see that
  (everything is encrypted to the node), but a node drops a relayed
  channel it has not admitted within 5 s, so a client still connected
  7 s after it was paired was admitted.

  A node past `--max-nodes` is told the relay is full and tries again
  every 30 s; a client or a registering node past a connection limit is
  closed with code `4503`, and a socket past an accept limit (or the
  rate) is answered `503`, both free to try again. The relay logs each
  kind of refusal at info, at most once every 10 s with the count in
  between.
- Deadlines, not configurable: a request's headers within 10 s; a
  registering node's first message, and a client's first record once its
  node answered, within 2 s; a stopping relay drops what is still open
  5 s after it was asked to stop; and a closed connection is read for at
  most 1 s more, until the other side closes too, so its close code is not
  lost to a reset (it keeps its place, and counts against the limits,
  meanwhile). Registering nodes take the open places
  first; the `--reserved-registering` places are taken only when no open
  place is left, by a connection whose first message names a node
  registered here before (since the relay started). Until it speaks such
  a connection holds no place; anyone else is then refused as full. So a
  known node gets back in while strangers hold every open place.
- What the limits hold against: one host, or one IPv6 `/48`, cannot deny
  the relay to anyone else, and nodes registered here before can register
  again while it tries. Many hosts at once (a botnet) are out of scope:
  they can fill the unauthenticated share and the open registering
  places, so new connections are refused until they stop, though nodes
  and clients already through keep working. Filtering at the network
  edge, or a DDoS service in front, is the answer to that. A node behind
  relays keeps, for each relay it is linked to, a budget of its own for
  channels waiting to be admitted through it (40), apart from the other
  relays' and its direct route's (64, of which one address holds 7, a
  `/48` 28), so a crowd through one route never takes another's room.
- A fleet's home that registers with the relay gives its members, those
  started without `--relay` of their own, tickets for their own keys
  (`fleet.callback`), never the token: a ticket registers that one node,
  for a day while the member renews it, and holds a place among
  `--max-nodes` like any node until it leaves or its ticket expires.
- Each relayed connection holds two sockets on the relay, each node one:
  keep `max-nodes + 2 × max-clients` under the relay's open-file limit
  (`ulimit -n`, or `LimitNOFILE` in a systemd unit), which the defaults
  do for the common 1024, and raise both together; `--max-connections`
  (1000) caps every socket, so the relay refuses before it runs out.

### A node behind NAT: pairing a device

A node with no public address keeps a connection to a relay (above), and
devices pair with it once. Every connection, direct or relayed, is
end-to-end encrypted to the node's key, which the device pins at pairing
(ADRs 0009 and 0010).

```bash
# on the node's machine
vesseld --relay wss://relay.example.com/vessel --relay-token-file relay.token
vessel node pair --label laptop       # prints a single-use link (and a QR code)
# on the laptop: paste the link at the prompt, never on the command line
vessel nodes add gpu-box --pair
vessel --node gpu-box run claude
# later, on the node (or from any admin device)
vessel node devices
vessel node devices revoke 3fa9       # its open connections close
```

- `vessel nodes add --pair` shows the node's key fingerprint; compare it
  with `vessel node status` on the node before confirming. Piped in, the
  link needs `--yes`.
- A node with a reachable address can advertise it (`vesseld --public
  203.0.113.5:7077`): devices try it first, for 3 s, then the relays. The
  node pings direct connections every 10 s, as a relay does, so a dropped
  network is noticed within 30 s and an attach resumes; behind a proxy, an
  idle timeout of 60 s is enough.
- The direct route holds at most 64 connections that have not finished
  their handshake and device preamble, 7 of them from one address (an
  IPv4 address or an IPv6 `/64`; a `/56` 14, a `/48` 28), so one host
  cannot keep devices out. Behind a reverse proxy every client would be
  the proxy's one address: run the node as a relay is run behind one,
  with `--client-address-header X-Real-IP --trusted-proxy 127.0.0.1`
  ("Running a relay": the header in one form, believed only from the
  proxies named, `400` otherwise; a request without it counts as the
  proxy). The node refuses to start with the header and no trusted
  proxy.
- The device's credential is a file in `~/.config/vessel/credentials/`
  (0600); the book points to it and never holds the secret. Moving a
  device means pairing it again.

## Profiles, the fleet and agent tools

Milestone 4 (ADRs 0011 to 0013). Each part works on its own: profiles on
one node; a fleet, where every node follows one home; conversations and
memory copied on request; and agents that use all of it through the
`vessel` MCP tools.

### Profiles: one harness, several accounts

A profile is a harness with its own environment, typically another
account's configuration directory. Profiles belong to the machine that runs
them: `profiles.toml` in the node's data directory (owner-only, like the
token).

```toml
[profiles.claude-work]
harness = "claude"
label = "Claude (work)"
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
unset = ["ANTHROPIC_API_KEY"]   # removed from the node's own environment

[profiles.codex-work]
harness = "codex"
env = { CODEX_HOME = "~/.codex-work" }
```

```bash
vessel profiles                              # the default per harness, and these
vessel profiles --reload                     # after an edit; a broken file changes nothing
vessel run claude --profile claude-work
vessel session move-profile 3f2a claude-work # at a usage limit: same conversation, other account
```

- Every harness has a default profile with its own name (`claude`,
  `codex`, `gemini`, `opencode`): the node's own environment.
- `vessel init` writes profiles for the other accounts' homes it finds and
  offers each one's login.
- A move keeps the session's id, log and followers. It works for
  structured Claude Code and Codex sessions, between turns (`--force`
  cancels a running one), and only to a profile that is logged in.

### A fleet: one home, many members

One node keeps your instructions and skills: the home. Every other node is
a member, and gets copies in each of its profiles (ADR 0012).

```bash
# on the home: name the sources (here, a directory you keep them in)
vessel fleet init --instructions ~/agents/AGENTS.md --skills ~/agents/skills --skip 'draft-*'
vessel fleet invite --label laptop            # a single-use link, valid 10 minutes
# on the laptop: a fresh machine sets itself up and joins in one step,
# reading the link from standard input (never the command line)
vessel init --service --join
vessel fleet status                           # what it applied, and the files it left alone
# on the home
vessel fleet members
```

- Edit the sources on the home; members follow within seconds.
- A file you edited on a member is kept, with the home's version beside it
  as `<name>.vessel-incoming`, and listed as a conflict by `vessel fleet
  status`. A fleet file you deleted stays deleted until `vessel fleet
  restore <path>`. Files Vessel did not write are never touched.
- `vessel fleet join` joins a node that is already set up; `vessel fleet
  leave` stops following, and the files stay.
- Names pick nodes (`--to`, the agents' `node`), so the home refuses a
  member named like itself or another member. A node takes its machine's
  name unless told otherwise, and machines cloned from one image share
  one: name each node when you join it (`vessel node start --name <name>`,
  `VESSEL_NAME`, or `--name` in the service unit `vessel init --service`
  wrote). A refused member says why in `vessel fleet status` and asks
  again after a restart under its new name.
- The home binds each member's node id to its key and name for good, and
  keeps a name reserved while its node's device is revoked. A member
  reinstalled with a new key, or renamed, is refused until you run `vessel
  fleet forget <node id> --node <home>` (it needs `admin` there) and then
  invite it again: forgetting withdraws every invitation still outstanding
  on the home, and the id and name then go to the first member device
  paired afterwards that claims them, so invite only that node next and
  others once it has joined. Revoke its old device there too if it should
  not sync any more.

### MCP servers and secrets

MCP servers defined on the home reach every session on every node, handed
to the harness at launch (never written into its configuration files).
A value they need is a secret, kept per node:

```toml
# in the home's fleet.toml
[mcp.servers.docs]
type = "stdio"
command = "docs-mcp"
env = { DOCS_TOKEN = { secret = "DOCS_TOKEN" } }
```

```bash
vessel fleet secret set DOCS_TOKEN                 # typed (not shown) or piped, never an argument
vessel fleet secret list                           # names only
vessel fleet secret copy DOCS_TOKEN --to NODE_ID   # to another node of the fleet
```

- A session on a node without the secret runs without that server, and
  its log says which secret is missing.
- A value is never shown, logged or passed as an argument. Copying one
  needs the target to have granted the home `admin` (`vessel fleet join
  --grant read,operate,admin`); it is asked before the value is sent.

### Conversations and memory, on request

A structured session's conversation, or a profile's memory, is copied
between profiles of one node or to another node of the fleet (`--to`):

```bash
vessel session copy 3f2a --profile claude-work --cwd /srv/app --start   # resume a copy there
vessel session move 3f2a --to gpu-box --profile claude                  # copy, resume there, stop here
vessel fleet memory copy --from-profile claude --from-project /srv/app --profile claude-work
```

- Between turns only. Nothing is overwritten: a session lands whole or not
  at all, and memory that differs goes beside the existing file as
  `<name>.vessel-incoming`. Memory is never synced continuously; each
  account keeps its own.
- The copy is renamed for the working directory it goes to, and the new
  session records where it came from.

### Calls across nodes, and what they may do

Calls from one node to another go through the home (protocol 6). A member
grants the home scopes on itself when it joins (`--grant`, by default
`read,operate`; `none` grants nothing), and a call runs on its target with
no more than that grant and the caller's own scopes. A member's calls
reach the other members only if it was invited with `vessel fleet invite
--relay`.

The home calls a member back through a route the member names. A member
with none of its own (started with `vessel node start` or the service
`vessel init --service` installs, no `--relay` or `--public`) registers
with the home's relays, which its home tells it, so run the home with a
relay (`vesseld --relay URL --relay-token-file FILE`, see "A node behind
NAT") and every member is reachable without flags. A home with no relay
has none to share: its members still sync, but the home cannot run calls
on them until one of the two gets a route (`vessel init --join` and
`vessel fleet status` say so). The relay's token stays with the home: a
member gets a ticket that registers its own key only, for a day, renewed
while it reaches its home (a member with no route has to join, and grant,
within an hour of being invited), and it dials only `wss://` relays at
public addresses, checked when it connects, or relays on the host it
reaches its home by.

### Agent tools

Every session's agent gets the `vessel` MCP server (`vessel mcp`, started
by the node) with a credential of its own, ending with the session. Its
tools: `list_nodes`, `list_sessions`, `create_session` (with a first
prompt), `send_input`, `answer_permission`, `read_events`, `wait_turn`
and `kill_session`; each but `list_nodes` takes `node`, so an agent can
start a session on another machine and read its answer.

- An agent has its creator's scopes, at most `read` and `operate`, and
  operates only on sessions it started (and theirs).
- At most 8 live sessions of its own per node, and agents start agents at
  most 3 levels deep.
- Sessions record who started them. This is attribution under one OS
  user, not a sandbox (ADR 0013).

## Web UI

`vessel ui` serves Vessel's web UI on this machine and opens it in the
browser on Unix (on Windows, open the printed address): the fleet, every node's sessions (a session an agent started
sits under the one that started it), a session as a terminal or as a
structured timeline with its prompt box and permission answers, new
sessions, fleet sync, devices and pairing with a QR code, and settings.

```bash
vessel ui            # serves on 127.0.0.1:7079 (any free port if taken), opens the browser
vessel ui --no-open  # prints the address instead
```

- The page reaches the nodes of your node book (and this machine's node)
  through `vessel ui`, which connects to them as `vessel` does: tokens,
  SSH, paired credentials and pinned ids stay in this process. The browser
  holds none of them and never speaks to a node itself.
- It listens on loopback only. Each printed address uses a fresh random
  `vessel-<id>.localhost` hostname so an earlier process on that port cannot
  seed this launch's HTML cache or service worker. Use a browser that resolves
  `*.localhost` locally, such as Chrome, Edge or Firefox; Safari does not
  support these hostnames. Do not replace the printed hostname with a fixed
  address: that loses the per-launch browser-origin isolation.
- The address's `#bootstrap=` fragment is a
  single-use code: treat it like a password until opened. The page removes
  it from the address bar and exchanges proof of it for an independent
  session key in this tab's `sessionStorage`. Reloads and network reconnects
  work; reusing the address in a fresh tab does not. Start `vessel ui` again
  for another independent browser context. Every connection verifies a fresh
  server proof and launch id; an old tab stops when the UI is restarted.
- The Unix browser handoff is a 0600 file in a directory created as 0700,
  under `XDG_RUNTIME_DIR` when set (otherwise the system temporary directory).
  SIGINT, SIGTERM and SIGHUP remove it. Forced kills/crashes cannot guarantee
  cleanup, but a redeemed code is already dead. On Windows automatic opening
  is disabled until an atomic private ACL implementation is available; the
  printed address works and no handoff file is created.
- Device pairing links and QR codes start hidden. Use **Show pairing link**;
  copying, leaving the page, or 30 seconds passing hides them again.
- One connection per node is shared by every tab. A session's stream
  survives the node's connection dropping and resumes where it was.
- Release builds of `vessel` embed the page. A build from source embeds it
  when `web/dist` exists (`npm ci && npm run build` in `web/`), and shows a
  page saying how to build it otherwise.

The page's own development (Node.js 22 or later):

```bash
cd web
npm ci
npm run lint && npm run typecheck && npm test   # ESLint, Prettier, the font rule; tsc; vitest
npm run build && npm run fonts                  # SRI-protected web/dist and built font audit
npm run e2e                                     # Playwright against real nodes with the fake harnesses
npm run screens                                 # every main screen into web/artifacts/screens/
```

`e2e` and `screens` need the Rust binaries first (`cargo build -p
vessel-cli -p vessel-node --bin vesseld` and `cargo build -p
vessel-testkit --bins`). Set `VESSEL_SCREEN_DIR` to write screenshots outside
the checkout. CI uploads only the deliberate screenshots with pairing links
hidden; traces, videos and automatic failure screenshots are disabled.

## Environment

| Variable | Means | Default |
| --- | --- | --- |
| `VESSEL_NODE` | `--node`: a book name, `ssh://[user@]host[:port]`, `user@host`, or a websocket address | the project's or the book's `default_node`, else `127.0.0.1:7077` |
| `VESSEL_TOKEN` | `--token`, for a websocket node | the book entry's token file; for a node on this machine, `~/.vessel/token` |
| `VESSEL_DATA_DIR` | `--data-dir`, this machine's data directory (the node's state; `ssh/` for shared logins) | `~/.vessel` |
| `VESSEL_CONFIG_DIR` | where the node book lives (`<dir>/vessel/nodes.toml`); tests point it at a temporary directory | the platform's config directory (`~/.config` on Linux) |
| `VESSEL_SSH` | the `ssh` program to run (tests use `fake-ssh`) | `ssh` on `PATH` |
| `VESSEL_GH` | the GitHub CLI used to download releases | `gh` on `PATH` |
| `VESSEL_CLIENT_ADDRESS_HEADER` | `vesseld --client-address-header`, behind a reverse proxy (needs a trusted proxy) | unset: the peer's address |
| `VESSEL_TRUSTED_PROXY` | `vesseld --trusted-proxy`, comma-separated | none |
| `VESSEL_RELAY_BIND` | `vesseld relay --bind` | `127.0.0.1:7078` |
| `VESSEL_RELAY_TOKEN_FILE` | `vesseld relay --token-file`, the registration token nodes present | required |
| `VESSEL_RELAY_CLIENT_ADDRESS_HEADER` | `vesseld relay --client-address-header` (needs a trusted proxy) | unset: the peer's address |
| `VESSEL_RELAY_TRUSTED_PROXY` | `vesseld relay --trusted-proxy`, comma-separated | none |
| `VESSEL_RELAY_MAX_NODES`, `VESSEL_RELAY_MAX_REGISTERING`, `VESSEL_RELAY_MAX_CLIENTS`, `VESSEL_RELAY_MAX_CLIENTS_PER_NODE`, `VESSEL_RELAY_MAX_BYTES_PER_SECOND`, `VESSEL_RELAY_MAX_CONNECTIONS`, `VESSEL_RELAY_MAX_UNAUTHENTICATED`, `VESSEL_RELAY_MAX_CONNECTIONS_PER_ADDRESS`, `VESSEL_RELAY_MAX_REGISTERING_PER_ADDRESS`, `VESSEL_RELAY_RESERVED_REGISTERING`, `VESSEL_RELAY_MAX_CLIENTS_PER_ADDRESS`, `VESSEL_RELAY_MAX_NODE_CLIENTS_PER_ADDRESS` | the `vesseld relay` limits of the same names ("Running a relay") | 128, 64, 384, 128, 16 MiB, 1000, 400, 16, 7, 16, 16, 4 |
| `VESSEL_NODE_SOCKET`, `VESSEL_AGENT_SESSION`, `VESSEL_AGENT_SECRET` | `vessel mcp` only: set by the node in each session's `vessel` MCP server | set by the node |

## Layout

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for every crate and how they
depend on each other, [docs/adr/](docs/adr/) for why, and
[docs/LESSONS.md](docs/LESSONS.md) for what Vessel takes from Zed and Paseo.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly those three on Linux, Windows and macOS. Lints are strict
(no `unwrap`, no panics, no indexing, no raw arithmetic); see the workspace
`Cargo.toml`. Changes to `web/` also need its checks ("Web UI" above),
which CI runs on Linux.

## License

Vessel is **source-available, not open source.** It is licensed under the
[PolyForm Strict License 1.0.0](LICENSE.md):

- You may read the code and use the software for noncommercial purposes
  (personal use, and noncommercial organisations as the license defines
  them).
- You may not distribute it, make changes to it or works based on it, or
  use it for any commercial purpose.
- Anything else, commercial use included, needs a separate license from
  Team-WICKED.

Contributions from outside Team-WICKED are not accepted.

Copyright 2026 Team-WICKED.
