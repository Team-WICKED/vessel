# Vision

Vessel is an IDE-shaped **Agentic Development Environment**. It should work as
an IDE when you want one, and as a plain remote vibe-coding console when you
do not, while managing many machines from one seat.

It borrows from five places.

## 1. Zed: lightweight

Zed is fast because it is native Rust with a GPU-rendered UI, not because of
anything server-side. Its remote-development mode does run a headless server
on the far machine and connects an editor to it, which is close to the shape
Vessel wants. Vessel takes the lesson, not the code: Zed is GPL-3, and Vessel
stays independent so its own licence is free to choose.

## 2. Paseo: an excellent wrapper

Paseo wraps well-known harnesses (Claude Code, Codex) without replacing them,
makes remote access painless, ships a relay so machines reach each other
without a VPN, and has its own plugin layer. Vessel's node daemon plays the
same role, and the relay should be built on an existing overlay (Tailscale's
`tsnet`, or raw WireGuard) rather than invented.

## 3. GOAT: the fleet

Things the GOAT project wanted and Vessel inherits:

- **First-run setup** that gets a new machine productive in minutes.
- **Sync and migration** of skills, MCP servers, memory, and conversation
  history between machines.
- **Every agent sees every other agent.** This is the point of a fleet:
  - delegate across machines (build on the Linux box, test on the Mac, run
    the local model on the GPU rig);
  - split one task queue across nodes;
  - cross-review each other's work;
  - share one memory and one set of tools.
  It is also where the hard part lives: identity, authorization, and what one
  agent is allowed to do on another node.

## 4. opencodex: any model in any harness

Every harness should be able to use every model, through a routing proxy the
node runs locally. opencodex already does this; Vessel embeds that behaviour
in the node.

## 5. JetBrains: IDE-grade context for the model

JetBrains IDEs hand the model what only an IDE knows: symbols, references,
diagnostics, call hierarchies, project structure. Most of this is available
from LSP servers. The node acts as an LSP client and exposes the result to
agents as MCP tools, so the context exists regardless of which editor (or no
editor) is attached.

## Order of attack

1. Node daemon + CLI (this repo today).
2. Relay and peer discovery.
3. Sync of skills, MCP, memory, history.
4. Model routing proxy inside the node.
5. LSP-to-MCP context bridge.
6. Web UI, then editor integrations, then (maybe) a native editor.

Building the editor first would sink everything else. The daemon is the
product; editors are clients.
