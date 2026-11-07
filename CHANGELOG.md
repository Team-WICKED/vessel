# Changelog

Notable changes are grouped by release, newest first. The workspace version is
`0.1.0`; no release has been tagged yet. The changes below are unreleased and
include backend milestones 1–4 ([#2](https://github.com/Team-WICKED/vessel/pull/2)).

## Unreleased

### Added

- **Local sessions:** Discover Claude Code, Codex, Gemini and OpenCode; run
  their terminal interfaces in a PTY or use structured ACP sessions. Attach
  and detach clients, follow output, and read the durable session log after a
  node restart. Live sessions are marked interrupted when the node stops.
- **SSH nodes:** Name and pin remote nodes, install and start `vesseld` on
  first contact, and reconnect an attached session after a connection drops.
  Updates can wait for live sessions to finish. Automatic SSH installation
  supports Linux and macOS remote hosts; SSH authentication requires a key.
- **Relay and pairing:** Reach nodes behind NAT through `vesseld relay` using
  end-to-end encrypted connections. Pair devices with single-use links,
  inspect or revoke their credentials, and use a direct route when available.
- **Harness profiles:** Configure separate accounts or environments for a
  harness. Move a structured Claude Code or Codex session between logged-in
  profiles while retaining its conversation and session history.
- **Fleet sync:** A home node shares instructions and skills with members and
  supplies MCP servers at session launch. Local edits are preserved as
  conflicts, and deleted fleet files stay deleted until restored.
- **Secrets and transfers:** Store MCP secrets per node and copy them to a
  node that grants the required access. Copy or move structured conversations
  and copy profile memory between profiles or fleet nodes; conflicting memory
  files are preserved beside incoming copies.
- **Setup and agent tools:** `vessel init` sets up a machine and can join a
  fleet; `--service` installs a per-user service on Linux or macOS. Session
  agents can use Vessel MCP tools to start and drive sessions across the fleet
  with scoped credentials and limits.
- **Access controls:** Token authentication for local connections, revocable
  device credentials, relay admission limits and trusted-proxy configuration.
- The project is licensed under [PolyForm Strict 1.0.0](LICENSE.md).

### Fixed

- Relay and node connections now close in a way that lets the peer receive the
  WebSocket close code even when unread data remains, including on Windows.
  ([#4](https://github.com/Team-WICKED/vessel/pull/4))

See the [README](README.md) for usage and limitations, and the
[roadmap](docs/ROADMAP.md) for milestone history and planned work.
