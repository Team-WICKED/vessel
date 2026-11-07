#!/usr/bin/env bash
# Milestone 5 smoke S1: a structured Claude Code session on a model route to
# a non-Anthropic model, through the node's proxy. See docs/smoke/README.md.
#
#   docs/smoke/model-route.sh --base-url URL --model MODEL [--small-model MODEL] \
#       [--api API] [--bin DIR] [--deadline S] [--keep]  <<<"$PROVIDER_KEY"
#   docs/smoke/model-route.sh --fake   # the same steps with the testkit fakes
#
# The provider's key is read from standard input (typed at a prompt, not
# shown, or piped in), never from an argument, and goes only into the
# throwaway node's secrets.toml; the script never prints it. The script
# starts its own node on a free loopback port with a temporary data
# directory and a temporary Claude Code home (so no account of yours is
# used: the session authenticates to the node's proxy with its own token),
# and leaves your own node, book and data directory alone.
#
# What it checks: the node lists the route; a structured Claude Code
# session on it creates a file and then edits it (tool calls, streaming, a
# second turn); the session's log has a model call per request, with token
# counts; and the key appears nowhere the run wrote but secrets.toml (the
# node's log, the event store, the Claude Code home, the working directory,
# and on Linux the harness processes' environments).
#
# --fake runs the same steps with fake-acp as Claude Code's adapter and
# fake-provider as the provider (an OpenAI-compatible stream on loopback),
# with a made-up key: no account and no key are involved. Use it to check
# the script itself after a change.
#
# Options:
#   --base-url URL   the provider's base URL (e.g. https://openrouter.ai/api/v1)
#   --model MODEL    the provider's model id for the route
#   --small-model M  the provider's model for Claude Code's small, fast calls
#                    (default: the model)
#   --api API        openai-chat-completions (default), openai-responses or
#                    anthropic-messages
#   --bin DIR        where vessel and vesseld are (default: the workspace's
#                    target/release; with --fake also fake-acp, fake-tui and
#                    fake-provider)
#   --deadline S     seconds to wait for each step (default 300)
#   --keep           keep the temporary directory
set -euo pipefail

base_url=""
model=""
small_model=""
api="openai-chat-completions"
bin=""
deadline=300
keep=0
fake=0

die() { printf 'model-route smoke: %s\n' "$*" >&2; exit 1; }
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --base-url) base_url=${2:?--base-url needs a value}; shift ;;
        --model) model=${2:?--model needs a value}; shift ;;
        --small-model) small_model=${2:?--small-model needs a value}; shift ;;
        --api) api=${2:?--api needs a value}; shift ;;
        --bin) bin=${2:?--bin needs a value}; shift ;;
        --deadline) deadline=${2:?--deadline needs a value}; shift ;;
        --keep) keep=1 ;;
        --fake) fake=1 ;;
        -h|--help) sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done

case "$api" in
    openai-chat-completions|openai-responses|anthropic-messages) ;;
    *) die "--api: one of openai-chat-completions, openai-responses, anthropic-messages" ;;
esac
if [ "$fake" = 1 ]; then
    [ -z "$base_url" ] && [ -z "$model" ] || die "--fake makes its own provider; leave out --base-url and --model"
    model="fake/coder"
    api="openai-chat-completions"
else
    [ -n "$base_url" ] || die "--base-url is required (see --help)"
    [ -n "$model" ] || die "--model is required (see --help)"
fi
[ -n "$small_model" ] || small_model=$model
for value in "$base_url" "$model" "$small_model"; do
    case "$value" in *'"'*|*"\\"*) die "quotes and backslashes are not supported in $value" ;; esac
done

if [ -z "$bin" ]; then
    command -v cargo >/dev/null || die "no --bin given and no cargo to find the target directory"
    target=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
    bin=$target/release
fi
bin=$(cd "$bin" && pwd) || die "no such directory: $bin"
needed=(vessel vesseld)
[ "$fake" = 1 ] && needed+=(fake-tui fake-acp fake-provider)
for program in "${needed[@]}"; do
    [ -x "$bin/$program" ] || die "$bin/$program is missing: build with \`cargo build --release --workspace --bins\`, or pass --bin"
done

# --- The key, from standard input only.
if [ "$fake" = 1 ]; then
    key="fake-provider-key-$RANDOM$RANDOM$RANDOM"
elif [ -t 0 ]; then
    printf 'provider key (not shown): ' >&2
    IFS= read -r -s key || true
    printf '\n' >&2
else
    IFS= read -r key || true
fi
[ -n "$key" ] || die "no provider key on standard input"
[ ${#key} -ge 8 ] || die "the key is too short to search for safely"

root=$(mktemp -d "${TMPDIR:-/tmp}/vessel-model-smoke.XXXXXX")
chmod 700 "$root"
node_pid=""
provider_pid=""
passed=0
cleanup() {
    for pid in "$node_pid" "$provider_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [ "$passed" = 0 ] && [ -f "$root/vesseld.log" ]; then
        printf '\n--- last lines of vesseld.log\n' >&2
        tail -n 40 "$root/vesseld.log" >&2
    fi
    if [ "$keep" = 1 ]; then say "kept $root"; else rm -rf "$root"; fi
}
trap cleanup EXIT

# The environment: yours (the harness needs it), without anything that
# would point vessel at your own node, book or data, pick an account, or
# give Claude Code a key or endpoint of its own.
clean=(env -u VESSEL_NODE -u VESSEL_TOKEN -u VESSEL_DATA_DIR -u VESSEL_NAME
    -u VESSEL_BIND -u CLAUDE_CONFIG_DIR -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN
    -u ANTHROPIC_BASE_URL VESSEL_CONFIG_DIR="$root/config")

if [ "$fake" = 1 ]; then
    fakes=$root/fakes
    mkdir -p "$fakes"
    ln -s "$bin/fake-tui" "$fakes/claude"
    ln -s "$bin/fake-acp" "$fakes/claude-agent-acp"
    mkdir -p "$root/home"
    clean=(env -i HOME="$root/home" PATH="$fakes:/usr/bin:/bin" LANG=C.UTF-8
        TMPDIR="$root" VESSEL_CONFIG_DIR="$root/config")
    FAKE_PROVIDER_KEY=$key "$bin/fake-provider" >"$root/provider.out" 2>"$root/provider.err" &
    provider_pid=$!
    for _ in $(seq 1 50); do
        grep -q '^listening ' "$root/provider.out" 2>/dev/null && break
        sleep 0.1
    done
    port=$(sed -n 's/^listening //p' "$root/provider.out")
    [ -n "$port" ] || die "fake-provider did not start: $(cat "$root/provider.err")"
    base_url="http://127.0.0.1:$port/v1"
else
    command -v claude-agent-acp >/dev/null \
        || die "claude-agent-acp is not on PATH (structured Claude Code sessions need it)"
fi

# --- The node's files: the route, a profile naming it, its key.
data=$root/data
claude_home=$root/claude-home
mkdir -m 700 "$data"
mkdir -p "$claude_home"
# File edits in the working directory go through without asking: the
# script cannot answer a permission prompt (only `vessel attach` does).
printf '{ "permissions": { "defaultMode": "acceptEdits" } }\n' >"$claude_home/settings.json"
(
    umask 077
    cat >"$data/models.toml" <<EOF
[providers.smoke]
api = "$api"
base_url = "$base_url"
key = { secret = "SMOKE_PROVIDER_KEY" }

[models.smoke]
provider = "smoke"
model = "$model"
small_model = "$small_model"
EOF
    cat >"$data/profiles.toml" <<EOF
[profiles.claude-routed]
harness = "claude"
model = "smoke"
env = { CLAUDE_CONFIG_DIR = '$claude_home' }
EOF
)
vessel_offline() { "${clean[@]}" "$bin/vessel" --data-dir "$data" "$@"; }
printf '%s' "$key" | vessel_offline fleet secret set SMOKE_PROVIDER_KEY >/dev/null

# --- The node, on a free loopback port.
port=""
for _ in $(seq 1 50); do
    candidate=$((20000 + RANDOM % 20000))
    if ! (exec 3<>"/dev/tcp/127.0.0.1/$candidate") 2>/dev/null; then port=$candidate; break; fi
done
[ -n "$port" ] || die "no free port found"
"${clean[@]}" "$bin/vesseld" --data-dir "$data" --bind "127.0.0.1:$port" --name smoke-models \
    >"$root/vesseld.log" 2>&1 &
node_pid=$!
vessel() { "${clean[@]}" "$bin/vessel" --node "127.0.0.1:$port" --data-dir "$data" "$@"; }

until_true() {
    local what=$1
    shift
    local waited=0
    until "$@" >/dev/null 2>&1; do
        waited=$((waited + 1))
        [ "$waited" -le "$deadline" ] || die "gave up after ${deadline}s: $what"
        sleep 1
    done
}
until_true "the node answering" vessel status
say "node on 127.0.0.1:$port, data in $data"

vessel models | tee "$root/models.txt" | sed 's/^/    /'
grep -q '^smoke ' "$root/models.txt" || die "the node does not list the route"
vessel profiles | tee "$root/profiles.txt" | sed 's/^/    /'
grep -q '^claude-routed .* smoke ' "$root/profiles.txt" || die "the profile does not show its route"

# --- A structured session on the route.
work=$root/work
mkdir -p "$work"
id=$(vessel run claude --structured --profile claude-routed --cwd "$work" --detach)
say "session $id on route smoke ($model)"
log() { vessel log "$id" 2>&1; }
turns() { log | grep -cE '^\[turn (ended|failed)' || true; }
ready_or_failed() {
    local text
    text=$(log)
    if printf '%s\n' "$text" | grep '^\[session failed' | tail -n 1 >"$root/failed" \
        && [ -s "$root/failed" ]; then
        return 0
    fi
    printf '%s\n' "$text" | grep -F -- "[ready: harness session" >/dev/null
}
until_true "the session becoming ready" ready_or_failed
if [ -s "$root/failed" ]; then
    die "the session failed before it was ready: $(cat "$root/failed")"
fi

turns_before=0
more_turns() { [ "$(turns)" -gt "$turns_before" ]; }
ask() {
    local after
    turns_before=$(turns)
    vessel send "$id" "$1" >/dev/null
    until_true "the turn after: $1" more_turns
    after=$(log)
    if printf '%s\n' "$after" | tail -n 3 | grep -q '^\[turn failed'; then
        die "the turn failed: $(printf '%s\n' "$after" | tail -n 3)"
    fi
    printf '%s\n' "$after" | awk -v prompt="> $1" '$0 == prompt { reply = ""; next } { reply = reply $0 "\n" } END { printf "%s", reply }'
}

words=(marmalade lighthouse tangerine quokka harpsichord)
first=${words[$((RANDOM % ${#words[@]}))]}
second=${words[$(((RANDOM + 1) % ${#words[@]}))]}
if [ "$fake" = 1 ]; then
    # The fake calls the proxy itself when asked `model <text>`; it makes
    # no files, so the script writes them as the agent would have.
    reply=$(ask "model write $first")
    case "$reply" in *"model fake reply to: write $first"*) ;; *) die "the fake's call failed: $reply" ;; esac
    printf '%s\n' "$first" >"$work/smoke.txt"
    reply=$(ask "model edit $second")
    case "$reply" in *"model fake reply to: edit $second"*) ;; *) die "the fake's call failed: $reply" ;; esac
    printf '%s\n' "$second" >>"$work/smoke.txt"
else
    reply=$(ask "Create a file named smoke.txt in the current directory containing exactly one line: $first. Then reply with DONE only.")
    say "first turn: $(printf '%s' "$reply" | tr '\n' ' ' | cut -c 1-80)"
    reply=$(ask "Edit smoke.txt so that it has a second line: $second. Keep the first line. Then reply with DONE only.")
    say "second turn: $(printf '%s' "$reply" | tr '\n' ' ' | cut -c 1-80)"
fi
[ -f "$work/smoke.txt" ] || die "the agent made no smoke.txt"
grep -qx -- "$first" "$work/smoke.txt" || die "smoke.txt does not hold $first: $(cat "$work/smoke.txt")"
grep -qx -- "$second" "$work/smoke.txt" || die "smoke.txt does not hold $second: $(cat "$work/smoke.txt")"
say "smoke.txt was created and edited"

# --- The model calls, metadata only.
log | grep -F '[model smoke ' >"$root/calls.txt" || true
calls=$(wc -l <"$root/calls.txt")
[ "$calls" -ge 2 ] || die "expected at least two model calls in the log, found $calls"
say "model calls ($calls):"
sed 's/^/    /' "$root/calls.txt"
if grep -F '[model smoke ' "$root/calls.txt" | grep -q 'failed'; then
    say "note: some calls failed (see above); the turns still ended"
fi

# --- The key: nowhere but secrets.toml.
harness_envs=$root/harness-environments
: >"$harness_envs"
if [ -d /proc ]; then
    for dir in /proc/[0-9]*; do
        [ "$(readlink "$dir/cwd" 2>/dev/null)" = "$work" ] || continue
        tr '\0' '\n' <"$dir/environ" >>"$harness_envs" 2>/dev/null || true
    done
fi
vessel kill "$id" >/dev/null 2>&1 || true
kill "$node_pid" 2>/dev/null || true
wait "$node_pid" 2>/dev/null || true
node_pid=""
leaks=$(grep -rlF --exclude=secrets.toml -- "$key" "$root" 2>/dev/null || true)
[ -z "$leaks" ] || die "the provider key was found in: $leaks"
grep -q '^ANTHROPIC_AUTH_TOKEN=vslm-' "$harness_envs" \
    || [ ! -s "$harness_envs" ] \
    || die "the harness's environment has no session token"
say "the key appears only in secrets.toml (searched the node's log and data, the Claude Code home, the working directory$([ -s "$harness_envs" ] && printf ' and the harness environments'))"

passed=1
say "PASS: a Claude Code session on route smoke ($model), two turns, $calls model calls"
