# ADR 0011: Harness profiles

Status: accepted 2026-11-06 with the coordinator decisions in `docs/plans/backend-m4.md`; drafted as proposed, 2026-11-06 (milestone 4, `docs/plans/backend-m4.md`).

## Context

`vesseld` passes its own environment to every harness it starts
(`docs/ARCHITECTURE.md`, State). For Claude Code that environment decides
the account: `CLAUDE_CONFIG_DIR` points at a configuration directory with
its own login, settings, transcripts and memory. Whoever started the node
therefore chose the account for every session it will ever run.

The operator's real setup (read 2026-11-06, read-only) runs four Claude Code
logins side by side on one machine and moves work between them:

- `~/.claude` and three more configuration directories beside it, one
  per login, selected with `CLAUDE_CONFIG_DIR`.
- In Paseo each is a provider that extends the built-in one:
  `{ "extends": "claude", "label": ..., "env": { "CLAUDE_CONFIG_DIR": ... } }`
  (the operator's account-swap plugin notes, "Add another account").
  Adding an account is configuration, not code.
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


## As built (Track H, harness side, 2026-11-06)

Refinements found while building `vessel-harness`; the decision above
stands.

- **Names.** A harness's default profile is named after its command
  (`claude`, `codex`, `gemini`, `opencode`). A profile name that parses as a
  harness (`claude`, `Claude-Code`, ...) is reserved, so `--profile claude`
  always means the default. Duplicate profile names are refused ignoring
  case. Variable names in `env` and `unset` are compared ignoring ASCII
  case (Windows treats them as one variable), so `Path` beside `PATH`, or a
  variable both set and removed, is an error on every system.
- **Errors never quote the file.** `profiles.toml` errors name the profile
  and the key, and give line and column without the source excerpt the
  TOML library would print, since a line of `env` may hold a secret and
  `profile.reload` returns the error to a client.
- **`~`** expands in `env` values and in `args`, only as `~` or a leading
  `~/` (as a shell would). A value using it on a node user without a home
  is an error.
- **`args` refusal.** Each harness descriptor says whether a profile may add
  terminal arguments; all four may today, so the refusal exists but has no
  case yet.
- **Home.** `home(profile, user_home, inherited)` also takes the node's own
  environment: a profile that neither sets nor removes the home variable
  (the default one included) runs in whatever home the node was started
  with, as the harness itself would. A relative value is reported as
  unknown. Gemini CLI and `OpenCode` have no confirmed variable that moves
  their whole home; their homes are the defaults (`~/.gemini`,
  `~/.config/opencode`).
- **Login check.** `login_check(lookup, profile)` runs with the profile's
  environment, so it checks that profile's account (`claude auth status`,
  `codex login status`; none for Gemini CLI and `OpenCode` yet). The
  commands come from `--help` only; whether a logged-out check exits
  non-zero was not tried, since that would touch an account.
- **Session files.** Claude Code's project name is its working directory
  with every UTF-16 unit other than an ASCII letter or digit replaced by
  `-`, cut at 200 characters with `-` and a base-36 hash appended when
  longer (checked against Claude Code 2.1.318's own function). Claude Code
  names the project after its resolved working directory, so on Unix the
  canonical directory is tried too. Codex's rollouts are found by id under
  `sessions/YYYY/MM/DD/`.
- **Linking.** `link_session` checks everything before linking anything: a
  path that is not plain and relative, or names `memory`, is refused; a
  symbolic link in either home's path is refused; symbolic links, special
  files and `memory` directories inside the session's directories are
  skipped and reported; a different file already in the target is a
  conflict, and the same file (a hard link made before) counts as already
  linked. File identity comes from `same-file` (device and inode on Unix,
  volume serial and file index on Windows, which `std` exposes only on
  nightly there); a target that is a symbolic link is refused before it is
  compared, since the comparison follows links.
- **Launch checks the profile.** The node resolves `spec.profile` (protocol
  5); `launch` refuses a profile of another harness (`ProfileMismatch`, the
  client's error) and a profile other than the one the spec names, or than
  the harness's default when it names none (`ProfileNotRequested`, the
  node's bug), so a session never runs as another account than recorded.
- **Launch result.** `launch` returns `Launch { command, mcp_file,
  unattached }` rather than a bare command: the caller has to write the MCP
  file and keep it for the session's life, and to know which servers a
  terminal harness could not take.
- **Moving (H2, 2026-11-06).** `session.move_profile` changes as little
  as possible when it fails, so its steps run in this order: refuse
  terminal sessions and resolve the target; run the target's login check
  with its environment (30 s at most; a check that does not answer counts
  as not logged in); hold the session's input (`conflict` until the move
  ends); ask the driver whether a turn runs (it answers after every
  command sent before, so a prompt already on its way counts) and refuse
  unless `force`, which cancels the turn and waits up to 10 s for it to
  end; link the files while the old agent still runs (a refusal or a
  conflict leaves everything as it was); stop the old agent (`SIGTERM`,
  `SIGKILL` after 5 s); start the new one with `session/load`, dropping its
  replay of the conversation (the log has it) until it is ready; publish
  `ProfileChanged`. If the new agent does not start, the previous command
  starts again on the same conversation and the move fails with
  `spawn_failed`. Trade-offs: the links made for a move whose agent did
  not start stay in the target home (they are the same files, and a retry
  finds them already linked), and the target home must exist, as it does
  once that profile has logged in. Moving back to a profile the session
  came from finds the transcript already linked, on every system.
