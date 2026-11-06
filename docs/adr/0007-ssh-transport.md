# ADR 0007: Reaching a remote node over SSH stdio

Status: accepted, 2026-11-06, with the coordinator decisions in `docs/plans/backend-m2.md`. Drafted (milestone 2, `docs/plans/backend-m2.md`)

## Context

In milestone 1 a client reaches a node only through its websocket (`/rpc`,
bearer token, ADR 0005), on loopback or through a port the user forwarded
by hand. ORIGIN item 2 asks for remote use that is very convenient. Most
machines a developer wants to drive already accept SSH, with keys, agents,
`~/.ssh/config`, jump hosts and `known_hosts` set up. Requiring an open
Vessel port, or a tunnel the user maintains, throws that away.

Two working designs exist (read 2026-11-06; ideas only):

- **Zed** (`crates/remote`, `crates/remote_server`). A long-lived server
  per workspace binds three Unix sockets (stdin, stdout, stderr). Each SSH
  connection runs a thin `proxy` that copies length-prefixed frames, as
  bytes, between its stdio and those sockets.
  - The proxy starts the server when it finds none. It checks liveness
    through a pid file, polls every 20 ms for the socket files to appear,
    and kills an existing server on a non-reconnect attach.
  - Exit code 90 means "server not running". There is no auth beyond the
    SSH login.
  - An unbounded replay buffer with acks covers drops, and a ping every 5 s
    detects dead links.
  - The server exits after 10 idle minutes.
- **Paseo**. SSH only tunnels to a daemon the user already started, at
  `127.0.0.1:6767` on the remote. It never installs, starts or configures
  anything.

What Vessel already has shapes the choice:

- `FrameTransport` accepts any framed stream.
- Session state lives in the node, not in the connection.
- Durable `EventSeq` cursors make resuming a read-only problem (ADR 0006).
- Every connection presents the node's token (ADR 0005: one rule, no "safe
  because local").

## Options

1. **Websocket through `ssh -W 127.0.0.1:7077` (Paseo).** Almost no new
   code: the websocket handshake runs over the forwarded channel.
   - The client needs the remote token, so it would read it with `ssh host
     cat ~/.vessel/token`, and the secret leaves the remote.
   - The node must listen on TCP.
   - Many hardened `sshd` configs set `AllowTcpForwarding no`.
   - There is no way to start the node, and failures arrive as a generic
     "channel open failed".
2. **A thin proxy over the SSH session's stdio, bridging to the node's
   local socket (Zed's split).** Needs a local listener, a byte-stream
   framing and the proxy.
3. **A Rust SSH client (`russh`) inside `vessel`.** Full control, but it
   re-implements what users configured in OpenSSH (agents, `ProxyJump`,
   `known_hosts`, `Match` blocks, hardware keys) and owns that attack
   surface.
4. **A persistent `ssh -L` port forward managed by the CLI.** Combines the
   costs of option 1 with a background process to babysit.

## Decision

Option 2, run through the system `ssh` client:

- **Topology.** One `vesseld` per data directory, as today. It also listens
  on a local socket: `<data_dir>/vesseld.sock` on Unix (0600, inside the
  0700 dir), or a named pipe on Windows.
  - `vesseld attach` is started per connection by `ssh -T host --
    '~/.vessel/bin/vesseld-<version>' attach`.
  - It connects to the socket and copies bytes between its stdin/stdout and
    the socket without decoding them.
  - `--bind none` runs a node with no TCP listener at all, reachable only
    through SSH and the local socket.
- **Starting the node.** If nothing answers, `attach` starts
  `current_exe` detached: its own process group, stdin from `/dev/null`,
  its log in `logs/`, and `SIGHUP` ignored.
  - It then waits for a readiness line the node writes to a pipe once it is
    listening, so there is no polling for files.
  - The node holds an exclusive lock on `vesseld.lock` (std
    `File::try_lock`) for its lifetime. Two racing `attach` processes
    therefore produce one node, and the loser connects to the winner.
  - Liveness is decided by the lock and by the socket answering, never by a
    pid.
  - The node does not exit when idle: sessions run whether or not anyone is
    attached.
- **Framing.** Each frame is a u32 big-endian length, then UTF-8 JSON
  identical to a websocket text frame. The maximum is 16 MiB, checked
  against the header before any allocation (`vessel-rpc::framed`, the
  `tokio-util` length-delimited codec). The same framing is used on the
  socket and on SSH stdio, which is why the proxy can stay a byte pump.
- **Authentication.** The SSH login authenticates the person, and the
  node's token still authenticates the connection.
  - `attach` runs as the node's user, reads `<data_dir>/token` and sends it
    as the socket's first frame. The node checks it with
    `NodeToken::verify` before serving.
  - The token never leaves the remote machine, and the client needs none
    for an SSH node.
  - Locality is not trusted: another local user who can reach the socket
    (for example through the default ACL of a Windows named pipe) still
    needs the token. This keeps ADR 0005's single rule without
    OS-specific peer-credential code (`SO_PEERCRED` has no safe portable
    equivalent for Windows pipes).
- **Exit codes as a typed side channel** (`vessel_proto::ProxyExit`):
  - `0` the connection ended;
  - `80` no node answered and none could be started;
  - `81` the data dir or token is unusable;
  - `82` the node refused the token (another node's socket);
  - `127` comes from the remote shell, which the client reads as "this
    version is not installed": it installs (ADR 0008) and retries once;
  - `255` is `ssh`'s own failure, reported with its stderr.

  Codes that Rust (1), clap (2), the shell (126, 127), signals (128+n) or
  `ssh` (255) already use are avoided. Proxy stderr carries one human line
  per failure, and the client keeps its last 64 lines for messages.
- **SSH invocation.** The client runs the system `ssh` binary with `-T -o
  BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3`, plus
  the user's `~/.ssh/config`.
  - On Unix it adds `ControlMaster=auto` with a `ControlPath` in a 0700
    runtime dir, so the several commands of an install share one login.
  - The remote command is built only from fixed words and single-quoted
    paths; no user text is interpolated.
- **Reconnect.** There is no replay buffer.
  - A drop fails the calls in flight with `Disconnected`, which means the
    outcome is unknown. State-changing calls are never retried
    automatically.
  - Reads and subscriptions resume: the client redials with backoff from
    0.5 s to 30 s with jitter, and every follower resubscribes from its
    cursor (the same path `SessionStream` takes after `Lagged`).
  - Dead links are detected by SSH's keepalive (about 45 s); protocol 3
    adds no ping.

## Why

- **No open port, nothing to start by hand.** One `ssh` command reaches a
  node, which matches what both ORIGIN and Zed's experience call
  "convenient". With `--bind none`, a remote node has no listening port at
  all.
- **The system `ssh` brings the user's whole SSH setup for free:** agents,
  jump hosts, known hosts, hardware keys. Vessel adds no SSH protocol code
  and no new attack surface.
- **Reconnect needs no buffer.** Zed buffers every message until it is
  acknowledged (unbounded) because its state lives in the stream. Ours
  lives in the node, with durable cursors, so a drop only costs re-reading
  from the cursor.
- **Fewer moving parts than Zed's design.** One socket, not three. A lock,
  not a pid file. A readiness pipe, not polling. No idle exit. No killing an
  existing server.
- **The token rule stays uniform.** The proxy proves it can read the token
  file, exactly the capability ADR 0005 makes the credential.

## Consequences

- New code: the `framed` transport in `vessel-rpc`; the local socket and
  `attach` in `vessel-node`; the `ssh` runner and `SshConnector` in a new
  crate `vessel-remote` (`-> proto, rpc, client`; ADR 0001 and
  `ARCHITECTURE.md` gain that edge and `vessel-cli -> remote`); and
  `fake-ssh` in the testkit, so tests never touch the network.
- Unix socket paths have a length limit of about 104 bytes. A data dir with
  a very long path fails with an error that names the path; a fallback
  under `$XDG_RUNTIME_DIR` is a later option.
- Password and 2FA logins do not work (`BatchMode`) until an askpass bridge
  exists.
- A node started by `attach` has no service manager watching it. A crash
  leaves it down until the next `attach` starts it again, which marks its
  sessions `Interrupted`, as any restart does.
- Protocol 3 is needed only for `node.stop` and `NodeInfo.state` (ADR 0008).
  The transport itself changes no wire type: it carries the same frames.
