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

Not yet: relay, TLS, fleet sync, harness profiles, model routing, the LSP
bridge. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Try it

```bash
cargo build --release
./target/release/vessel node start          # or ./target/release/vesseld
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

### Environment

| Variable | Means | Default |
| --- | --- | --- |
| `VESSEL_NODE` | `--node`: a book name, `ssh://[user@]host[:port]`, `user@host`, or a websocket address | the project's or the book's `default_node`, else `127.0.0.1:7077` |
| `VESSEL_TOKEN` | `--token`, for a websocket node | the book entry's token file; for a node on this machine, `~/.vessel/token` |
| `VESSEL_DATA_DIR` | `--data-dir`, this machine's data directory (the node's state; `ssh/` for shared logins) | `~/.vessel` |
| `VESSEL_CONFIG_DIR` | where the node book lives (`<dir>/vessel/nodes.toml`); tests point it at a temporary directory | the platform's config directory (`~/.config` on Linux) |
| `VESSEL_SSH` | the `ssh` program to run (tests use `fake-ssh`) | `ssh` on `PATH` |
| `VESSEL_GH` | the GitHub CLI used to download releases | `gh` on `PATH` |

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
