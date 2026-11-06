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

2026-11-06, branch `backend/m4-profiles`.

- **Harness side of H1-H3 (done):** `Profile` (default per harness,
  `info()`, redacted `Debug`), `parse_profiles` for `profiles.toml` text,
  `home`, `login_check`, `layout` (shared instructions and skills, for
  Track F), `session_files` and `link_session`, `launch` with profile, MCP
  servers and the node's MCP directory, ACP `session/new`/`session/load`
  with MCP servers, `SessionRuntime::create_with(spec, LaunchOptions)`,
  and the fakes reporting environment, arguments, MCP servers and the
  loaded session id. Refinements are in ADR 0011 and ADR 0012, "As built".
- **Review fixes (independent review of the harness side):** errors never
  quote a `profiles.toml` value; Codex MCP variables cannot change Codex's
  own environment (inherited values, the profile's, and variables that
  steer it); the MCP file needs the node's directory (no shared fallback;
  Windows only inside the user's profile); Codex rollouts match the whole
  id and two are an error; one case rule for variable names (Windows
  ignores case); `EPERM` from an exited leader's group counts as gone only
  once the group is (`vessel-proc`, `vessel-pty`); typed errors and
  config; links taken back on a failure partway; `memory` in any case.
- **H1 on the node (done):** `profiles.toml` in the data directory, read
  at start (a broken file stops the node) and on `profile.reload` (a
  broken one leaves the profiles in effect, `conflict`), warned about when
  others can read it; `profile.list` with the harness filter;
  `session.create` resolves `spec.profile` (unknown or another harness's:
  `invalid_request`) and records `created_by` (owner or device); an
  owner-only `<data_dir>/mcp`, emptied at start; store schema 3
  (`profile`, following `ProfileChanged`, and `created_by`); `vessel run
  --profile`, `vessel profiles [--harness] [--reload]`. MCP servers are
  still empty: Track F's resolve plugs in at
  `vessel-node/src/profiles/launch.rs` (`mcp_servers`). End-to-end tests:
  two profiles, two homes; `unset` removes; unknown and other-harness
  profiles refused; values never reach `profile.list` or reload errors;
  reload keeps the set on a bad file; a broken file stops the start;
  profile and creator survive a restart; a device's session records the
  device.
- **H2 (done):** `session.move_profile` end to end: the node resolves the
  target and both homes and runs the target's login check (`fake-tui`'s
  `auth status` stands in); the session actor holds input, refuses a
  running turn unless forced, links the files, restarts the agent with
  `session/load` (its replay kept out of the log) and publishes
  `ProfileChanged`, or starts the previous agent again when the new one
  does not start. `vessel session move-profile`. Tests: one transcript in
  two homes (same inode on Unix; a write through one name shows through
  the other elsewhere), `memory/` untouched, `fake-acp` reports the loaded
  session id and the new home; mid-turn refused, forced cancels; a failed
  start falls back; terminal sessions, missing transcripts, unknown,
  other-harness and logged-out profiles refused unchanged; the moved
  profile survives a restart. Details in ADR 0011, "As built".
- **H3, node side (done):** `McpSource` in `vessel-node`
  (`profiles/launch.rs`), asked per launch and per move; `NoMcpServers`
  until fleet sync replaces it in `node.rs` with a source over its
  definitions and secrets (`vessel_fleet::resolve`). Missing secrets are
  named in the session's log (`LaunchOptions::notices`, `vessel:` log
  lines after `starting`, and after a move) and in the node's log.
- **H4 (done):** agent principals and `vessel mcp`, as ADR 0013 "As built"
  describes. Decision 8's check (2026-11-06, rmcp 3.9.0): passed (no
  `unsafe` of ours, the same tokio, features `server` and `transport-io`
  only, nine crates added), so `vessel-mcp` uses `rmcp`, with hand-written
  tools and no proc-macros. Tests: registry units (admission, retirement
  closing connections, grants shrinking, depth and count, move
  credentials); end to end on the node (an agent lists, creates, sends to
  and kills its own sessions; the owner's, another agent's and admin calls
  are forbidden; forged, borrowed and ended credentials refused and the
  connection closed; depth 3 and 8 owned sessions; the top agent stops a
  descendant); `vessel-mcp` over a pipe with the node's real environment
  (create with a prompt, `wait_turn` reads `echo: plain`, refusals as tool
  errors); the `vessel mcp` binary on stdio. Not built: profile grants
  (`[profiles.X.agent]`) and other nodes (Track X). A device with only `read` cannot create
  sessions at all (scope check), so "a device with `read` cannot start a
  session whose agent operates" holds by the scope table; the unit test
  checks that a device's grant caps its sessions' agents.
- **Next:** Track H is done; Track X needs F2.

### Track F progress

2026-11-06, branch `backend/m4-fleet`.

- **F1 (done, merged):** `vessel-fleet`, then hardened after an
  independent review (owner-only files, canonical roots, directory-handle
  access, opaque plans, name collisions, strict `SKILL.md`); ADR 0012
  "As built".
- **F2 (done):** the node's `fleet` module (home publishing and serving
  `fleet.*`, members joining, pulling, applying, reporting), the client's
  fleet calls, `vessel fleet init|invite|join|leave|status|members|
  restore`. Not built: the mutual grant and `fleet.push` (Track X,
  protocol 6). Tests: in process over in-memory channels through the real
  gateway (follow, kept edits, reports, scopes, delete and restore),
  between two nodes through an in-process relay (join while running,
  follow an edit, leave), and the CLI through a relay.
- **F3 (done, except `secret.copy`):** `secrets.toml`, `vessel fleet
  secret set|list|delete`, and the fleet's `McpSource` (`FleetMcp`) in
  place of `NoMcpServers`; `secret.copy` and the `secret.*` rows wait for
  protocol 6, whose handlers will call the same store. Tests: the store
  (comments kept, owner-only, errors never quoting a value), a member
  receiving the definitions, a session's agent getting a server with
  this node's secret and starting without it once the secret is deleted
  (named in its log), and the CLI (stdin only, names only).
- **Next:** F4 (history and memory transfer), once Track R's protocol 6
  contract lands.

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

### F5 progress

2026-11-06, moved from Track F to Track H's worker, branch
`backend/m4-profiles`.

- **Done:** `vessel init [--service] [--yes] [--install]` (vessel-cli
  `init`), with the harness knowledge in `vessel-harness` (`setup`: install
  commands, login commands, other accounts' homes, adding a profile to the
  file). Each step says `done`, `already done` or `skipped` with why, and
  what is left is listed at the end. Steps: the data directory and node
  identity; the harnesses found; install offers (the vendor's `npm`
  command, run only after a yes; `--yes` alone never installs, `--install
  --yes` does); profiles for other accounts' homes beside the default ones
  (`~/.claude-<name>` holding `.credentials.json` or `.claude.json`,
  `~/.codex-<name>` holding `auth.json`; names only), written into
  `profiles.toml` after a yes with the file's own text kept; a login check
  per profile, with the command to log in printed (only the home variable
  is ever shown); `--service`: a systemd user unit (with the `PATH` init ran
  with) or a launchd agent, enabled, with how to undo, left alone if it
  differs; Windows says it is not available yet.
- **`--join` (after F2 landed):** one more step, before `--service`: the
  link `vessel fleet invite` printed, read from standard input on the same
  reader as init's other answers, shown and confirmed (`--yes` confirms;
  piped without it is refused), then `vessel_node::fleet::join`. A member
  reports "already done: a member of <home>" and reads no link; a home
  reports it joins none.
- **Tests** (vessel-cli, the `vessel` binary with a temporary home, made-up
  account names, and `fake-tool` as `npm`, `systemctl` and `launchctl` on a
  `PATH` that holds only fakes): a first run sets up, a second changes no
  file and runs no tool; no answers or "no" install and write nothing, a
  yes runs that one install, `--install` needs `--yes`; `--join` pairs
  with a home behind an in-process relay once, and a second run is
  already done; the service is installed once, an edited file left alone (and
  Windows reports it unavailable); logged-out profiles get their login
  command and a token in `profiles.toml` never appears. Unit tests for
  discovery, adding a profile, the unit and plist text.

## Track X: across nodes (after H4 and F2)

- `fleet.forward` on the home, the intersection rule and the creator chain
  on the target, the `vessel` MCP tools taking a node. Tests: an agent on
  one node creates, drives and reads a session on another through the home;
  the target's grant to the home caps it; depth counts across nodes.

## Protocol 6 (draft)

Drafted 2026-11-06 by Track H's worker for the coordinator's review; the
coordinator lands the `vessel-proto` contract. It covers F4 (sessions and
memory across nodes), the rest of F3 (secrets on the wire) and Track X
(agent calls across nodes through the home), on ADR 0012 (one home,
members pull; membership by pairing; secrets by reference; history and
memory on demand) and ADR 0013 (a credential per session; scopes plus
ownership; across nodes through the home; grants only shrink).

### What exists to build on

- Members hold a `sync` credential on the home (F2, as built). The home
  holds none on its members yet: the mutual grant was left for this
  version. Only the home can reach every node.
- `fleet.manifest` + `fleet.blob` already move a set of files by content
  hash, in 1 MiB chunks (`Manifest`, `BundlePath`, `BundleFile`,
  `BlobChunk`). `fleet.watch` set the precedent of a long poll instead of
  a new stream type.
- Every row names `[Scope, since]`; `Call::since` raises it per field; the
  generated guard checks every call's scope, and for agents (H4) the
  ownership of the session an `operate` call names (`Target`).
- `Creator` has `owner`, `device {id}` and `agent {node, session}`.
  Session structs do not reject unknown fields; some fleet types do.

### Shape in one paragraph

Data moves by **target pull** with one generic mechanism: a source node
snapshots a set of files into a *transfer* (a manifest of `BundlePath →
BundleFile` plus chunks by hash, the bundle's own shape) and the target
reads it. Calls reach other nodes in an **envelope** that travels caller →
its node → home → target and is checked at each step: a node checks the
call against its caller as if it were local, the home checks that a
member speaks only for itself, and the target runs it with the
intersection of the delivering node's grant there and the origin's
scopes, ownership and limits applied to the origin. Cross-node waiting is
a **long-poll read**, not a relayed subscription. Secrets are set and
copied, never read back.

### Rows

Every row is `since: 6`. "Target" means the node that runs the inner
call.

Envelope (Track X):

| Method | Scope | Request → response | What it does |
| --- | --- | --- | --- |
| `fleet.forward` | Read | `Forward { node: NodeId, call: Call }` → `Forwarded { reply: Reply }` | A client (CLI, device, agent) asks its own node to run `call` on fleet node `node`. The node checks `call`'s own row scope (and, for an agent, the ownership and limits it can check locally) against the caller, as if the call were local, builds the `Origin` (below), and sends `fleet.relay` to its home (or, on the home, `fleet.deliver` to the member). `node` equal to the node itself runs the call locally. |
| `fleet.relay` | Sync | `Relay { node, call, origin: Origin }` → `Forwarded` | A member asks the home to pass a call on. The home refuses an `origin.node` other than the member's own node (a member speaks only for its own principals), then sends `fleet.deliver` to `node` with its credential there, or, when `node` is the home itself, runs the call as `fleet.deliver` would. |
| `fleet.deliver` | Read | `Deliver { call, origin }` → `Forwarded` | The home asks a member to run a call on behalf of `origin`. The member runs it as a delegated principal (below). Never passed on: delivering is the last hop, so there are no loops and at most two hops. |
| `fleet.grant` | Sync | `GrantHome { link: PairingLink }` → `Ack` | The mutual grant (ADR 0012): a member hands the home a single-use pairing link to itself, made with its own `device.invite` and the scopes its owner chose (`vessel fleet join --grant`, default `read`, `operate`). The home redeems it over the link's routes like any device (ADR 0010) and keeps the credential. Revoking it on the member cuts the home off from that member. |
| `fleet.peers` | Sync | `ListPeers` → `Vec<FleetNode>` | The home's view of the fleet for members: `FleetNode { node, name, protocol, reachable }`, the home included. |
| `fleet.nodes` | Read | `ListNodes` → `Vec<FleetNode>` | What any node knows of its fleet (a member asks its home with `fleet.peers`); the agents' `list_nodes`. |

`Origin { node: NodeId, by: OriginBy, scopes: Vec<Scope> }`, with `OriginBy`
`owner`, `device { id }` or `agent { session, depth }`. `scopes` is what the
origin principal holds on its own node; `depth` is its session's agent
depth.

Transfers, generic (F4):

| Method | Scope | Request → response | What it does |
| --- | --- | --- | --- |
| `session.export` | Operate | `ExportSession { session, force: bool }` → `TransferManifest` | Snapshot a structured session's harness files (the descriptor's `session_files`, never `memory/`) into a transfer. A live session mid-turn is refused unless `force`, as for `session.move_profile`. For agents it is an `operate` call on a session, so ownership applies: an agent copies away only what it started. |
| `memory.export` | Admin | `ExportMemory { profile: Option<ProfileName>, project: Option<PathBuf> }` → `TransferManifest` | Snapshot a profile's memory: one project's (`project`, a working directory on this node) or all of them. `admin`, since memory is kept per account on purpose (decision 4). |
| `transfer.read` | Read | `ReadTransfer { transfer: TransferId, hash: BlobHash, offset: u64 }` → `BlobChunk` | A chunk of one file of an open transfer, as `fleet.blob`. |
| `transfer.close` | Read | `CloseTransfer { transfer }` → `Ack` | Drop the snapshot. Transfers also expire after 10 idle minutes and at restart (the staging directory is emptied at start). |
| `session.import` | Operate | `ImportSession { from: SessionRef { node, session }, profile: Option<ProfileName>, cwd: Option<PathBuf>, start: bool }` → `SessionInfo` | The target pulls: `session.export` on `from.node` (through `fleet.forward` when it is another node), reads every file, places them into the chosen profile's home with the project renamed for the target's working directory, and with `start` creates a session resuming the conversation (`session/load`). A file already there with other content refuses the import before anything is written. The new session records `origin` (below). `move` is `import` then `session.kill` on the source, done by the client. |
| `memory.import` | Admin | `ImportMemory { from: MemoryRef { node, profile, project }, profile: Option<ProfileName>, project: Option<PathBuf> }` → `MemoryImported { written: u32, same: u32, beside: Vec<BundlePath> }` | The same pull for memory. Conflicting files are written beside the existing ones (`.vessel-incoming`), as the materialiser does; nothing is overwritten. |

`TransferManifest { transfer: TransferId, files: BTreeMap<BundlePath,
BundleFile>, detail: TransferDetail }`; `TransferDetail` is `session {
harness, harness_session, profile, cwd, info: SessionInfo }` or `memory {
harness, profile, project }`. A `TransferId` is 128 random bits and
belongs to whoever opened it: the same principal (for a delivered call,
the delivering device together with the origin) must read and close it.

Secrets (F3):

| Method | Scope | Request → response | What it does |
| --- | --- | --- | --- |
| `secret.set` | Admin | `SetSecret { name: SecretName, value: SecretValue }` → `Ack` | Store a value in this node's `secrets.toml` (owner-only). |
| `secret.list` | Admin | `ListSecrets` → `Vec<SecretName>` | Names only. |
| `secret.delete` | Admin | `DeleteSecret { name }` → `Ack` | |
| `secret.copy` | Admin | `CopySecret { name, to: NodeId }` → `Ack` | The source sends its value on: `fleet.forward { node: to, call: secret.set { … } }`. The target's `admin` check applies to the delivering grant, so copying needs the target's owner to have granted the home `admin` there, explicitly. |

There is no `secret.get`: no call returns a value, so a value leaves a node
only into another node's store. `SecretValue` is a new type: serialisable
(unlike `ResolvedValue`), with a redacted `Debug`, held in memory only for
the call. Values cross inside the secure channels (client → node, member
↔ home); the home sees one in passing as it relays, which adds nothing to
what its credentials on members already allow. Nothing logs it: `vessel-rpc`
logs method names, never parameters, and every type that can hold one
redacts its `Debug`.

One field (Track X):

- `session.read` gains `wait: Option<u32>` (seconds, at most 60; `Call::since`
  6 for the field): with nothing at `from` yet, the node answers once
  something arrives or the wait ends (an empty page). It serves waiting on
  another node without relaying subscriptions.

### Authority on the target: the intersection rule

The target runs a delivered call as a **delegated principal**:

- `effective scopes = grant of the delivering device on the target ∩
  origin.scopes`. The delivering device is the home's (on a member), or
  the relaying member's own device on the home (when the target is the
  home). The call's row scope must be in `effective`, as for any caller.
  So nobody exceeds what the target's owner granted, and an agent never
  exceeds its own scopes.
- Ownership and limits apply to the origin. For `origin.by = agent`, an
  `operate` call on a session needs that session's `created_by` chain to
  reach `agent { node: origin.node, session }`, followed through the
  target's own sessions (the `Target` rule as built, generalised to an
  agent of another node). The target counts the agent's live sessions it
  holds (at most 8) and refuses a session that would be deeper than 3.
- The creator chain reaches the target in the envelope and stays in what
  the session records. A session created by a delivered call records
  `created_by` as `agent { node: origin.node, session }` for an agent
  (a variant protocol 5 already has), and as `device { id }` of the
  delivering device otherwise, which is true on the target. New optional
  fields keep the rest: `SessionInfo.via: Option<NodeId>` (the home it
  came through) and `SessionInfo.origin: Option<SessionOrigin>`
  (`remote { node, by: owner | device { id } }` for a forwarded creation,
  `imported { node, session }` for an import).
- A target refuses a delivered call whose `origin.node` is not a member of
  the delivering home's fleet (the home vouches only for its members).

### What crate owns what

| Crate | Part |
| --- | --- |
| `vessel-proto` | The rows, `Origin`, `TransferId`, `TransferManifest`, `TransferDetail`, `SessionRef`, `MemoryRef`, `FleetNode`, `SecretValue`, the `SessionInfo` fields, `ReadEvents.wait`. |
| `vessel-rpc` | Nothing new beyond the table; a per-call deadline long enough for a forwarded long poll. |
| `vessel-harness` | Which files a session or a profile's memory is (`session_files`, a new `memory_files`), and renaming a project directory for another working directory (`rebase`: Claude Code's slug; Codex keeps dates, so its rollouts need no rename). |
| `vessel-fleet` | Transfers as file sets: snapshot into a staging blob store, read by hash, and `place` (write a received set into a home without following links, never overwriting, conflicts beside), shared with the materialiser; `secrets.toml`. |
| `vessel-node` | The handlers: `transfers` (open, read, close, expiry), imports (pulling through the fleet link), `fleet.forward`/`relay`/`deliver` and the delegated principal in the guard, `fleet.grant` on both sides, `fleet.peers`/`nodes`, `session.read` waiting. |
| `vessel-client` | Typed calls, `NodeClient::on(node)` for forwarded calls, a transfer reader that checks hashes. |
| `vessel-mcp` | The tools take `node` (below). |
| `vessel-cli` | `vessel session move|copy --to NODE [--cwd]`, `vessel fleet memory copy`, `vessel fleet secret set|list|delete|copy`, `--node` on a fleet node through forwarding. |

Tracks: F3 the secret rows; F4 the transfers and imports; X the envelope,
the mutual grant, `fleet.peers`/`nodes`, `session.read` waiting and the
tools. F4's cross-node pull depends on X's envelope; within one node
(another profile) it does not.

### `vessel mcp` across nodes

- `list_nodes` calls `fleet.nodes`. Every other tool takes `node`
  (default: this node); a call for another node goes in `fleet.forward`.
- `wait_turn` becomes a loop of `session.read { from, wait: 30 }` until a
  turn ends, a permission is asked or the session ends. A subscription
  across two hops would need the home to relay a stream per waiting agent,
  with lag and resume on each hop; a long poll returns within one round
  trip of a record landing, holds one call open per waiting agent, and
  needs nothing new in `vessel-rpc`. The local case uses the same loop, so
  the tool has one code path.
- `read_events` is `session.read` (paging) as now; `create_session`,
  `send_input`, `answer_permission` and `kill_session` are forwarded as
  they are, and the target applies ownership to the agent.

### What a protocol 5 peer sees

- It knows none of the new methods; a protocol 6 client refuses to send
  them to a protocol 5 node (`Call::since`), and `session.read` with
  `wait` likewise, so `vessel-client` falls back to polling there.
- New optional `SessionInfo` fields are ignored when decoded (session
  structs do not reject unknown fields), and `created_by` uses only
  variants it knows, so no stand-in is needed.
- In a mixed fleet the home lists a protocol 5 member with its version in
  `fleet.peers`, and refuses to deliver to it (`unsupported`, naming the
  version): delivering without the envelope would lose the origin, and
  with it ownership and limits. It still serves the bundle to it.

### Alternatives weighed

- **Who moves the files.** A CLI-mediated push (download from the source
  to the person's machine, upload to the target) works without a fleet,
  but needs an upload channel, sends the data over the person's link
  twice, and is out of reach of agents, which only reach their own node. A
  source push through the home needs upload calls too, with the target's
  policy checked per write. **Target pull through the home** needs only
  reads, which the bundle already has, serves CLI and agents with one call
  (`session.import` on the target), and a direct link (a mesh, ADR 0013
  option 1) can replace the hop later without changing the calls. Cost:
  two hops per chunk until then.
- **Snapshot or live files.** Reading a live transcript races the harness
  appending to it; a snapshot at `export` (copied, not linked: an append
  would change a link too) gives fixed hashes and resumable reads, for
  the disk it takes until closed or expired.
- **One transfer mechanism or two.** Sessions and memory are both "a set
  of files under a harness home"; only what is chosen and where it lands
  differ. One manifest and chunk shape (the bundle's) serves both, and
  scopes differ by the opening row (`session.export` operate,
  `memory.export` admin) rather than inside one method.
- **An envelope or a `node` field on every request.** A field on each
  request would change every type and leave the guard to rediscover the
  intersection per method. Three rows (forward, relay, deliver), each with
  a real static scope for its own hop, keep the generated guard as it is
  and put the intersection in one place.
- **Relayed subscriptions or a long-poll read.** See `vessel mcp` above;
  `fleet.watch` already chose the long poll for the same reason.
- **`secret.get`, sealed values, or copy by the source.** A `get` would
  make every admin client a way to read secrets; sealing a value to the
  target's key would need a second Noise pattern (against ADR 0009's one
  mode) to hide it from a home that can already call the target with its
  own credential. The source sending `secret.set` keeps values in node
  stores only.
- **The mutual grant as a credential the member mints and sends, or as a
  pairing link.** A link reuses ADR 0010 end to end (single use, expiry,
  scopes, revocation, the device list) and needs no second credential
  format.
- **`fleet.push`: dropped.** It would be the protocol's only upload, and
  would write into the person's own source files on the home, the one
  place Vessel never writes (ADR 0012: the source is never overwritten
  automatically). The home's sources are ordinary files the person edits
  there, or with their usual tools over SSH. If it is wanted later, it is
  `transfer` in the other direction, on the same shapes.

### Open points for the review

- Whether a member's `sync` credential on the home should also cover
  relaying (`fleet.relay` at `Sync`), or relaying should be a scope of its
  own that the home's owner grants per member.
- Codex's `session/load` after an import into another working directory:
  the rollout records its first working directory; whether `codex-acp`
  resumes under a new one needs checking with throwaway homes.
- Limits for transfers (largest file, total size, files per manifest) and
  where they live (`Config`).

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
