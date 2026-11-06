# ADR 0011: Harness profiles

Status: proposed, 2026-11-06 (milestone 4, `docs/plans/backend-m4.md`).

## Context

`vesseld` passes its own environment to every harness it starts
(`docs/ARCHITECTURE.md`, State). For Claude Code that environment decides
the account: `CLAUDE_CONFIG_DIR` points at a configuration directory with
its own login, settings, transcripts and memory. Whoever started the node
therefore chose the account for every session it will ever run.

The operator's real setup (read 2026-11-06, read-only) runs four Claude Code
logins side by side on one machine and moves work between them:

- `~/.claude`, `~/.claude-alt`, `~/.claude-alalt`, `~/.claude-alalalt`, one
  per login, selected with `CLAUDE_CONFIG_DIR`.
- In Paseo each is a provider that extends the built-in one:
  `{ "extends": "claude", "label": ..., "env": { "CLAUDE_CONFIG_DIR": ... } }`
  (`teamwicked-infra/paseo-claude-account-swap/README.md`, "Add another
  account"). Adding an account is configuration, not code.
- When one account reaches its usage limit, a plugin moves a running agent
  to another account without losing the conversation. It hard-links the
  session's transcript files into the target directory (never `memory/`),
  points the agent's `CLAUDE_CONFIG_DIR` at the target, and restarts only the
  Claude process, which resumes the same session.
- Memory, settings and subagents stay per account on purpose; instructions
  and skills are shared (`~/.claude/CLAUDE.md` section 8).

How Paseo's `extends` works (installed daemon 0.15.1, read 2026-11-06;
`agent/provider-registry.js`, `agent/provider-launch-config.js`,
`protocol/dist/provider-config.js`):

- A provider entry may extend only a built-in provider, never another
  custom one, and must then carry a label.
- It inherits the base's modes, client and settings. `env` is the base's
  merged with its own; a `command` replaces the base's argv whole; model
  lists are not inherited.
- The environment a harness sees is, later winning: the daemon's own,
  then the provider's `env`, then what the creating call passed (stored
  with the agent), changed by plugin hooks.
- Separately, "agent profiles" are presets (provider, model, mode) that an
  agent copies into its own create call; they change no environment.

Vessel needs the same: one harness, several accounts or configurations,
chosen per session, and a session that keeps its account when it is resumed.

## Options

### Where profiles are defined

1. **Node configuration**: a `profiles.toml` in the node's data directory.
   Paths and accounts differ per machine, so a profile belongs to the
   machine that runs it.
2. **The client's book**: the client sends the environment with each
   `session.create`. Every client would need to know each node's paths, and
   any client could set any variable on the node (`LD_PRELOAD`).
3. **Fleet-wide templates** (ADR 0012) rendered per node. Useful for
   "every machine has a `claude-work` profile", but still materialised as
   option 1 on each node.

### What a profile may change

1. **Environment only** (Paseo's `extends` with `env`): set or remove
   variables on top of the node's own environment.
2. **Environment plus arguments**: extra harness arguments (a default
   model, `--settings`), terminal mode only, since structured mode goes
   through ACP.
3. **A different binary**: a profile per installed version. Overlaps with
   the harness lookup, and invites arbitrary commands.

### Moving a live session to another profile

1. **Not at all**: a new session on the other profile.
2. **Through the harness's own resume**: for harnesses whose conversation
   is a file under the profile's directory (Claude Code), hard-link the
   session's files into the target profile's directory and restart the
   harness there with ACP `session/load`. What the operator's plugin does.
3. **Copy the transcript**: copies diverge if the old process is still
   writing.

## Decision

### Profiles in node configuration (option 1), environment and arguments (option 2)

```toml
# <data_dir>/profiles.toml (owner-only, like `token`)
[profiles.claude-alt]
harness = "claude"                 # the harness it extends; one of `harness.list`
label = "Claude (Alt)"
env = { CLAUDE_CONFIG_DIR = "~/.claude-alt" }
unset = ["ANTHROPIC_API_KEY"]      # removed from the inherited environment
args = ["--model", "opus"]         # terminal mode only, before the session's own

[profiles.codex-work]
harness = "codex"
env = { CODEX_HOME = "~/.codex-work" }
```

- Every harness has an implicit default profile with its own name
  (`claude`, `codex`, `gemini`, `opencode`): today's behaviour, the node's
  environment as is. Existing clients and stored sessions mean the default.
- A profile extends exactly one harness, never another profile: one level,
  so what a session runs with is readable from one table.
- `~/` expands against the node user's home. Values are text; no command
  substitution.
- The file is read at start and again on `profile.reload` (admin scope).
  A broken file stops the node from starting; on reload it leaves the
  previous profiles in place and returns the error.
- **Secrets stay on the node.** `profile.list` returns names, labels,
  harnesses and the *names* of the variables a profile sets, never their
  values: an `ANTHROPIC_API_KEY` in a profile does not reach clients.
- Which variables the harness descriptor considers its "home"
  (`CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME` for Codex, the
  defaults when unset) is harness knowledge, in `vessel-harness`, as all
  harness knowledge is (`docs/LESSONS.md`). Fleet sync (ADR 0012) and the
  profile move below need it.

### Sessions record their profile

- `SessionSpec` gains `profile` (optional; additive, protocol 5). The node
  refuses a profile whose harness differs from `spec.harness`.
- `SessionInfo` records the profile, so the log shows which account ran
  what, and a resumed structured session (`--resume`, ACP `session/load`)
  runs under the profile that holds its transcript.
- `vessel run --profile claude-alt ...`; `vessel profiles` lists them.

### Moving a session (option 2), structured Claude Code first

- `session.move_profile { session, profile }` (operate scope), for
  harnesses whose descriptor says how their session files are laid out.
  Claude Code: `<home>/projects/<project>/<session id>.jsonl`, the
  `<session id>/` directory beside it, and `file-history/<session id>/`.
- Refused while a turn runs (unless `force`, which cancels the turn first),
  and when the target profile is not logged in (the descriptor's check;
  for Claude Code, `claude auth status`).
- The node hard-links those files into the target profile's home (same
  inode, so both accounts append to one transcript), stops the harness
  process, and starts it under the target profile with `session/load`. The
  Vessel session keeps its id, its log and its subscribers; a
  `ProfileChanged { from, to }` event records the move.
- `memory/` is never linked: memory stays per profile.
- Terminal sessions cannot move: the harness's own TUI owns its state.

## Why

- **Per-machine definitions** match reality: account directories, logins
  and API keys live on the machine that runs the harness. Templates for the
  fleet are a convenience on top (ADR 0012), not the source.
- **Environment plus arguments** covers every real case seen (accounts,
  config homes, a default model) without letting a profile run an
  arbitrary program; the binary still comes from the harness lookup.
- **No client-supplied environment**: environment variables are code
  injection on the node (`LD_PRELOAD`, `NODE_OPTIONS`). Paseo lets the
  creating call pass environment; Vessel does not. Clients pick a profile
  by name; the node owner defines what the names mean.
- **The move uses the harness's own resume**, which the operator verified
  works across accounts (signed thinking blocks and tool calls included,
  2026-10-25), and hard links avoid the divergence of copies.

## Consequences

- Protocol 5: `SessionSpec.profile`, `SessionInfo.profile`,
  `profile.list` (read), `profile.reload` (admin),
  `session.move_profile` (operate), the `ProfileChanged` event. All
  additive.
- `vessel-harness` gains `Profile`, applied in `launch`, and per-harness
  knowledge of the home variable, the session file layout and the login
  check.
- The node keeps `profiles.toml` owner-only and warns about a file others
  can read, as for `token` and `node.key`.
- A session created before protocol 5 runs on the default profile; nothing
  stored changes meaning.
- Switching accounts at a usage limit becomes a node operation any client
  can trigger (the CLI, the web UI later, or an agent with the scope),
  instead of a plugin bound to one product.
