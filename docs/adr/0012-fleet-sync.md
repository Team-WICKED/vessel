# ADR 0012: Fleet sync and joining a fleet

Status: accepted 2026-11-06 with the coordinator decisions in `docs/plans/backend-m4.md`; drafted as proposed, 2026-11-06 (milestone 4, `docs/plans/backend-m4.md`).

## Context

A fleet is the machines one person runs agents on. The vision (VISION
section 3) asks that skills, MCP servers, memory and conversation history
follow the person from machine to machine, and that a new machine becomes
productive in minutes (`vessel init`).

### What the operator does today (read 2026-11-06, read-only)

On one machine, five agent configurations (Codex and four Claude Code
logins) share one set of global instructions and one set of skills
(the operator's global instructions and their `agent-config-sync` script):

- **One source, links everywhere.** Instructions live in `~/.codex/AGENTS.md`;
  `~/.claude/CLAUDE.md` is a symlink to it. Skills live in
  `~/.codex/skills/<dir>`; every Claude configuration has one symlink per
  skill, named by the `name` in its `SKILL.md`.
- **A watcher keeps it true**: a systemd path unit on the skill directories
  and a 10-minute timer run an idempotent script under one lock. A skill
  created on a Claude side is moved into the source and linked back.
- **Conflicts never overwrite the source**: the same name with different
  content keeps the source, backs the copy up, and writes a line to
  `CONFLICTS.log` for a person to settle.
- **Not synced, on purpose**: memory, `settings.json`, subagents (per
  account); skills that another product manages (Paseo copies its own and
  refuses symlinks; marked by `.paseo-managed-files.json`); claude.ai
  account skills.
- **Conversations move between accounts** by hard-linking one session's
  files, never `memory/` (ADR 0011).

This works for one machine. Nothing carries it to a second one.

### What Paseo does (installed daemon 0.15.1, read 2026-11-06)

- Nothing is synced between hosts. Connected hosts route sessions to other
  daemons; the Hub runs "executions" on enrolled daemons; `directory-sync`
  only versions the lists the daemon sends its own app clients.
- Paseo's own skills are copied (not linked) into `~/.agents/skills` and
  `~/.claude/skills`, each with `.paseo-managed-files.json` (file → SHA-256).
  On update it overwrites a changed file even if the user edited it, and
  deletes only files whose hash still matches; it refuses to copy through
  symlinks (`orchestration-skills/internal/sync.js`).

### What the harnesses keep where (checked 2026-11-06)

- Claude Code (`CLAUDE_CONFIG_DIR`, default `~/.claude`): `CLAUDE.md`,
  `skills/`, `settings.json`, transcripts in `projects/<cwd as a name>/
  <session>.jsonl`, memory in `projects/<…>/memory/`, and user-scope MCP
  servers in `.claude.json`, a file that also holds the OAuth account and
  caches the CLI rewrites often.
- Codex (`CODEX_HOME`, default `~/.codex`): `AGENTS.md`, `skills/`,
  `config.toml` with `[mcp_servers.<name>]` tables beside model settings,
  `sessions/YYYY/MM/DD/*.jsonl`, `memories/`.
- So a harness's MCP configuration cannot be synced by copying a file: it
  sits in files that also hold credentials and state the harness owns.

## Options

### Topology

1. **One origin, members pull** (the operator's single-source rule, across
   machines): one node, the fleet's *home*, holds the shared configuration;
   the others fetch it.
2. **Multi-master**: any node edits, changes merge everywhere (last writer
   wins, or a CRDT). Edits on two machines at once are where the trouble
   is, and the operator's rule is that the source is never overwritten
   automatically.
3. **A git repository** the user provides, every node pulls. Good for
   instructions and skills; wrong for secrets, memory and transcripts, and
   it needs a remote everywhere.

### Getting MCP servers to a harness

1. **Write them into the harness's configuration files**, managing only the
   entries Vessel owns. Races with the harness rewriting the same file
   (`.claude.json`), and puts secrets in files.
2. **Hand them to the harness at launch**: ACP's `session/new` takes the
   client's MCP servers, and in terminal mode the harnesses accept them on
   the command line (`claude --mcp-config`, `codex -c mcp_servers…`).
   Nothing is written; secrets stay in the node's process.
3. Both: launch-time where the harness allows it, files otherwise.

### History and memory

1. **Continuous replication** of every transcript and memory file.
   Transcripts are large and grow per turn; memory is deliberately per
   account.
2. **On demand**: move or copy one session to another node when asked;
   copy a profile's memory once when asked.

## Decision

### One home, members pull (topology option 1)

- `vessel fleet init` makes a node the fleet's home. Its `fleet.toml`
  names the sources:

  ```toml
  # <data_dir>/fleet.toml on the home node
  instructions = "~/.codex/AGENTS.md"
  skills = "~/.codex/skills"
  skip = [".system", "paseo*"]        # managed by someone else

  [mcp.servers.docs]
  command = "npx"
  args = ["-y", "@example/docs-mcp"]
  env = { DOCS_TOKEN = { secret = "DOCS_TOKEN" } }
  ```

- The home publishes a **bundle**: a manifest of `path → SHA-256, mode`
  plus the blobs, with a generation number. Members fetch the manifest,
  then only the blobs they lack, over the M3 secure channel (direct or
  relayed). The home notifies members of a new generation through a
  subscription; members also check on start and every 10 minutes.
- To change the fleet's configuration, edit it on the home (where the
  operator's own symlink scheme keeps working untouched), or send a file
  there explicitly with `vessel fleet push`.

### Materialising on each node, per profile

- Each node writes the bundle into every profile's harness home (ADR 0011:
  the descriptor knows `CLAUDE.md`/`AGENTS.md` and `skills/` for its
  harness). Skills are named by the `name` in `SKILL.md`, as the operator's
  script does.
- Copies, not symlinks, because the bundle lives in Vessel's data
  directory. A target that already resolves to the source (the operator's
  links on the home itself) is left as it is.
- Every file Vessel writes is recorded with the hash it wrote
  (`<data_dir>/fleet/managed.json`, the role of Paseo's marker file).
- **Local edits win and are reported, never overwritten**: if a managed
  file changed since Vessel wrote it, the incoming version is written
  beside it (`<name>.vessel-incoming`), the conflict is listed by `vessel
  fleet status`, and a person settles it. Files Vessel did not write are
  never touched.

### MCP servers at launch (option 3)

- Structured sessions get the fleet's MCP servers in ACP `session/new`
  (and `session/load`); the harness receives them over its stdin, as JSON.
- Terminal sessions get them as a per-session file the harness is pointed
  at (`claude --mcp-config <file>`), owner-only in the node's data
  directory and deleted when the session ends.
- **No secret value ever goes on a command line**: arguments are visible
  to every local user (`ps`). Where a harness takes MCP servers only as
  arguments, secrets reach the server through its environment instead.
- Nothing is written into `.claude.json` or `config.toml`.
- Only harnesses with no launch-time way would fall back to managed
  entries in their files; none of the four needs it today.
- The same path attaches Vessel's own MCP server to every session
  (`vessel mcp`, ADR 0013).

### Secrets stay on each node

- The bundle carries references (`{ secret = "NAME" }`), never values. Each
  node resolves them from its own `<data_dir>/secrets.toml` (owner-only)
  when it launches a harness.
- The types make the rules hold (contract step 2a, 2026-11-06,
  `vessel_proto::fleet`): one `McpTransport<V>` shape, where `V` is
  `ConfigValue` (literal text or a secret's name) in definitions and
  `ResolvedValue` at launch. Only environment variables and headers hold a
  `V`, so a secret cannot become an argument; `ResolvedValue` has no serde
  implementation, so a resolved server cannot be serialised onto the wire,
  and its `Debug` is redacted. A reference is a typed table, not
  `${secret:NAME}` inside a string, so a mistyped reference is an error
  rather than a literal.
- `vessel fleet secret set NAME` stores one on a node (read from a prompt or
  stdin, never an argument). `vessel fleet secret copy NAME --to NODE`
  sends one to another node inside the secure channel, explicitly, with
  the admin scope there.
- A session whose MCP server needs a missing secret starts without that
  server and says so in its log.

### History and memory on demand (option 2)

- `vessel session move ID --to NODE [--cwd PATH]` (and `copy`): for a
  structured session whose harness descriptor knows its transcript layout,
  the node exports the transcript files; the target imports them into the
  chosen profile's home under the target's working directory and resumes
  with `session/load`. The new session records where it came from.
- `vessel fleet memory copy --profile P --from A --to B`: a one-time copy
  of a profile's memory files, with conflicting files written beside the
  existing ones. Memory is never synced continuously: the operator keeps it
  per account on purpose.

### Membership and trust

- A node joins by pairing with the home, exactly as a device does
  (ADR 0010), with a new `sync` scope: read the bundle, move sessions in.
  The joining node stores that credential in its data directory.
- Joining is mutual: the new node also issues the home a credential on
  itself, with the scopes its owner grants (by default `read` and
  `operate`, not `admin`). The home can then reach every member, which
  cross-node agent calls use (ADR 0013).
- A member that should not take the fleet's configuration any more is
  revoked on the home like any device.

### Joining from scratch: `vessel init`

On a fresh machine, `vessel init`:

1. prepares the node (data directory, token, key) and, with `--service`,
   installs it as a per-user service (systemd user unit, launchd agent);
2. finds the harnesses, and offers their install commands where missing;
3. proposes profiles for the harness homes it finds (`~/.claude*`,
   `~/.codex*`) and offers each one's login command;
4. with a pairing link (read from stdin or a prompt, never the command
   line: it is a single-use secret, and arguments are visible to every
   local user), joins the fleet, pulls the bundle, materialises it, and
   takes the fleet's relay settings;
5. prints what it did and what is left.

It is idempotent: running it again fills gaps and changes nothing else.

## Why

- **One home** is the operator's rule, proven on one machine, extended to
  many: there is always one answer to "which copy is right", and conflicts
  go to a person, not to a merge algorithm.
- **Copies with recorded hashes** are what lets a member tell its own
  edits from stale copies, and never lose either. Symlinks cannot reach
  across machines.
- **Launch-time MCP** avoids racing harnesses over their own files and
  keeps secrets out of files nobody audits.
- **Secrets by reference** mean the bundle can be fetched by any member
  without handing every machine every credential.
- **On-demand history** moves what someone asked for, the way the
  operator's account switch does, without replicating gigabytes nobody
  reads.
- **Pairing for membership** reuses ADR 0010 instead of inventing a second
  trust scheme.

## Consequences

- Protocol 5: the `sync` scope; `fleet.manifest`, `fleet.blob`,
  `fleet.watch`, `fleet.members`, `fleet.push`; `session.export` and
  `session.import`; `secret.copy` (admin).
- A new crate, `vessel-fleet`: bundle manifests, the materialiser, managed
  files and conflicts. No network: the node moves its data.
- `vessel-harness` descriptors gain each harness's instruction file, skill
  directory, transcript layout and launch-time MCP form.
- The node gains `fleet.toml`, `secrets.toml` and `fleet/` in its data
  directory, all owner-only.
- The home is a single point for configuration changes, not for running
  agents: members keep working, with the last bundle, while it is away.


## As built (Track H, MCP at launch, 2026-11-06)

- **Claude Code** reads the servers from `--mcp-config=<file>`, passed as
  one argument because the option takes several values and would swallow
  the session's own arguments after it. The file is JSON, owner-only,
  created exclusively under a random name in the directory the node gives
  (`<data_dir>/mcp`, owner-only and emptied at start, so a crash leaves
  nothing behind; there is no shared fallback), written right before the
  program starts, and removed when the session ends.
- **Windows (decision, 2026-11-06):** the file is owner-only because it is
  written inside the user's profile directory, whose inherited ACL admits
  only that user, the protection the node's `token` and `node.key` rely
  on; anywhere else it is refused. Trade-off: Vessel sets no explicit
  DACL, since that needs `unsafe` Win32 calls or a Win32 crate. Revisit if
  Vessel ever has to write the file outside the profile directory.
- **Codex** has no file flag that leaves its home alone, so each server is
  one `-c mcp_servers.<name>={...}` override that replaces any server of
  that name whole. Values go into Codex's environment: a stdio server's
  under its own variable names, forwarded with `env_vars`; a header's under
  a generated name (ending in `_SECRET`) read with `env_http_headers`.
  Because that environment is Codex's own, a variable is refused rather
  than silently changing Codex itself when two servers, a server and the
  profile, or a server and the environment Codex inherits would give it
  different values, when the profile removes it, and always for variables
  that steer Codex (`PATH`, `HOME`, `LD_*`, `DYLD_*`, `NODE_OPTIONS`,
  `*_PROXY`, `OPENAI_*`, `CODEX_*`, and the Windows equivalents). Unlike
  Claude Code's file, these values are visible to the commands the agent
  runs unless Codex's environment filter drops them (by default it drops
  names holding `KEY`, `SECRET` or `TOKEN`).
- **Gemini CLI and `OpenCode`**: no launch-time form that leaves the user's
  own configuration alone was confirmed, so their terminal sessions start
  without the servers, and the launch reports which servers were left out.
  Their structured sessions get every server over ACP.
- **ACP**: stdio servers always; HTTP servers only when the agent advertises
  `mcp_capabilities.http`, otherwise left out and logged by name.
- A test asserts that no value appears on any harness command line, in
  `vessel-harness` (the command) and in `vessel-session` (the arguments the
  fakes actually received).
- **Where the servers come from (node side, 2026-11-06).** The node asks an
  `McpSource` (`vessel-node`, `profiles/launch.rs`) once per launch and
  per profile move, with the session's spec. Fleet sync implements it over
  the fleet's definitions and this node's secrets (`vessel_fleet::resolve`)
  and replaces the empty source where the node is put together; the
  source answers from memory, since it is called on the async runtime.
  Secrets it reports missing are named in the session's log, as
  `vessel:` log lines right after `starting` (and after a move), and in
  the node's log; the session starts without the servers that need them.
  The agent's own `vessel` server (ADR 0013) joins the same list.

## As built (Track F, materialiser, 2026-11-06)

- **Deleted stays deleted.** A file Vessel wrote that is missing on the
  node is not written again, even when the bundle changes it: deleting is
  a local edit. It is listed as a conflict of its own kind ("deleted
  locally"), `managed.json` keeps its record, and nothing is written beside
  it. `vessel fleet restore <path>` settles it: the record is dropped and
  the next apply writes the file again. Settling by removing the record by
  hand was not chosen, because it asks people to edit `managed.json`.
- **Unknown bundle paths are ignored.** Only `instructions.md` and
  `skills/<name>/...` are placed (PROTOCOL.md, version 5); a member skips
  any other top-level path without a failure, so a newer home does not
  stop an older member.
- **Owner-only.** Members write every fleet file 0600 (0700 when
  executable) and every directory they create 0700, whatever the
  source's mode: a manifest carries only the executable bit, so a member
  can never make a file readable by more people than the home did. Dot
  entries inside a skill (`.env`, `.git/`) are not bundled.
- **Safety of the materialiser** (after an independent review): records
  are kept by canonical target root, so a profile linked to another
  home is written once and never loses files to the other's name; a
  file is removed only if it is still a regular file with the recorded
  content, was not adopted (a file that was the user's before Vessel
  came is released, never removed), and is not the same file as one the
  bundle places; places are reached through open directories without
  following a symlink, and every step looks again right before acting;
  a harness home that does not exist is skipped, not created.
- **Home and members on the node (F2).** `fleet.toml` takes MCP servers
  in the manifest's own shape (`[mcp.servers.<name>]` with `type =
  "stdio"` or `"http"`), so the file and the protocol cannot drift; paths
  are absolute or start with `~/`. The home notices edits by polling a
  metadata stamp of its sources every few seconds and bundles only when
  it changed (and every 10 minutes), instead of the subscription this ADR
  first named: `fleet.watch` is a long poll (protocol 5), and a file
  watcher was not taken (a new dependency; unreliable through symlinked
  layouts). The home also applies the bundle to its own profiles, its
  sources and the homes linked to them left alone. Membership is a
  pairing with a `sync` link; the credential and the home's routes are
  files in the member's data directory, written by `vessel fleet join`
  and noticed by a running node within seconds. `vessel fleet restore`
  forgets the records and leaves `fleet/apply`, which the node acts on.
- **Secrets on the node (F3).** `<data_dir>/secrets.toml` is a flat
  table, `NAME = "value"` (names by the environment variable rule),
  owner-only. Errors never show a value: a file that does not parse is
  reported by line only, since the TOML parser quotes the line. `vessel
  fleet secret set NAME` takes the value from a prompt that does not echo
  or from standard input; `list` prints names; `delete` removes one. They
  edit the file through `toml_edit` under the fleet lock, keeping its
  comments; the node notices a change within seconds. There is no way to
  read a value back out, and protocol 6's `secret.set`, `secret.list` and
  `secret.delete` (admin) will call the same functions; `secret.copy`
  waits for protocol 6.
- **MCP servers at launch (F3).** The node's `McpSource` is the fleet's
  (`fleet::FleetMcp`): a snapshot of the definitions of the manifest this
  node last applied (or published, on the home) and of `secrets.toml`,
  replaced by the fleet loop and answered from memory. Every session gets
  every server; one whose secret is missing is left out and named in the
  session's log. A node that leaves the fleet stops giving its sessions
  the fleet's servers. `vessel fleet status` lists the servers and the
  secrets missing here.
- **Not built in F2:** the mutual grant (a member issuing the home a
  credential on itself) and `fleet.push` need calls protocol 5 does not
  have; they come with cross-node calls (Track X) and protocol 6.
- **`OpenCode`** also reads `~/.claude/CLAUDE.md` and `~/.claude/skills`,
  so on a node whose default Claude Code home is a target it would see the
  fleet's files twice; the node leaves `OpenCode` out there.

## As built (`vessel init`, F5, 2026-11-06)

- Steps 1 to 3 and 5 are built; step 4 (`--join`) refuses, changing
  nothing, until fleet sync (F2).
- Consent: installs run only after a yes to each (or `--install --yes`;
  `--yes` alone never installs); profiles are written after a yes (or
  `--yes`); `--service` is itself the request. Logins are only checked
  (`login_check`), and the command to log in printed for the person; of a
  profile's values only its home is ever shown.
- Other accounts' homes are found by name beside the default ones
  (`.claude-<name>`, `.codex-<name>`), real directories holding the
  harness's login file; contents are never read.
- `--service` writes a systemd user unit or a launchd agent that runs the
  same `vessel` with `node start`, this data directory and the `PATH` init
  ran with (a service manager's own `PATH` misses the person's harness
  installs). An existing file with other content is left alone. Windows
  says it is not available yet.
