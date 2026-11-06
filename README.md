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

Milestone 1 (one node, done properly; see
[docs/plans/backend-m1.md](docs/plans/backend-m1.md)) is implemented:

- the protocol 2 contract in `vessel-proto` (typed RPC, durable event log,
  ACP-shaped agent timeline);
- the session runtime: terminal sessions in a PTY and structured sessions
  over ACP (`vessel-proc`, `vessel-harness`, `vessel-pty`, `vessel-agent`,
  `vessel-session`), with fake harnesses in `vessel-testkit` for tests;
- the RPC layer (`vessel-rpc`), the SQLite event store (`vessel-store`),
  the node daemon `vesseld` (`vessel-node`), the typed client
  (`vessel-client`) and the `vessel` CLI (`vessel-cli`).

Milestone 2 (any node, over SSH; see
[docs/plans/backend-m2.md](docs/plans/backend-m2.md)) is in progress: a
remote machine needs only SSH access. `vessel` installs its own `vesseld`
there on first contact, `attach` starts the node, and attaching survives
a lost connection.

Milestone 3 (a node behind NAT, through a relay; see
[docs/plans/backend-m3.md](docs/plans/backend-m3.md)) is built and awaits
its manual smoke test: the end-to-end secure channel (`vessel-secure`),
the relay (`vessel-relay`, `vesseld relay`), and pairing devices with a
node, which they then reach directly or through relays.

Milestone 4 (one person's machines as one fleet; see
[docs/plans/backend-m4.md](docs/plans/backend-m4.md)) is in progress.
Built so far on one node: harness profiles (`profiles.toml`, `vessel run
--profile`, `vessel profiles`), moving a structured session to another
profile (`vessel session move-profile`) or copying it to one (`vessel
session copy|move`), and agent tools: every session's
agent gets the `vessel` MCP server (`vessel mcp`) with a credential of its
own, to start and drive sessions of its own. `vessel init` sets a machine
up and can be run again: the node, its harnesses (offering their install
commands), profiles for the other accounts it finds, their logins, and
with `--service` the node as a per-user service.

Fleet sync is built: one node, the home, shares its instructions and
skills, and every member node follows it into every profile (below).

Not yet: copying secrets between nodes, moving session history between
machines, model routing, the LSP bridge. See
[docs/ROADMAP.md](docs/ROADMAP.md).

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
  location /vessel/ {
      proxy_pass http://127.0.0.1:7078/;
      proxy_http_version 1.1;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_set_header X-Real-IP $remote_addr;
  }
  ```

- Behind a proxy every connection comes from the proxy, so the per-client
  rate limit needs the client's address: `--client-address-header
  X-Real-IP` (or `CF-Connecting-IP` behind Cloudflare). Set it only if
  every request goes through that proxy and the proxy overwrites the
  header; otherwise clients could pick their own address.
- The relay pings every connection every 10 s, so a proxy's idle timeout
  of 60 s (nginx's default) is enough.
- `GET /health` answers `ok`.

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
- The device's credential is a file in `~/.config/vessel/credentials/`
  (0600); the book points to it and never holds the secret. Moving a
  device means pairing it again.

### A fleet: one home, many members

One node keeps your instructions and skills (the home); every other node
gets copies in each of its harness profiles, keeping any local edit
(ADR 0012).

```bash
# on the home
vessel fleet init --instructions ~/.codex/AGENTS.md --skills ~/.codex/skills --skip 'paseo*'
vessel fleet invite --label laptop    # a single-use link with the `sync` scope
vessel fleet invite --label laptop --relay   # also lets the laptop reach other members
# on the laptop: paste the link at the prompt, never on the command line
vessel fleet join                     # the home gets read, operate here (--grant to choose)
vessel fleet status                   # generation, and files left as they are
# on the home
vessel fleet members
```

- Calls across nodes go through the home: a member's node grants the home
  the scopes chosen at join when it connects, and a call to another node
  runs there with no more than that grant and the caller's own scopes
  (protocol 6, ADR 0013). `vessel fleet join --grant none` grants nothing.

- Edit the sources on the home; members follow within seconds (the home
  watches `<data_dir>/fleet.toml` and its sources).
- A file you edited is kept, with the home's version beside it as
  `<name>.vessel-incoming`; a fleet file you deleted stays deleted until
  `vessel fleet restore <path>`. Files Vessel did not write are never
  touched.
- `vessel fleet leave` stops following; the files stay.
- MCP servers defined in the home's `fleet.toml` reach every session; a
  secret they need is set on each node with `vessel fleet secret set
  NAME` (typed, not shown, or piped), or copied from a node that has it
  with `vessel fleet secret copy NAME --to NODE` (the target must have
  joined with `--grant read,operate,admin`); it is never shown.

### Copying a conversation or a memory

A structured session's conversation, or a profile's memory, is copied on
demand, between profiles of one node or between the nodes of a fleet
(`--to NODE`; a member pulling from another needs `relay` on the home,
and memory needs the source to have granted the home `admin`):

```bash
vessel session copy 3f2a --profile claude-work --cwd /srv/app --start   # resume a copy there
vessel session move 3f2a --profile claude-work   # copy, resume there, stop it here
vessel fleet memory copy --from-profile claude-alt --from-project /srv/app --profile claude-work
```

- Between turns only. Nothing is overwritten: a session lands whole or
  not at all, and memory that differs is written beside the existing
  file as `<name>.vessel-incoming`.
- The copy is renamed for the working directory it goes to (Claude Code
  names its projects after it); the new session records where it came
  from.

### Environment

| Variable | Means | Default |
| --- | --- | --- |
| `VESSEL_NODE` | `--node`: a book name, `ssh://[user@]host[:port]`, `user@host`, or a websocket address | the project's or the book's `default_node`, else `127.0.0.1:7077` |
| `VESSEL_TOKEN` | `--token`, for a websocket node | the book entry's token file; for a node on this machine, `~/.vessel/token` |
| `VESSEL_DATA_DIR` | `--data-dir`, this machine's data directory (the node's state; `ssh/` for shared logins) | `~/.vessel` |
| `VESSEL_CONFIG_DIR` | where the node book lives (`<dir>/vessel/nodes.toml`); tests point it at a temporary directory | the platform's config directory (`~/.config` on Linux) |
| `VESSEL_SSH` | the `ssh` program to run (tests use `fake-ssh`) | `ssh` on `PATH` |
| `VESSEL_GH` | the GitHub CLI used to download releases | `gh` on `PATH` |
| `VESSEL_RELAY_BIND` | `vesseld relay --bind` | `127.0.0.1:7078` |
| `VESSEL_RELAY_TOKEN_FILE` | `vesseld relay --token-file`, the registration token nodes present | required |
| `VESSEL_RELAY_CLIENT_ADDRESS_HEADER` | `vesseld relay --client-address-header` | unset: the peer's address |
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
`Cargo.toml`.

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
