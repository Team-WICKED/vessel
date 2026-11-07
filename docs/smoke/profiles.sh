#!/usr/bin/env bash
# Milestone 4 smoke (a): several profiles of one harness on one node, and a
# structured Claude Code session moved from each to the next, keeping its
# conversation. See docs/smoke/README.md.
#
#   docs/smoke/profiles.sh --profile NAME=HOME [--profile NAME=HOME ...] \
#       [--model MODEL] [--bin DIR] [--deadline S] [--keep]
#   docs/smoke/profiles.sh --fake      # the same steps with the testkit fakes
#
# Each --profile names a profile and the Claude Code configuration directory
# it uses (its CLAUDE_CONFIG_DIR, one logged-in account each; four for the
# milestone's done criterion). The script starts its own node on a free
# loopback port with a temporary data directory, writes a profiles.toml
# with those profiles there, and leaves your own node, book and data
# directory alone.
#
# Real accounts: every turn uses that account's quota (--model picks a
# small model), and each move hard-links the conversation's files into the
# next account's directory, where they stay; the script lists them at the
# end.
#
# --fake runs everything against the testkit fakes with four temporary
# homes, to check the script itself; no account is involved.
#
# Options:
#   --model MODEL  the model for the session (structured mode)
#   --bin DIR      where vessel and vesseld are (default: the workspace's
#                  target/release; with --fake also fake-tui and fake-acp)
#   --deadline S   seconds to wait for each step (default 300)
#   --keep         keep the temporary directory
set -euo pipefail

profiles=()
model=""
bin=""
deadline=300
keep=0
fake=0

die() { printf 'profiles smoke: %s\n' "$*" >&2; exit 1; }
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --profile) profiles+=("${2:?--profile needs NAME=HOME}"); shift ;;
        --model) model=${2:?--model needs a value}; shift ;;
        --bin) bin=${2:?--bin needs a value}; shift ;;
        --deadline) deadline=${2:?--deadline needs a value}; shift ;;
        --keep) keep=1 ;;
        --fake) fake=1 ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done

if [ -z "$bin" ]; then
    command -v cargo >/dev/null || die "no --bin given and no cargo to find the target directory"
    target=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
    bin=$target/release
fi
bin=$(cd "$bin" && pwd) || die "no such directory: $bin"
needed=(vessel vesseld)
[ "$fake" = 1 ] && needed+=(fake-tui fake-acp)
for program in "${needed[@]}"; do
    [ -x "$bin/$program" ] || die "$bin/$program is missing: build with \`cargo build --release --workspace --bins\`, or pass --bin"
done

root=$(mktemp -d "${TMPDIR:-/tmp}/vessel-profiles-smoke.XXXXXX")
chmod 700 "$root"
node_pid=""
passed=0
cleanup() {
    if [ -n "$node_pid" ]; then
        kill "$node_pid" 2>/dev/null || true
        wait "$node_pid" 2>/dev/null || true
    fi
    if [ "$passed" = 0 ] && [ -f "$root/vesseld.log" ]; then
        printf '\n--- last lines of vesseld.log\n' >&2
        tail -n 40 "$root/vesseld.log" >&2
    fi
    if [ "$keep" = 1 ]; then say "kept $root"; else rm -rf "$root"; fi
}
trap cleanup EXIT

# The environment: yours (the harness needs it), without anything that
# would point vessel at your own node, book or data, or pick an account.
clean=(env -u VESSEL_NODE -u VESSEL_TOKEN -u VESSEL_DATA_DIR -u VESSEL_NAME
    -u VESSEL_BIND -u CLAUDE_CONFIG_DIR VESSEL_CONFIG_DIR="$root/config")

if [ "$fake" = 1 ]; then
    [ ${#profiles[@]} -eq 0 ] || die "--fake makes its own profiles; leave out --profile"
    fakes=$root/fakes
    mkdir -p "$fakes"
    ln -s "$bin/fake-tui" "$fakes/claude"
    ln -s "$bin/fake-acp" "$fakes/claude-agent-acp"
    for name in first second third fourth; do
        mkdir -p "$root/homes/$name"
        profiles+=("$name=$root/homes/$name")
    done
    clean=(env -i HOME="$root/homes" PATH="$fakes:/usr/bin:/bin" LANG=C.UTF-8
        TMPDIR="$root" VESSEL_CONFIG_DIR="$root/config")
else
    command -v claude-agent-acp >/dev/null \
        || die "claude-agent-acp is not on PATH (structured Claude Code sessions need it)"
fi
[ ${#profiles[@]} -ge 2 ] || die "name at least two profiles (--profile NAME=HOME); the done criterion uses four"
[ ${#profiles[@]} -eq 4 ] || say "note: ${#profiles[@]} profiles; the done criterion uses four"

# --- profiles.toml, from the arguments only.
data=$root/data
mkdir -m 700 "$data"
names=()
(
    umask 077
    for entry in "${profiles[@]}"; do
        name=${entry%%=*}
        dir=${entry#*=}
        if [ "$name" = "$entry" ] || [ -z "$name" ] || [ -z "$dir" ]; then
            die "--profile wants NAME=HOME, got: $entry"
        fi
        [[ "$name" =~ ^[a-z0-9][a-z0-9._-]{0,63}$ ]] \
            || die "profile name $name: lowercase letters, digits, '.', '_' and '-'"
        case "$dir" in /*) ;; *) die "$name: the home must be an absolute path" ;; esac
        [ -d "$dir" ] || die "$name: $dir is not a directory"
        case "$dir" in *"'"*) die "$name: a home with a ' in it is not supported here" ;; esac
        printf "[profiles.%s]\nharness = \"claude\"\nenv = { CLAUDE_CONFIG_DIR = '%s' }\n\n" "$name" "$dir"
    done >"$data/profiles.toml"
)
for entry in "${profiles[@]}"; do names+=("${entry%%=*}"); done

# --- The node, on a free loopback port.
port=""
for _ in $(seq 1 50); do
    candidate=$((20000 + RANDOM % 20000))
    if ! (exec 3<>"/dev/tcp/127.0.0.1/$candidate") 2>/dev/null; then port=$candidate; break; fi
done
[ -n "$port" ] || die "no free port found"
"${clean[@]}" "$bin/vesseld" --data-dir "$data" --bind "127.0.0.1:$port" --name smoke-profiles \
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

vessel profiles | tee "$root/profiles.txt" | sed 's/^/    /'
for name in "${names[@]}"; do
    grep -q "^$name " "$root/profiles.txt" || die "the node does not list profile $name"
done

# --- A structured session under the first profile.
work=$root/work
mkdir -p "$work"
run=(run claude --structured --profile "${names[0]}" --cwd "$work" --detach)
[ -n "$model" ] && run+=(--model "$model")
id=$(vessel "${run[@]}")
say "session $id under ${names[0]}"
# Notes (status, profile changes) come on stderr, the conversation on stdout.
log() { vessel log "$id" 2>&1; }
# Reads all of the log: `grep -q` would stop early, and with pipefail the
# writer's broken pipe would read as "not there".
logged() { log | grep -F -- "$1" >/dev/null; }
turns() { log | grep -cE '^\[turn (ended|failed)' || true; }
# Ready, or stop at once with the reason when the session failed first
# (a model the agent does not offer fails the handshake within a second;
# waiting out the deadline would only hide that line). `until_true` sends
# the check's output to /dev/null, so the reason goes through a file and
# the check ends the wait; the script stops after it.
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
# Claude Code keeps a conversation as projects/<working directory, every
# character but a letter or digit made '-'>/<session>.jsonl in its home.
encoded=$(printf '%s' "$work" | sed 's/[^A-Za-z0-9]/-/g')
if [ "$fake" = 1 ]; then
    # The fake writes no transcript; plant one where Claude Code would.
    harness_session=$(log | sed -n 's/^\[ready: harness session \(.*\)\]$/\1/p' | head -n 1)
    first_home=${profiles[0]#*=}
    mkdir -p "$first_home/projects/$encoded"
    printf '{}\n' >"$first_home/projects/$encoded/$harness_session.jsonl"
fi

# `ask TEXT`: send a prompt and wait for its turn to end; prints the reply.
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
    # The reply: what follows the last line that is this prompt.
    printf '%s\n' "$after" | awk -v prompt="> $1" '$0 == prompt { reply = ""; next } { reply = reply $0 "\n" } END { printf "%s", reply }'
}

words=(marmalade lighthouse tangerine quokka harpsichord)
word=${words[$((RANDOM % ${#words[@]}))]}
recall="What word did I ask you to remember? Reply with that word only."
if [ "$fake" = 1 ]; then
    # The fake answers `plain` at once (anything else asks a permission).
    reply=$(ask plain)
else
    reply=$(ask "Remember the word $word for later. Reply with OK only.")
fi
say "first turn under ${names[0]}: $(printf '%s' "$reply" | tr '\n' ' ' | cut -c 1-80)"

# --- Moves: to each next profile, then back to the first.
order=("${names[@]:1}" "${names[0]}")
from=${names[0]}
for to in "${order[@]}"; do
    vessel session move-profile "$id" "$to" >"$root/move.txt" 2>&1 \
        || die "move $from -> $to failed: $(cat "$root/move.txt")"
    until_true "the move to $to in the log" logged "[profile $from -> $to]"
    if [ "$fake" = 1 ]; then
        # The fake cannot remember; it says which conversation it loaded.
        reply=$(ask loaded)
        case "$reply" in
            *"loaded none"*|"") die "after $from -> $to the agent loaded no conversation: $reply" ;;
            *loaded*) ;;
            *) die "after $from -> $to the agent did not answer: $reply" ;;
        esac
    else
        reply=$(ask "$recall")
        if ! printf '%s\n' "$reply" | grep -i -- "$word" >/dev/null; then
            die "after $from -> $to the reply does not hold the word $word: $reply"
        fi
    fi
    say "moved $from -> $to; the conversation carried over"
    from=$to
done

vessel kill "$id" >/dev/null 2>&1 || true
passed=1
say "PASS: one conversation through ${#names[@]} profiles and back"

if [ "$fake" = 0 ]; then
    say "the conversation's files were linked into each account's directory:"
    for entry in "${profiles[@]}"; do
        dir=${entry#*=}
        [ -d "$dir/projects/$encoded" ] && printf '    %s\n' "$dir/projects/$encoded"
    done
    say "remove them if you do not want to keep the smoke's conversation"
fi
