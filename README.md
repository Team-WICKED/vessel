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

Early. What works today:

- `vesseld` runs on one machine, discovers installed harnesses, spawns them,
  and streams their output over HTTP + WebSocket.
- `vessel` CLI talks to a node: status, harness list, run, attach, send, kill.

Nothing about peers, relay, or sync exists yet. See
[docs/ROADMAP.md](docs/ROADMAP.md).

## Quick start

```bash
cargo build --workspace
```

Start a node (foreground):

```bash
cargo run --bin vesseld
```

In another shell:

```bash
cargo run --bin vessel -- status
cargo run --bin vessel -- run claude-code -- --version
```

`run` prints the session id, then streams output until the harness exits.
Use `--detach` to return immediately and `attach <id>` to come back later.

## Layout

| Crate | What |
| --- | --- |
| `crates/vessel-core` | Shared types and wire protocol. No I/O. |
| `crates/vessel-node` | The node daemon (`vesseld`) and its library. |
| `crates/vessel-cli` | The `vessel` command-line client. |

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly those three on Linux, Windows and macOS. Lints are strict
(no `unwrap`, no panics, no indexing, no raw arithmetic); see the workspace
`Cargo.toml`.
