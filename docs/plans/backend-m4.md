# Backend milestone 4: the fleet

Status: proposed (draft 2026-11-06). Starts once milestone 3 is merged and
green on all three CI platforms. Owner: coordinator. Two implementation
tracks, then a short joint one, as in earlier milestones.

Read first: `AGENTS.md`, `docs/VISION.md` section 3 (the fleet),
`docs/ROADMAP.md` Phase 2, `docs/LESSONS.md`, ADRs 0005, 0009 and 0010,
and the three proposed ADRs this plan depends on:

- 0011, harness profiles;
- 0012, fleet sync and joining a fleet (`vessel init`);
- 0013, agent principals and cross-node sessions.

"Grounding" at the end summarises the operator's real setup and what Paseo
does, with paths.

## Goal

Roadmap Phase 2. One person's machines behave as one fleet:

```bash
# a harness with several accounts: one profile per account, chosen per session
vessel profiles                                  # claude, claude-alt, codex, ...
vessel run claude --profile claude-alt
vessel session move-profile 3f2a claude-alalt    # at a usage limit: same conversation, next account

# a new machine joins in minutes
vessel init --service                            # node, harnesses, profiles, logins
vessel init --join                               # paste the home's pairing link when asked
vessel fleet status                              # bundle generation, conflicts, members

# agents use the fleet
#   (inside a session, through the `vessel` MCP tools)
#   "start codex on gpu-box in ~/src/model, run the benchmarks, tell me the result"
vessel sessions --node gpu-box                   # the session the agent started, created_by agent 3f2a@laptop
```

1. **Harness profiles** (ADR 0011): named variants of a harness, defined on
   each node (`profiles.toml`: environment to set or remove, extra
   arguments), chosen per session and recorded with it. A structured Claude
   Code session can move to another profile and keep its conversation,
   which is what the operator's account-swap plugin does today.
2. **Fleet sync** (ADR 0012): one node is the fleet's home; members pull its
   bundle (instructions, skills, MCP server definitions) and write it into
   every profile's harness home, keeping local edits and reporting
   conflicts. MCP servers reach harnesses at launch, not through their
   config files. Secrets stay on each node and are referenced by name.
   Sessions and memory move on demand.
3. **`vessel init`**: first-run setup that is safe to repeat: node, service,
   harnesses, profiles and logins, joining the fleet.
4. **Agent-to-agent** (ADR 0013): every session gets a credential and the
   `vessel` MCP tools; an agent lists the fleet's sessions and starts, drives
   and reads sessions on any member, with scopes that only shrink, ownership
   of what it started, and limits on depth and count.

## Out of scope

- Sandboxing agents (separate OS users, containers). ADR 0013 says plainly
  that per-agent identity under one OS user is attribution, not
  containment.
- Continuous replication of memory or transcripts; a mesh of node links;
  more than one fleet home.
- Harness installation beyond running each harness's documented install
  command on request.
- Model routing (Phase 3), the LSP bridge (Phase 4), the web UI.
- Moving terminal sessions between profiles or nodes: the harness's TUI
  owns that state.

## What M3 gives us

- Secure channels to any node, direct or relayed, with pairing links,
  device credentials, scopes and revocation (ADRs 0009, 0010). Membership in
  a fleet is a pairing with a new scope; cross-node calls ride the same
  channels.
- The guard that checks every call's scope from one table, and principals
  fixed per connection. Agents become one more kind of principal.
- `SecureGateway` serving any carrier, `Pinned`, the node book,
  `follow_resuming`.

## Contract changes (coordinator, before the tracks start)

Protocol 5, all additive. Nodes speak 2..=5.

- **Profiles**: `SessionSpec.profile`, `SessionInfo.profile`,
  `ProfileInfo { name, label, harness, sets: [variable names] }`,
  `profile.list` (read), `profile.reload` (admin),
  `session.move_profile { session, profile, force }` (operate), and the
  session event `ProfileChanged { from, to }`.
- **Origins**: `SessionInfo.created_by: Creator` (`owner`, `device {id}`,
  `agent {node, session}`, with the chain for forwarded calls).
- **Agents**: the local socket preamble gains
  `{ "type": "agent", "session", "secret" }`; the principal kind `agent`.
- **Fleet**: scope `sync`; `fleet.manifest`, `fleet.blob {hash}` (chunked),
  `fleet.watch` (as built: a long poll for a newer generation), `fleet.members`,
  `fleet.push` (admin), `fleet.forward { node, call }` (the call's own
  scope, intersected), `session.export`, `session.import`, `secret.copy`
  (admin). As built, protocol 5 carries the profile, agent and fleet-core
  rows and `fleet.report`; `fleet.push`, `fleet.forward`, `session.export`,
  `session.import` and `secret.copy` follow in a later version ("Contract
  as built" below).
- `PROTOCOL.md` gets a version 5 entry; the method table gains the rows and
  their scopes.

### Contract as built

The contract lands in steps, so Track H can start before M3's Track P is
merged and the session structs (which Track P also touches) change once.

- **Step 1a (2026-11-06, additive types only, no wire change yet):**
  `vessel_proto::profile` has `ProfileName` (1-64 of letters, digits, `.`,
  `_`, `-`; safe in file names, command lines and TOML keys unquoted),
  `ProfileInfo`, `Creator` (tag `kind`; default `owner`, which is what every
  session from before protocol 5 is), `AgentSecret` (32 bytes, redacted in
  `Debug`) and `AgentAuth` (tag `type: "agent"`). The node tells an
  `AgentAuth` preamble from a token preamble by the first character: a token
  is never a JSON object. The `secret!` macro moved from `device.rs` to
  `key.rs` so `AgentSecret` shares it with `DeviceSecret` and
  `InvitationSecret`. `PROTOCOL_VERSION` stays 4.
- **Step 1b (2026-11-06, with 2b: `PROTOCOL_VERSION = 5`):** `SessionSpec`
  gains `profile` and `SessionInfo` gains `profile` and `created_by`; both
  are `#[non_exhaustive]` with constructors (`SessionSpec::new(harness,
  mode).with_cwd(..).with_profile(..)`, `SessionInfo::new(id, spec,
  created_at)`), and every literal in the workspace moved to them. The
  methods `profile.list`, `profile.reload`, `session.move_profile`; the
  event `ProfileChanged`, which the session runtime already projects into
  `SessionInfo.profile` (the store does with schema 3, Track H).
- **Step 2a (2026-11-06, additive types only):** validated names in
  `vessel_proto::name`, one implementation and one `InvalidName { kind,
  value }` error: `ProfileName` (moved here; `InvalidProfileName` is gone),
  `EnvName` and `SecretName` (`[A-Za-z_][A-Za-z0-9_]*`, at most 128),
  `McpServerName` (`[A-Za-z0-9_-]`, 1-64, no `.` so it stays one key in
  `codex -c mcp_servers.<name>…`), `HeaderName` (an RFC 9110 token).
  `ProfileInfo.sets` is `Vec<EnvName>`. `vessel_proto::fleet` has
  `Generation`, `BlobHash` (SHA-256, hex), `BundlePath` (relative, `/`
  separated, no `.`/`..`/empty segments, no characters or device names
  Windows refuses; `under(root)` cannot leave `root`), `BundleFile`,
  `Manifest { generation, files: BTreeMap<BundlePath, BundleFile>, mcp }`,
  `HttpUrl`, `ConfigValue` (a string or `{"secret": "NAME"}`),
  `ResolvedValue` (no serde, redacted `Debug`) and
  `McpTransport<V>` with the aliases `McpDefinitions` and
  `ResolvedMcpServers` (maps by `McpServerName`, so duplicates cannot be
  represented). Track H takes `ResolvedMcpServers` at launch and in
  `session/new`/`session/load`, so no `vessel-agent -> harness` edge is
  needed; Track F's resolve is `McpTransport::try_map_values`. The hex
  helpers (`hex_array`, `write_hex`) moved into `key.rs` and serve
  `RoutingId`, `ConnId` and `BlobHash`.
- **Step 2b (2026-11-06, the same bump):** scope `Sync`
  (`Scope::DEVICE_DEFAULT` keeps invitations at read, operate and admin;
  `InviteDevice.scopes` asks for others), and the rows `fleet.manifest`,
  `fleet.blob` (`ReadBlob`/`BlobChunk`, 1 MiB chunks), `fleet.watch` (a
  long poll, not a subscription: no new frame type), `fleet.report` (what a
  member applied and its conflicts) and `fleet.members`. Not in protocol 5:
  `fleet.push`, `fleet.forward`, `session.export`/`import`, `secret.copy`;
  they come with Tracks F4 and X as a later version.
- **Versioning, from protocol 5 on:** every table row names its first
  version (`[Scope, since]`), and `Call::since` raises it for later fields.
  `vessel-rpc` enforces both on both sides and adapts outgoing frames to an
  older connection (`ServerFrame::for_protocol`: event stand-ins with the
  same `seq`, newer scopes left out of `device.list`). The client's
  per-method version constants are gone. The node answers the new methods
  `unsupported` until the tracks implement them.

## Crates (ADR 0001)

| Crate | Change | New edges |
| --- | --- | --- |
| `vessel-harness` | `Profile` applied in `launch`; per-harness knowledge of its home variable, instruction file, skill directory, transcript layout, login check, and launch-time MCP form | none |
| `vessel-fleet` (new) | Bundle manifests (content addressed), the materialiser (copies with recorded hashes, local edits kept, conflicts listed), secret references | **new crate: `-> proto, harness`** |
| `vessel-mcp` (new) | `vessel mcp`: the MCP server (stdio) attached to every session; each tool is one call through the agent's principal | **new crate: `-> proto, client`** |
| `vessel-agent` | MCP servers in `session/new` and `session/load` | none |
| `vessel-store` | Schema 3: `profile` and `created_by` on sessions; fleet membership and bundle state | none |
| `vessel-node` | `profiles.toml`, agent credentials and the ownership rule, `move_profile`, fleet home and member roles, `fleet.forward`, session export and import, `secrets.toml` | **`-> fleet, client`** (calls to the home and to members) |
| `vessel-client` | Profile, fleet and session-move calls | none |
| `vessel-cli` | `vessel init`, `vessel profiles`, `--profile`, `session move-profile`, `session move/copy`, `vessel fleet …`, `vessel mcp` | **`-> mcp`** |
| `vessel-testkit` | `fake-acp` records the MCP servers and environment it was given and supports `session/load` across homes | none |

## Seams

### `vessel-harness`: profiles

```rust
pub struct Profile { pub name: ProfileName, pub harness: HarnessKind, pub label: Option<String>,
                     pub env: Vec<(String, String)>, pub unset: Vec<String>, pub args: Vec<String> }
pub fn launch(lookup: &Lookup, spec: &SessionSpec, profile: &Profile, mcp: &[McpServer]) -> Result<CommandSpec, HarnessError>;
pub fn home(harness: HarnessKind, profile: &Profile, user_home: &Path) -> PathBuf;   // CLAUDE_CONFIG_DIR, CODEX_HOME, defaults
pub fn session_files(harness: HarnessKind, home: &Path, cwd: &Path, id: &HarnessSessionId) -> Option<SessionFiles>;
pub fn login_check(harness: HarnessKind) -> Option<CommandSpec>;  // `claude auth status`
```

- The implicit default profile per harness is today's launch, unchanged.
- Arguments apply in terminal mode only; structured mode goes through ACP.

### `vessel-fleet`

```rust
pub struct Manifest { pub generation: u64, pub entries: Vec<Entry> }  // path, sha256, mode
pub fn bundle(sources: &FleetSources) -> io::Result<(Manifest, BlobStore)>;  // on the home
pub fn plan(manifest: &Manifest, targets: &[Target], managed: &Managed) -> Plan; // writes, skips, conflicts
pub fn apply(plan: &Plan, blobs: &BlobStore, managed: &mut Managed) -> io::Result<Report>;
pub fn resolve(servers: &[McpDefinition], secrets: &Secrets) -> (Vec<McpServer>, Vec<Missing>);
```

- Never writes a file it did not write before (by recorded hash), never
  follows a symlink out of a target, skips targets that resolve to the
  source.

### `vessel-mcp`

- MCP over stdio: `initialize`, `tools/list`, `tools/call` for list nodes,
  list sessions, create session, send input, read events, wait for turn end,
  kill. Reads its credential from its own environment, connects to the
  node's local socket, presents the agent preamble.

### `vessel-node`

- Agent credentials: minted per session, hash in memory, the credential
  passed only to `vessel mcp`'s environment; refused once the session ends;
  connections closed then.
- Guard: for `Agent` principals, `operate` calls on a session also need
  ownership (created by the agent or its descendants) unless `owned_only`
  is off; limits of 8 owned live sessions and depth 3.
- Fleet roles: home (`fleet.toml`, bundle, generations, `fleet.watch`,
  `fleet.forward` to members with its credential there) and member (pull on
  start, on notice, every 10 minutes; apply; report).

## Data flow

```text
agent in session S on laptop                 laptop vesseld              home vesseld              gpu-box vesseld
  |-- MCP tools/call create_session(gpu-box) ->|                            |                          |
  |   (vessel mcp, agent preamble: S + secret) |-- fleet.forward ---------->|                          |
  |                                            |   {gpu-box, session.create, on behalf of S@laptop}   |
  |                                            |                            |-- session.create ------->|
  |                                            |                            |   principal: home's grant |
  |                                            |                            |   ∩ agent scopes, chain   |
  |<-- session T (created_by agent S@laptop) --|<---------------------------|<-------------------------|
```

```text
member vesseld                         home vesseld
  |-- fleet.watch (subscription) ------->|
  |<-- generation 42 --------------------|
  |-- fleet.manifest -------------------->|
  |-- fleet.blob {hash} (missing only) ->|
  |  plan: write / keep local edit (conflict) / skip linked
  |  apply into each profile's home; MCP servers kept for launch time
```

## Track H: profiles and agents on one node

- **H1. Profiles.** `profiles.toml` (owner-only, warn if not), parsing with
  clear errors, the default profiles, `profile.list`/`reload`, `--profile`
  in `vessel run`, `SessionInfo.profile`. Tests with `fake-acp` and
  `fake-tui` that print the environment they got: two profiles, two homes;
  `unset` removes; a profile for another harness is refused; values never
  reach `profile.list`.
- **H2. Moving a structured session.** `session.move_profile`: refuse
  mid-turn unless `force`, refuse a target that is not logged in, hard-link
  the descriptor's session files, restart with `session/load`, emit
  `ProfileChanged`. Tests with a fake Claude layout in a temporary home:
  the same inode in both homes, `memory/` untouched, the conversation
  continues (fake-acp checks the loaded session id).
- **H3. MCP at launch.** `session/new`/`session/load` carry MCP servers;
  terminal sessions get an owner-only per-session MCP file (deleted with
  the session). A test asserts no secret value appears in any harness
  command line.
- **H4. Agent principals.** Per-session credentials, the agent preamble,
  ownership, limits, `created_by`. `vessel-mcp` with its tools. Tests:
  an agent lists and creates sessions; a forged session id or a stale
  credential is refused; another agent's session cannot be killed; depth
  and count limits; a device with `read` cannot start a session whose agent
  operates.

### Track H progress

Harness side of H1-H3, 2026-11-06, branch `backend/m4-profiles` (crates
`vessel-harness`, `vessel-agent`, `vessel-session`, `vessel-testkit`):

- Done: `Profile` (default per harness, `info()`, redacted `Debug`),
  `parse_profiles` for `profiles.toml` text, `home`, `login_check`,
  `session_files` and `link_session`, `launch`/`launch_in` with profile and
  MCP servers, ACP `session/new`/`session/load` with MCP servers,
  `SessionRuntime::create_with(spec, LaunchOptions)`, and the fakes
  reporting environment, arguments, MCP servers and the loaded session id.
  Tests cover: two profiles give two homes, `unset` removes, another
  harness's profile is refused, values never reach `ProfileInfo` or
  `Debug`, MCP servers arrive at the fake (new and load; HTTP left out for
  an agent without it), no value on any command line, the MCP file is
  owner-only and removed with the session, and linking on a fake Claude
  layout (same inode, `memory/` untouched, no symbolic link followed).
- Refinements: ADR 0011 and ADR 0012, "As built" sections.
- Left for the node, after the protocol 5 contract (step 1b): reading
  `profiles.toml` (owner-only check), `profile.list`/`reload`, `--profile`,
  `SessionSpec.profile` and `SessionInfo.profile`, passing the resolved
  profile and an owner-only MCP directory into `create_with`, and H2's
  `session.move_profile` on top of `login_check`, `session_files` and
  `link_session`. H4 is not started.

## Track F: fleet and first run

- **F1. `vessel-fleet`.** Manifests, blob store, plan and apply with
  recorded hashes, conflicts, skipping linked targets, skill names from
  `SKILL.md`, secret references. Unit tests over temporary trees, including
  the operator's layout (one source, symlinked targets).
- **F2. Home and members.** `vessel fleet init`, membership by pairing with
  the `sync` scope (link read from stdin or a prompt), the mutual grant,
  `fleet.manifest`/`blob`/`watch`/`members`/`push`, pull on start, notice
  and timer. End-to-end over in-memory carriers and over a real relay.
- **F3. Secrets.** `secrets.toml`, `vessel fleet secret set` (prompt or
  stdin), `secret.copy`, resolution at launch, a missing secret reported.
- **F4. History and memory.** `session.export`/`import`, `vessel session
  move|copy --to`, working-directory mapping, `vessel fleet memory copy`.
- **F5. `vessel init`.** Idempotent steps, `--service` (systemd user unit,
  launchd agent; Windows later), harness install offers, profile
  discovery, login offers, `--join`. Tested against a temporary `HOME` with
  fake harnesses; a second run changes nothing.

## Track X: across nodes (after H4 and F2)

- `fleet.forward` on the home, the intersection rule and the creator chain
  on the target, the `vessel` MCP tools taking a node. Tests: an agent on
  one node creates, drives and reads a session on another through the home;
  the target's grant to the home caps it; depth counts across nodes.

## Done criteria (milestone)

- The operator's four Claude logins work as four profiles on one node, and
  a structured session moves between them keeping its conversation (manual
  smoke by the coordinator on the operator's development machine, with
  test agents).
- A fresh machine runs `vessel init --service --join`, and within minutes
  has the fleet's instructions, skills and MCP servers in each of its
  profiles; a local edit on it is kept and listed as a conflict.
- An agent on one node starts a session on another, sends it a prompt, and
  reads the result; the session shows its creator; a forged identity is
  refused.
- No secret value appears in a bundle, a log, `profile.list` or a command
  line (checked by tests that grep captured output).
- The gate passes on Linux, macOS and Windows. `PROTOCOL.md` has version 5;
  ADRs 0011 to 0013 are accepted; `ARCHITECTURE.md`, `README.md` and
  `docs/ROADMAP.md` describe profiles, the fleet and agent tools.

## Decisions (coordinator, 2026-11-06)

All ten proposals below are accepted as drafted. Notes on the ones that
carry a condition:

- **8, MCP implementation:** use `rmcp` (the official Rust SDK) if its
  dependency check passes AGENTS.md section 9 (no `unsafe` of ours, no
  duplicate async runtime, a justified feature set). If it fails the check,
  write the small server the plan sketches. The track records the check's
  result either way.
- **4, memory:** one-time copies only, never continuous sync. The operator
  keeps per-account memory separate on purpose (the account-swap plugin
  never shares `memory/`).
- **2 and 6, one home and calls through it:** this matches the operator's
  single-source rule for instructions and skills (`~/.codex` as origin,
  everything else links or copies).

ADRs 0011, 0012 and 0013 are accepted with these decisions. Milestone 4
starts on `backend/m4`, which branches from `backend/m3` and receives M3's
remaining Track P work by merge. Protocol 5 is the coordinator's next
contract change; tracks start after it.

## Open decisions (as drafted, kept for the record)

1. **Profile switch of a live session in M4** (ADR 0011). Proposed: yes,
   structured Claude Code first; it is the operator's daily need and the
   only part of the account-swap plugin Vessel cannot replace otherwise.
   Alternative: profiles only, switching later.
2. **One fleet home** (ADR 0012). Proposed: one home node, members pull, the
   operator's single-source rule. Alternatives: multi-master merging, or a
   git remote as the bundle's transport.
3. **MCP servers at launch, not in harness config files** (ADR 0012).
   Proposed: launch-time only; files would race the harnesses and hold
   secrets.
4. **Memory**. Proposed: per profile, never synced continuously; a one-time
   copy on request. Alternative: continuous two-way sync with conflict
   copies.
5. **History**. Proposed: move or copy one session on request. Alternative:
   continuous replication.
6. **Cross-node calls through the home** (ADR 0013). Proposed: the home
   relays calls to members it holds credentials on. Alternative: a mesh
   (every node pairs with every other).
7. **Agent defaults** (ADR 0013). Proposed: `read` and `operate` on owned
   sessions only, no `admin`, at most 8 owned live sessions, depth 3.
8. **MCP implementation for `vessel mcp`**. Proposed: the official Rust SDK
   (`rmcp`) if its dependency tree is reasonable (to be checked like
   `snow`); otherwise a small hand-written stdio server for the three
   methods it needs.
9. **`vessel init` installing harnesses**. Proposed: offer each harness's
   documented install command and run it only when the user confirms;
   `--yes` for scripts. Alternative: print the commands only.
10. **Service install** in `vessel init --service`. Proposed: systemd user
    unit (with linger notice) and launchd agent in M4, Windows later.

## Grounding

### The operator's setup (read 2026-11-06, read-only; nothing changed)

- **Instructions and skills, one source**: `~/.codex/AGENTS.md` is the
  source and `~/.claude/CLAUDE.md` links to it; skills live in
  `~/.codex/skills/<dir>` and every Claude login has a link per skill,
  named by `SKILL.md`'s `name` (a directory with a Korean name is linked
  under its English `name`). `agent-config-sync` keeps it true: a systemd path
  unit plus a 10-minute timer, an idempotent script under one lock; a skill
  made on a Claude side moves into the source; a same-name conflict keeps
  the source and logs to `CONFLICTS.log`. Not synced: memory,
  `settings.json`, subagents, Paseo-managed copies, claude.ai account
  skills (the script's README and the global instructions).
- **Accounts**: four Claude Code logins (`~/.claude` and three more
  configuration directories), each a Paseo provider with
  `extends: "claude"` and `CLAUDE_CONFIG_DIR` in its `env`. The account-swap
  plugin hard-links one session's transcript files (never `memory/`) into
  the target login, points the agent's `CLAUDE_CONFIG_DIR` at it through a
  per-agent symlink, and restarts only the Claude process, measured at about
  3.4 s for any conversation length (the plugin's README).
- **Harness files**: Claude Code's `.claude.json` holds user-scope MCP
  servers next to the OAuth account and caches; Codex's `config.toml` holds
  `[mcp_servers.*]` next to model settings, some with `env` tables. Neither
  file can be synced whole.

### Paseo (installed daemon 0.15.1, read 2026-11-06, ideas only)

Paths are under `@getpaseo/server/dist/server/server/`.

- **Providers**: `extends` names a built-in provider only; `env` merges
  over the base's; `command` replaces it; models are not inherited; the
  harness environment is the daemon's, then the provider's, then the
  creating call's (`agent/provider-registry.js`,
  `agent/provider-launch-config.js`). "Agent profiles" are presets an
  agent copies into its own create call.
- **Agents calling Paseo**: an MCP server at
  `/mcp/agents?callerAgentId=<id>` with one token per daemon run shared by
  all agents; the caller is whoever the parameter names; open without a
  daemon password; no ownership check on `kill_agent`/`send_agent_prompt`;
  seven tools route to other daemons by `serverId`
  (`agent/runtime-mcp-config.js`, `bootstrap.js`, `auth.js`,
  `agent/tools/paseo-tools.js`).
- **Across hosts**: connected hosts route sessions (pinned server ids,
  `hosts.json` 0600); the Hub runs "executions" with granted permissions;
  `directory-sync` is daemon-to-own-clients only. Nothing is synced.
- **Skills**: Paseo copies its own skills into `~/.agents/skills` and
  `~/.claude/skills` with a per-file hash list
  (`.paseo-managed-files.json`); updates overwrite edited files; deletions
  only touch unmodified ones; symlinks are refused
  (`orchestration-skills/internal/sync.js`).
- **First run**: the daemon creates its config, server id, key pair and a
  per-start local credential silently; `paseo onboard` asks about voice,
  starts the daemon and prints pairing instructions.
