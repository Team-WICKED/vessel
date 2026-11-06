# Backend milestone 2: any node, over SSH

Status: proposed (draft 2026-11-06). Starts once milestone 1 is green on all
three CI platforms. Owner: coordinator. Two implementation tracks, as in M1.

Read first: `AGENTS.md`, `docs/ORIGIN.ko.md` (item 1: Zed's lightness and
fast server-side updates; item 2: remote use that is "very convenient"),
`docs/LESSONS.md` (the Zed `remote` rows), ADRs 0001-0006, and the two
proposed ADRs this plan depends on: 0007 (SSH transport) and 0008 (remote
install and update).

## Goal

Roadmap Phase 1, minus the relay:

- `vessel --node gpu-box run claude` works against a machine that only runs
  `sshd`: no open Vessel port and no Vessel installed beforehand. The CLI
  installs the `vesseld` matching its own version, the node starts on first
  use, and the connection runs over `ssh gpu-box -- ~/.vessel/bin/vesseld-<v>
  attach` (ADR 0007).
- When the SSH connection drops, an attached client reconnects and continues
  from its sequence cursor. No record is lost or shown twice, and calls that
  were in flight report that their outcome is unknown.
- `vessel node update gpu-box` installs the new `vesseld` side by side and
  switches to it without ending live sessions unless the user asks for that
  (ADR 0008).
- Nodes have names: a client config lists known nodes (an SSH target or a
  `ws://` URL, plus where the token comes from), and the node's id is pinned
  on first contact, like `known_hosts`.

## Out of scope

Each of these is a later plan or an explicit non-goal:

- The relay and overlay networks, and node-to-node peers with routed calls
  (M3, Paseo's connected hosts).
- Windows as a *remote*: installing and auto-starting `vesseld` there. A
  Windows *client* (`vessel` on Windows driving a Linux or macOS node over
  OpenSSH) is in scope.
- Service-manager units (systemd, launchd). The node is started on demand by
  `vesseld attach`, and a user who wants a boot-time service writes one.
- A session host that keeps sessions alive across node restarts. ADR 0008
  records it as the real fix for "update without interrupting", and why it
  is not part of M2.
- Interactive SSH authentication (passwords, 2FA prompts). M2 runs `ssh` with
  `BatchMode=yes`, so keys or an agent are required, as Paseo requires. An
  askpass bridge in the style of Zed is a candidate for later.
- TLS for `ws://` nodes. It comes with the relay (ADR 0005 amendment).
- Self-update of the `vessel` CLI.

## What M1 gives us

- `vessel-rpc::FrameTransport`: any `Stream<String> + Sink<String>` carries
  protocol 2, so a new byte-stream framing plugs in without touching the
  RPC layer. `NodeClient::over(transport, ..)` already exists.
- Durable, gap-free `EventSeq` cursors, and `SessionStream`, which already
  pages over a gap and resubscribes after `Lagged`. Reconnecting is the same
  move with a new connection, so no replay buffer is needed (ADR 0007).
- `DataDir` (0700, files 0600), `NodeToken::verify` (constant time), and
  `NodeInfo { id, name, version, os, arch, protocol }` in `Welcome`.
- Sessions survive client disconnects. Node shutdown ends them as
  `Interrupted`, and structured ones resume through ACP `session/load`.
  ADR 0008 builds its update policy on this behaviour, which it does not
  change.
- The CLI already sends the local token file only to a loopback `--node`,
  and refuses a relative `--cwd` for a remote one.

## Contract changes (coordinator, before the tracks start)

`vessel-proto` stays frozen for the tracks, as in M1.

1. **Protocol 3.** `PROTOCOL_VERSION` becomes 3; nodes speak 2..=3.
   - `node.stop { when: StopWhen } -> Ack`, where `StopWhen` is `Now` (the
     M1 shutdown order: sessions end `Interrupted`) or `WhenIdle`.
     `WhenIdle` makes the node refuse `session.create` with `unavailable`
     ("the node is draining for an update") and exit once no session is
     live.
   - `NodeInfo.state: NodeState { Serving, Draining }`, an additive field
     that defaults to `Serving` when absent.
   - A client that negotiated 2 never calls `node.stop`.
2. **`ProxyExit`** (new module `vessel_proto::proxy`): the exit codes of
   `vesseld attach`, a closed enum with `code()` and `from_code()`.
   - `0 Closed`
   - `80 NodeUnavailable`: no node answered, and starting one failed
   - `81 DataDirUnusable`: the data dir or token is unreadable, or the dir
     is not private
   - `82 Refused`: the node at the socket refused the token
   - `127` comes from the remote shell, not from us. Clients read it as "not
     installed".
   - Codes 1, 2, 126 to 255 belong to Rust, clap, the shell, signals and
     `ssh`, and are never used. ADR 0007 has the reasoning.
3. **Byte-stream framing** (documented in `PROTOCOL.md`, implemented in
   `vessel-rpc`):
   - Each frame is a u32 big-endian length followed by that many bytes of
     UTF-8. The JSON is the same as a websocket text frame.
   - The maximum frame size is 16 MiB, checked before allocating.
   - On the node's local socket only, the first frame is the token (see
     ADR 0007), followed by the usual `hello`.

## Crates (ADR 0001)

| Crate | Change | New edges |
| --- | --- | --- |
| `vessel-rpc` | `framed`: `Framed<S: AsyncRead + AsyncWrite>` is a `FrameTransport` over any byte stream (`tokio-util` `codec` feature), with a bounded frame length | none |
| `vessel-store` | `DataDir::{socket_path, lock_path, log_dir, bin_dir}`. `NodeLock` is an exclusive lock on `vesseld.lock` (std `File::try_lock`, stable since 1.89), whose content is a typed record of pid, executable path, version and start time | none |
| `vessel-node` | `ipc`: the local socket (Unix socket 0600 inside the 0700 data dir; a named pipe on Windows), which checks the token preamble and then serves protocol 2/3 over `Framed`. Subcommands `attach`, `version` and `cleanup`. `--bind none` (SSH-only node). `node.stop` and draining. The single-node lock | none |
| `vessel-client` | `Connector` (two implementations, websocket and SSH, so a real extension point), reconnecting follow, node addresses and the named-node book (layered config) | none |
| `vessel-remote` (new) | Running `ssh`, remote OS/arch detection, install, update and cleanup orchestration, and `SshConnector` (a `Connector`: `ssh ... vesseld attach`, `Framed` over the child's stdio, `ProxyExit` mapping) | **new crate: `-> proto, rpc, client`** |
| `vessel-cli` | Named nodes, `vessel nodes ...`, `vessel node install / update / status`, attach that survives reconnects | **`-> remote`** |
| `vessel-testkit` | `fake-ssh`: runs the remote command locally under a fake `$HOME`, can inject a disconnect | none (dev-only) |

ADR 0001 and `docs/ARCHITECTURE.md` gain `vessel-remote -> proto, rpc,
client` and `vessel-cli -> remote` in the same change that adds the crate.
`vessel-remote` uses `tokio::process` directly: it starts `ssh`, a single
process, so it needs no process groups and no `vessel-proc` edge.

## Seams

### `vessel-rpc::framed`
```rust
pub struct Framed<S> { /* tokio_util::codec::Framed<S, LengthDelimitedCodec> */ }
impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> Framed<S> {
    pub fn new(stream: S, max_frame: NonZeroUsize) -> Self;
}
impl<S> FrameTransport for Framed<S> { type TransportError = FramedError; }
// Also usable on split halves: Framed::from_parts(reader, writer, max_frame)
// for a child's stdout/stdin.
```

### `vesseld` command line (Track N owns it; Track C calls it)
```text
vesseld [--bind ADDR|none] [--data-dir DIR] [--name N]   run in the foreground (M1)
        [--agent-handshake-timeout SECONDS]   ACP handshake deadline, default 60 (N8)
vesseld attach [--data-dir DIR]     proxy: stdio <-> local socket; exits with ProxyExit
vesseld version                     one line: "<version> <target-triple>"
vesseld cleanup [--data-dir DIR] [--keep N]   delete unused bin/vesseld-* (ADR 0008)
```
`attach` starts the node if none answers. It runs `current_exe` in its own
process group with stdin from `/dev/null` and its log in `logs/`. It waits
for the node's readiness line on a pipe; it does not poll for a socket file.
The node ignores `SIGHUP` when started this way.

### Remote layout
```text
~/.vessel/                 data dir, 0700 (M1)
  bin/vesseld-0.2.0                  release build, named by version
  bin/vesseld-0.2.0-dev-3fa9c2d1e4b5a6f7   uploaded local build, named by content hash
  vesseld.sock             local socket (Unix), 0600
  vesseld.lock             held by the running node: pid, executable, version
  logs/vesseld.log         log of a node started by `attach` (size-capped, one rotation)
```

### `vessel-client`
```rust
pub trait Connector: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    fn connect(&self, client: ClientInfo) -> impl Future<Output = Result<NodeClient, Self::Error>> + Send;
}
pub struct WsConnector { url: NodeUrl, token: BearerToken, config: ClientConfig }
/// A session stream that redials through `connector` and resumes from its
/// cursor; reconnects show up as non-fatal `StreamError::Reconnecting`
/// and `StreamError::Resumed` items. Capped exponential backoff.
pub fn follow_resuming<C: Connector>(connector: C, session: SessionId, from: EventSeq) -> ResumingStream<C>;
pub enum NodeAddress { Ws(NodeUrl), Ssh(SshTarget) }   // plain data
pub struct SshTarget { pub host: String, pub user: Option<String>, pub port: Option<u16>,
                       pub data_dir: Option<RemotePath> }
```

### The node book (named nodes)
- Settings are layered in Zed's way: content structs whose fields are all
  `Option`, merged into a resolved struct. The order is built-in defaults <
  user file (`<config_dir>/vessel/nodes.toml`) < project file
  (`.vessel/config.toml` in the working directory or its parents) <
  environment (`VESSEL_NODE`, `VESSEL_TOKEN`) < flags.
- The project layer may only set `default_node`, and only to a name that the
  user layer defines. A cloned repository cannot add a node, change a
  transport, or name a token source. Otherwise it could steer commands or
  tokens to a host of its choosing.
- An entry looks like this:
  ```toml
  [nodes.gpu-box]
  ssh = "me@gpu-box"
  id = "6f1c…"
  ```
  A `ws://` node uses `url = "ws://10.0.0.5:7077"` and
  `token = { file = "~/.vessel/tokens/lab" }`. `id` is written on first
  contact. An SSH node needs no token (ADR 0007).
- Entries are typed with `deny_unknown_fields`, and the `toml` crate is added
  for them (justified in the PR: the config is hand-edited, and TOML is what
  people expect from Rust tools).
- `--node X` resolves in this order: a name in the book, then an
  `ssh://[user@]host[:port]` URL, then today's `NodeUrl` forms.

## Track N: node side

Owns `vessel-rpc`, `vessel-store`, `vessel-node`, `vessel-testkit`.

- **N1. `vessel-rpc::framed`**, delivered first because Track C needs it.
  - Round-trip and size-limit tests.
  - A header announcing more than the limit fails before anything is
    allocated.
  - A frame split across reads is reassembled.
- **N2. Store layout and the single-node lock.**
  - `NodeLock::acquire` fails with a typed error that names the holder's pid
    and version.
  - The node takes the lock before it opens the store, which also stops two
    nodes from sharing one data dir (possible in M1).
- **N3. Local socket** (`vessel-node::ipc`).
  - The listener is created 0600 inside the 0700 dir.
  - The token preamble is checked with `NodeToken::verify`; a wrong or
    missing token closes the connection without serving it.
  - After the preamble, the same `vessel-rpc` server and handler serve the
    connection as on the websocket path, with the same in-flight limits.
  - Socket paths longer than the OS limit (about 104 bytes) fail with a
    clear error naming the path.
  - Windows uses `tokio`'s named pipes. The token makes the default pipe
    ACL acceptable (ADR 0007).
- **N4. `vesseld attach`.**
  - It connects, sends the token preamble, then copies bytes both ways
    without decoding them.
  - When no node answers, it starts one detached, waits for the readiness
    line, then connects.
  - It exits with `ProxyExit`. Its stderr carries one human line per
    failure.
  - Stdin EOF, socket EOF or an error ends both directions.
- **N5. `vesseld version`, `vesseld cleanup`, `--bind none`.**
  - `cleanup` keeps three binaries: the one the lock names (the running
    node), the newest one, and the one running the cleanup. It deletes the
    other `bin/vesseld-*` files (ADR 0008).
- **N6. `node.stop` and draining** (protocol 3).
  - `WhenIdle` sets `NodeState::Draining` and refuses new sessions.
  - The node exits after the last live session's final status is committed,
    using the M1 shutdown order with nothing left to interrupt.
  - `Now` is the M1 shutdown.
- **N7. `fake-ssh`** (testkit).
  - It accepts the `ssh` options Track C emits, ignores the host, and runs
    the command with `sh -c` under `HOME=$FAKE_SSH_HOME`.
  - It passes stdio and the exit code through.
  - `FAKE_SSH_DROP_AFTER` (bytes) cuts both directions, simulating a dropped
    connection.
  - A missing command gives 127, as a real shell does.
- **N8. ACP handshake timeout** (added by the coordinator, 2026-11-06).
  `AgentSetup::handshake_timeout` (Track R) is set from
  `RuntimeConfig::agent_handshake_timeout`, node `Config`, and `vesseld
  --agent-handshake-timeout` (`VESSEL_AGENT_HANDSHAKE_TIMEOUT`), default
  60 s. A node started by `attach` inherits the variable from the login
  environment.
- **N done when:**
  - An in-process test reaches a node through `vesseld attach` started under
    `fake-ssh`, including the node auto-start.
  - A test shows two `attach` processes racing to start the node produce
    one node.
  - The draining test passes (a live session keeps running and new ones are
    refused).
  - Unix socket permissions are tested.
  - The gate passes on all three platforms; on Windows the named-pipe path is
    compiled and unit-tested.

## Track C: client side

Owns `vessel-client`, `vessel-cli`, the new `vessel-remote`, and the release
workflow.

- **C1. `Connector` and the resuming follow.**
  - Backoff runs from 0.5 s to 30 s with jitter.
  - A dropped connection fails calls in flight with `Disconnected`, which
    the CLI shows as "may or may not have been applied".
  - Nothing that changes state is retried automatically. Reads and
    subscriptions resume.
  - Tests use an in-memory connector that drops on command.
- **C2. `vessel-remote::ssh`.**
  - The command is built from fixed words and single-quoted paths (ADR
    0007).
  - Options: `-T`, `-o BatchMode=yes`, `-o ServerAliveInterval=15`,
    `-o ServerAliveCountMax=3`.
  - On Unix, `ControlMaster=auto` with a `ControlPath` in a 0700 runtime
    dir, so an install's several commands share one login. Windows OpenSSH
    has no ControlMaster, so there each command logs in again.
  - stderr is captured, keeping the last 64 lines for errors.
- **C3. Platform detection.**
  - `uname -sm` is sent to `sh -s` on stdin. Only the last line is parsed,
    so login banners do not break it.
  - It maps to a target triple: Linux `x86_64`/`aarch64` (musl) and macOS
    `x86_64`/`aarch64`. Anything else is `Unsupported` with the raw output.
- **C4. Install** (ADR 0008).
  - The installer:
    1. resolves the artefact for the target;
    2. downloads it on the remote (`curl`, else `wget`), or downloads it
       locally and pipes it over `ssh` stdin when the remote cannot;
    3. checks it against signed checksums;
    4. moves it into place with `chmod 755` and `mv` on the same filesystem;
    5. runs `vesseld-<v> version` to confirm, then `cleanup`.
  - For a local build on the same target, it uploads a content-addressed
    `-dev-<hash>` binary instead.
- **C5. `SshConnector`.**
  - It runs `ssh … '~/.vessel/bin/vesseld-<v>' attach` and wraps the child's
    stdio in `Framed`.
  - Exit codes map to typed errors: 127 means install and retry once; 80, 81
    and 82 are shown as they are; 255 is an `ssh` failure, shown with its
    stderr.
  - It pins `NodeInfo.id` against the book.
- **C6. CLI.**
  - `--node` resolution and `vessel nodes add|list|remove|show`.
  - `vessel node install|update|status <name>`.
  - `update` installs, then:
    - with live sessions and no flag, it lists them and stops;
    - `--when-idle` sends `node.stop{WhenIdle}`;
    - `--now` sends `node.stop{Now}` after confirming.
  - Attach shows `[vessel] connection lost; reconnecting…` and resumes in
    place.
- **C7. Release workflow.**
  - On a `v*` tag, CI builds `vesseld` for the four targets, using native
    arm64 runners where GitHub offers them and `cargo-zigbuild` for musl.
  - It publishes `vesseld-<version>-<target>.tar.gz`, a `SHA256SUMS` file
    and its signature (ADR 0008).
- **C done when:**
  - Tests with no network, through `fake-ssh` and a temporary "remote
    home":
    - a fresh remote gets the binary by upload and starts on first
      `attach`;
    - `vessel --node test run` then works end to end;
    - an injected drop during a terminal attach resumes gap-free;
    - a second version installs side by side, and `cleanup` keeps the
      running one;
    - `update` refuses with a live session and drains with `--when-idle`;
    - a pinned id that changed is refused.
  - The book's layering is unit-tested, including the project-layer
    restrictions.

## Done criteria (milestone)

- The gate passes on Linux, macOS and Windows. Windows runs the client-side
  tests; the remote-side tests are Unix-only and say why.
- Manual smoke by the coordinator against a real Linux host over real SSH:
  install from a release, run, drop the network, resume, update while
  draining, clean up.
- `PROTOCOL.md` has a version 3 entry. ADRs 0007 and 0008 are accepted.
  `ARCHITECTURE.md` and `README.md` describe remote use.

## Decisions (coordinator, 2026-11-06)

1. **Release source:** for M2 there is no remote download. `vessel`
   fetches the release asset locally through the authenticated GitHub API
   (the repository is private), or uses a local build, and uploads it over
   `ssh` stdin. It checks the SHA-256 published with the release, then
   renames atomically. Downloading directly on the remote, and the minisign
   key, wait until the owner decides whether releases are public; that is
   an outward-facing decision the plan does not make.
2. **Auto-start:** yes. `vesseld attach` starts the node when none answers.
   This serves ORIGIN item 3, a new machine productive in minutes.
3. **Update with live sessions:** refuse by default and list them.
   `--when-idle` and `--now` are explicit.
4. **Linux binaries:** static musl, as Zed ships its remote server. If
   allocator speed ever shows up in profiles, revisit with an explicit
   allocator rather than glibc.
5. **Nodes under the user's own service manager:** as proposed. `update`
   sends `node.stop` and starts nothing.
6. **Windows remotes:** out of M2. Windows clients stay in.

The contract changes listed above are on `backend/m1` (protocol 3, commit
after a5832cc). Until Track N implements it, the node answers `node.stop`
with `unsupported`.

### Open decisions as drafted (kept for the record)

1. Release source and signing key. The plan assumes GitHub Releases on
   `Team-WICKED/vessel` and a minisign key, with the public key compiled
   into `vessel`. If the repository stays private, remote downloads need a
   token, and the fallback becomes "download locally (with `gh`) and
   upload".
2. Whether `vesseld attach` should auto-start the node at all. The proposal
   is yes. The alternative, Paseo's, is "SSH only reaches a node you
   started".
3. Whether the update default with live sessions is to refuse (proposed)
   or `--when-idle`.
4. Linux binaries as static musl (proposed; any glibc works) or gnu (faster
   allocator, but tied to the remote's glibc).
5. Whether `vessel node update` may restart a node that was not started by
   `attach` (a user's own service). The proposal is to stop it with
   `node.stop`, start nothing, and tell the user that their service manager
   restarts it.
