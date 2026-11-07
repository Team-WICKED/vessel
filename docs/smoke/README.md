# Manual smokes

Checks a person runs by hand before a milestone closes, where the
automated tests cannot reach: real accounts, a fresh machine. Each script
prints what it does, stops at the first failure with the reason and the
end of the node's log, and ends with `PASS`.

Build first (every script defaults to the workspace's `target/release`;
pass `--bin DIR` for another build):

```bash
cargo build --release --workspace --bins
```

## Milestone 4

The plan's done criteria (`docs/plans/backend-m4.md`) ask for two smokes.

### (a) Four profiles on one node, a session moved between them

`profiles.sh` starts a throwaway node, gives it one profile per account
you name, starts a structured Claude Code session under the first, and
moves it to each next profile and back to the first. After every move it
asks the agent for a word it was told before the first move: the reply
must hold it, so the conversation came along.

```bash
docs/smoke/profiles.sh \
    --profile first=/path/to/first-account-home \
    --profile second=/path/to/second-account-home \
    --profile third=/path/to/third-account-home \
    --profile fourth=/path/to/fourth-account-home \
    --model haiku
```

- Each home is a Claude Code configuration directory (what
  `CLAUDE_CONFIG_DIR` names) with a logged-in account. Profile names and
  homes come only from these arguments; nothing about your accounts is
  stored anywhere but the script's temporary directory.
- Needs `claude-agent-acp` and `claude` on `PATH`. The node runs with your
  environment, minus anything that points `vessel` at your own node, book
  or data directory (`VESSEL_*`) and minus `CLAUDE_CONFIG_DIR`; it listens
  on a free loopback port with a temporary data directory, which is
  removed at the end (`--keep` keeps it).
- Costs: one short turn per account and one more on the first, each on
  that account's quota. `--model` picks the model by the ACP agent's own
  ids or aliases (`haiku`, `sonnet`, ...), not the API's model ids: a model
  the agent does not offer fails the session at the handshake, and the
  script stops there with the agent's list of what it offers.
- Two or more profiles work; the done criterion was drafted with four, and
  the operator's machine has three accounts.
- What stays: each move hard-links the conversation's files into the next
  account's home, as a move does (ADR 0011), so the smoke's conversation
  remains under `projects/` in every account's home, named after a
  temporary working directory. The script lists those directories at the
  end; remove them if you do not want them.
- `--fake` runs the same steps against the testkit fakes with four
  temporary homes (no account, no quota): use it to check the script
  itself after a change.

### (b) A fresh machine joins a fleet

`join.sh` sets up a throwaway home with sample instructions, a skill and
two MCP servers, then a "fresh machine" whose home directory holds four
account directories (two Claude Code, two Codex) and nothing of Vessel.
The fresh machine runs `vessel init --join` with the home's link, starts
its node, and the script checks, timing each step:

- `vessel init` made a profile for each extra account;
- the home took the fresh machine's grant through the home's relay, so it
  can call that node back (`--to`, `vessel mcp`'s `node`, `secret.copy`);
- the fleet's instructions (`CLAUDE.md`, `AGENTS.md`) and skill are in all
  four homes;
- both MCP servers reach a structured session in every profile, the one
  needing a secret with the value set on that node (read back from the
  harness, never printed);
- after a local edit to one home's instructions and a new version on the
  home, the edit is kept, the home's version is beside it as
  `CLAUDE.md.vessel-incoming`, the other homes follow, and `vessel fleet
  status` lists the edit.

```bash
docs/smoke/join.sh --docker              # in a throwaway container (needs Docker)
docs/smoke/join.sh --local               # or here, under a temporary HOME
```

- `--docker` runs both nodes in one container with no network (they talk
  over its loopback), the binaries mounted read-only, and the container
  removed at the end. The binaries need the build machine's C library or a
  newer one: the default image is `ubuntu:24.04` (`--image` for another);
  Docker pulls it if it is missing.
- `--local` runs the same steps here with nothing of your environment but
  the paths it creates: a temporary HOME for each node, loopback ports,
  and no `--service`, so nothing is installed into this machine's service
  manager and your own node, book and accounts are left alone.
- The harnesses are the testkit fakes under the real names (`claude`,
  `codex` and their ACP adapters), so no login or account is involved; a
  real harness found on the script's `PATH` stops it before it runs.
- A run takes about ten seconds once the binaries are built; the
  `--deadline` for each step defaults to two minutes.
- Expected in the output: `vessel init` lists Gemini CLI and OpenCode as
  not installed (offered, never run). The home runs with a relay on
  loopback (`vesseld relay`, a throwaway token), and the fresh machine's
  node, started with plain `vessel node start`, registers there too, with
  a ticket for its own key (never the relay's token), and the home takes
  its grant (`fleet.callback`); before, it had no route the
  home could call back on, and `vessel fleet status` said the home did
  not take its grant. A home with no relay still syncs its members, and
  `vessel init --join` and `vessel fleet status` say then that the home
  cannot call them and how to add a route.
- `--service` is not part of the run: in a container there is no user
  service manager, and on this machine it would install one. `vessel init
  --service` is covered by `vessel-cli`'s tests against a temporary home.

## Milestone 5

The plan's done criteria (`docs/plans/backend-m5.md`, "Manual smokes")
ask for S1; S2 is optional.

### S1: Claude Code on a route to another provider's model

`model-route.sh` starts a throwaway node with one model route to an
OpenAI-compatible provider and a Claude Code profile naming it, starts a
structured Claude Code session under that profile, and asks it to create a
file and then to edit it (tool calls, streaming, a second turn). It checks
the file, prints the session's model calls (route, model, tokens, how each
ended; never content), and searches everything the run wrote for the key.

```bash
docs/smoke/model-route.sh \
    --base-url https://openrouter.ai/api/v1 \
    --model qwen/qwen3-coder
# then type the provider's key at the prompt (not shown), or pipe it in:
#   read -rs KEY; printf '%s\n' "$KEY" | docs/smoke/model-route.sh ...
```

- The key is read from standard input only, never from an argument, and
  goes only into the throwaway node's `secrets.toml` (the script never
  prints it). At the end the script searches the node's log and data
  directory, the Claude Code home, the working directory and, on Linux,
  the harness processes' environments for it: it must be in
  `secrets.toml` only. The harness's environment must hold the session's
  model token (`vslm-...`) instead.
- No account of yours is used: the session gets a temporary Claude Code
  home (`CLAUDE_CONFIG_DIR`) and authenticates to the node's proxy with
  its own token. That home's `settings.json` sets `acceptEdits`, so the
  file edits go through without a permission prompt the script could not
  answer.
- `--api` picks the provider's API (`openai-chat-completions` by default,
  `openai-responses`, `anthropic-messages`); `--small-model` the model
  for Claude Code's small, fast calls (titles, summaries), the route's
  model by default.
- Needs `claude-agent-acp` and `claude` on `PATH`. The node runs with your
  environment, minus `VESSEL_*`, `CLAUDE_CONFIG_DIR` and the
  `ANTHROPIC_*` key and endpoint variables; it listens on a free loopback
  port with a temporary data directory, removed at the end (`--keep`
  keeps it).
- Costs: two short turns on the provider's model, plus whatever small
  calls Claude Code makes on its own.
- `--fake` runs the same steps with `fake-acp` as Claude Code's adapter
  and `fake-provider` as the provider (a canned stream on loopback that
  refuses any key but the one it was given), with a made-up key; the fake
  agent makes its calls on `model <text>` and the script writes the file
  itself. Use it to check the script after a change: it takes a few
  seconds.

### S2 (optional): Codex through an Anthropic route

Not scripted: a node with a route whose provider is `anthropic-messages`
(`https://api.anthropic.com`, the key as a secret), a Codex profile naming
it, and one turn of `vessel run codex --profile <it> --structured` with a
tool call; `vessel log` then shows the call's tokens.
