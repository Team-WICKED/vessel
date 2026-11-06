# ADR 0013: Agent principals and cross-node sessions

Status: accepted 2026-11-06 with the coordinator decisions in `docs/plans/backend-m4.md`; drafted as proposed, 2026-11-06 (milestone 4, `docs/plans/backend-m4.md`).

## Context

"Every agent sees every other agent" (VISION section 3): an agent in a
session on one node lists the fleet's sessions, starts a session on
another node (build on the Linux box, test on the Mac), sends it work,
reads its result. The vision names the hard part: identity, authorization,
and what one agent may do on another node.

Today an agent cannot call the node at all. ADR 0005 noted that agents
calling back would need principals of their own; ADR 0010 gave devices
principals and scopes (`read`, `operate`, `admin`), checked for every call
by one generated guard, and closed a device's connections on revocation.

What Paseo does (installed daemon 0.15.1, read 2026-11-06; paths under
`@getpaseo/server/dist/server/server/`):

- It injects an MCP server named `paseo` into every agent:
  `http://127.0.0.1:<port>/mcp/agents?callerAgentId=<agent id>`, with one
  bearer token that is random per daemon run and shared by every agent
  (`agent/runtime-mcp-config.js`, `bootstrap.js`).
- The caller is whoever the query parameter names; nothing checks it. With
  no daemon password the endpoint accepts anyone who reaches the port
  (`auth.js`, `isAgentMcpRequestAuthorized` returns true without one).
- The tools have no ownership check: `kill_agent`, `send_agent_prompt` and
  `archive_agent` take any agent id (`agent/tools/paseo-tools.js`). The
  permission table in `authorization/` covers app connections, not these
  tools. A provider can only switch tools off.
- Seven tools take a `serverId` and route through connected hosts to other
  daemons; children are labelled with their parent for notifications.
- Any process of the same OS user can also read the per-start
  `local-credential` file and connect as the owner.

Its docs describe intersecting a caller's grants with a link's for calls
between daemons; that idea is worth keeping (`docs/LESSONS.md`).

What a per-agent identity can and cannot be on one machine must be said
plainly: every session runs as the same operating-system user as the node.
Such a process can read another's environment (`/proc/<pid>/environ`) and
files, the node's own token (`~/.vessel/token`) included, which gives full
owner rights exactly as Paseo's `local-credential` does. Per-agent identity
is therefore attribution and least privilege against a confused or
prompt-injected agent, not containment of a hostile one. Containment needs separate users or sandboxes, a later milestone.

## Options

### How an agent proves who it is

1. **A claimed id** (Paseo): simple; anyone can claim any agent.
2. **The process tree**: the node reads the connecting process's id from
   the local socket (`SO_PEERCRED`, `LOCAL_PEERPID`, the named pipe's
   client id) and finds which session's harness it descends from. No secret
   at all, but different on every platform, exposed to pid reuse, and
   meaningless across nodes.
3. **A per-session credential** the node mints when it starts the session
   and hands to that harness only, presented on every connection like a
   device credential.
4. **A key pair per session** and a Noise pattern that authenticates the
   client (XK). A second handshake pattern, against ADR 0009's one-mode
   rule, for no gain over option 3 under one OS user.

### What an agent may do

1. **Everything its node may do**: the node token's authority.
2. **Scopes, as devices** (ADR 0010).
3. **Scopes plus ownership**: an agent operates only on sessions it
   created (and theirs), unless granted more; it can see more than it can
   touch.

### How a call reaches another node

1. **Each node pairs with every other** (a mesh): n² credentials.
2. **Through the fleet's home** (ADR 0012), which already holds a
   credential on every member: calls go agent → its node → home → target.
3. **The agent connects to the other node itself**, with a credential of
   its own there: every agent would need pairing with every node.

## Decision

### A credential per session (option 3)

- When it starts a session, the node mints a 256-bit credential bound to
  that session, keeps its hash in memory, and hands it only to that
  session's `vessel mcp` (below), in the MCP server definition's
  environment: inside ACP `session/new` for structured sessions, in the
  owner-only per-session MCP file for terminal ones (ADR 0012). Never in the
  harness's own environment, never on a command line.
- The agent reaches its node through `vessel mcp`, a small MCP server
  (stdio) that the node attaches to every session at launch (ADR 0012,
  launch-time MCP). It connects to the node's local socket and presents
  `{ "type": "agent", "session", "secret" }` as the socket's preamble
  instead of the node token. The node checks it in constant time.
- The credential dies with the session: an ended, killed or interrupted
  session's credential is refused, and its open connections close, the
  way a revoked device's do.

### Scopes plus ownership (option 3)

- An agent principal is `Agent { node, session, scopes, owned_only }`.
- Default: `read` and `operate`, owned only: the agent sees the fleet's
  nodes and sessions, and creates, sends to, reads and kills the sessions it
  started and their descendants. It cannot touch the user's own sessions or
  another agent's.
- Never `admin`: no agent stops a node, invites devices or changes the
  fleet unless its profile grants it explicitly (`[profiles.X.agent]` in
  `profiles.toml`, ADR 0011).
- **Grants only shrink.** A session created by a principal gets at most
  that principal's scopes: a device with `read` cannot start a session whose
  agent operates. A session created by an agent is owned by that agent.
- `SessionInfo` records `created_by` (the user, a device, or an agent with
  its node and session), so every session's origin is visible and
  ownership is checkable.
- Limits per agent: at most 8 live sessions it owns, and at most 3 levels
  of agents starting agents. Both are counted by the node that holds the
  sessions.

### Across nodes through the home (option 2)

- `fleet.forward { node, call }` sends one call to a member through the
  home. The home calls the member with its own credential there (ADR 0012,
  mutual join) and states on whose behalf: the agent's node, session and
  the chain of creators.
- The target computes the effective authority as the intersection of the
  home's grant on it, the agent's scopes, and ownership on its side.
  Nobody can exceed what the target's owner granted the home.
- Sessions started this way record the full chain in `created_by`, and the
  depth limit applies along it.
- A mesh (option 1) can replace the hop later without changing what an
  agent sees: the calls and the intersection rule stay.

### What agents get as tools

`vessel mcp` offers: list nodes, list sessions (any node), create a
session (node, profile, working directory, first prompt), send input,
read events, wait until a turn ends, kill. Each tool is one protocol call
through the agent's own principal; the server holds no authority of its
own.

## Why

- **A credential, not a claim**: the call says who it is in a way another
  agent cannot forge by guessing an id, and the node can attribute and
  limit each call. Unlike the process tree, it works the same on every
  platform and carries across nodes.
- **Through `vessel mcp` only**: the harness never sees the secret in its
  own environment, so a casual `env` dump in a transcript does not leak it
  (the honest limit above still applies to a hostile agent).
- **Ownership** gives the useful default, "agents manage the work they
  started", without a permission language.
- **Grants only shrink**, as in Paseo's documented intersection, so
  delegation can never escalate.
- **The home hop** reuses the membership ADR 0012 already sets up and keeps
  every cross-node call auditable in one place.

## Consequences

- Protocol 5: the agent preamble on the local socket, `created_by` on
  `SessionInfo`, `fleet.forward`, and the ownership rule in the guard for
  agent principals (a method's scope still comes from the table; ownership
  is checked for `operate` methods on a session).
- The node keeps live session credentials in memory; a restart voids them
  with the sessions it interrupts.
- A new crate, `vessel-mcp`: the MCP server, on `vessel-client`.
- Per-agent identity under one OS user is accounting, not isolation; this
  ADR says so, and the plan lists sandboxing as later work.

## As built (Track H, H4, 2026-11-06)

The decision stands, on one node; cross-node calls (`fleet.forward`, the
creator chain) are Track X.

- **Credential.** Minted by `session.create` before the session starts:
  the session runtime takes its id from the node (`LaunchOptions::id`), so
  the credential is bound to it. 256 random bits; the node keeps only its
  SHA-256 (`CredentialHash`), in memory, compared in constant time.
- **Where it goes.** The `vessel` MCP server (`Config::vessel_mcp`; `vessel
  node start` uses its own executable, `vesseld` a `vessel` beside it):
  `vessel mcp`, with `VESSEL_NODE_SOCKET`, `VESSEL_AGENT_SESSION` and
  `VESSEL_AGENT_SECRET` in that server's environment. A structured
  session's agent gets it over ACP, Claude Code's terminal in the
  owner-only MCP file. Codex's terminal form puts server values in Codex's
  own environment (ADR 0012, as built), so there the credential is in the
  harness's environment after all; its name ends in `SECRET`, which keeps
  Codex from passing it to the commands its agent runs. A node without
  `vessel_mcp` mints no credentials; limits and depth still apply.
- **Admission.** On the local socket only, a JSON object as the first
  frame. Anything wrong (unknown session, wrong secret, ended session) is
  the same `rejected` as a wrong token. The check and the registration of
  the connection share one lock, and the session's end cancels the
  registered connections, as revoking a device does.
- **Grants.** The agent's scopes are its creator's, at most `read` and
  `operate`: an owner's or a device's session's agent gets the
  intersection, an agent's session's agent the agent's. `owned_only` is
  always on; profile grants (`[profiles.X.agent]`) are not built yet.
- **Ownership.** Every request type declares the session an `operate` call
  acts on (`session.send`, `session.kill`, `session.move_profile`); the
  guard checks it after the scope, walking `created_by` up to the depth
  limit through live and stored sessions. Reads (`session.get`, `.read`,
  `.subscribe`, `.list`) are open to agents, which see the node's sessions.
  An agent does not own its own session.
- **Limits.** 8 live sessions an agent created itself (descendants count
  for their own creators), and depth 3 (a user's session is 0). Both are
  counted in the registry under one lock, so concurrent creations cannot
  overshoot. Refusals are `forbidden`.
- **Moves.** A profile move restarts the agent and its `vessel mcp`: the
  move gets a second credential, and when it ends the one in use stays.
- **`vessel mcp`.** On `rmcp` (plan decision 8; the check is recorded in
  `vessel-mcp`'s Decision Log). Tools: `list_nodes` (this node until Track
  X), `list_sessions`, `create_session` (with an optional first prompt,
  sent once the session is ready), `send_input` (returns `from`),
  `answer_permission`, `read_events`, `wait_turn` (from the record
  `send_input` or `create_session` returned, until the turn ends, a
  permission is asked, the session ends, or a timeout; with the reply
  text), `kill_session`. The node's refusals come back as tool errors with
  the protocol's code; a wrong call is a JSON-RPC error.
