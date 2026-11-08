# Tutorial: run a local session

This tutorial starts a node on your machine, runs an installed harness and
reattaches to its session. It uses Claude Code as an example; Vessel also
recognizes Codex, Gemini and OpenCode. The harness must already be installed
and usable on the machine running the node.

## Build and start the node

Install Rust 1.93 with `rustfmt` and `clippy` (see
[rust-toolchain.toml](../rust-toolchain.toml)), then from the repository root:

```bash
cargo build --release
./target/release/vessel node start
```

Keep the node running. In another terminal, from the same directory:

```bash
./target/release/vessel status
./target/release/vessel harnesses
```

`status` shows the node and detected harnesses. `harnesses` lists harness
availability. Local commands read the node's token from its data directory.

## Run and reattach

```bash
./target/release/vessel run claude
```

This opens the harness's terminal interface. Press `Ctrl-]` to detach without
stopping the session. Then list sessions and reattach using the id, or a unique
prefix, shown by the list:

```bash
./target/release/vessel sessions
./target/release/vessel attach <session-id>
```

The placeholder `<session-id>` must be replaced with an actual id; do not type
its angle brackets. After detaching again, `vessel log <session-id>` prints the
session log and `vessel kill <session-id>` stops the session. Stop the
foreground node with `Ctrl-C` in its terminal. Stopping the node interrupts
live sessions, while their logs remain available after a restart.

For structured ACP sessions, account profiles, remote nodes, pairing and
fleets, see the [README](../README.md). Run `vessel --help` or
`vessel <command> --help` for current command options.
