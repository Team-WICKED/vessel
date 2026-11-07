#!/usr/bin/env bash
# Milestone 4 smoke (b): a fresh machine joins a fleet with `vessel init
# --join`, and within minutes has the fleet's instructions, skills and MCP
# servers in each of its profiles; a local edit is kept and listed as a
# conflict. See docs/smoke/README.md.
#
# Both nodes are throwaway: a home with sample sources and a "fresh
# machine" whose home directory holds four empty account directories. The
# harnesses are the testkit fakes installed under the real names, so no
# account, login or real harness is involved, and the MCP servers each
# session gets can be read back from the fake.
#
#   docs/smoke/join.sh --docker [--image IMAGE]   # in a container (default)
#   docs/smoke/join.sh --local                    # under a temporary HOME here
#
# --docker runs everything in one container with no network (both nodes on
# its loopback), the binaries mounted read-only. --local runs the same steps
# on this machine with a scrubbed environment: a temporary HOME and data
# directories, loopback ports, and no `--service`, so nothing reaches this
# machine's services or your own node, book or accounts.
#
# Options:
#   --bin DIR    where vessel, vesseld, fake-tui and fake-acp are
#                (default: the workspace's target/release)
#   --keep       keep the temporary directory (--local)
#   --deadline S seconds to wait for each step (default 120)
set -euo pipefail

mode=docker
image=ubuntu:24.04
bin=""
keep=0
deadline=120
inside=0

die() { printf 'join smoke: %s\n' "$*" >&2; exit 1; }
say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --docker) mode=docker ;;
        --local) mode=local ;;
        --image) image=${2:?--image needs a value}; shift ;;
        --bin) bin=${2:?--bin needs a value}; shift ;;
        --keep) keep=1 ;;
        --deadline) deadline=${2:?--deadline needs a value}; shift ;;
        --inside) inside=1; mode=local ;;
        -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
for program in vessel vesseld fake-tui fake-acp; do
    [ -x "$bin/$program" ] || die "$bin/$program is missing: build with \`cargo build --release --workspace --bins\`, or pass --bin"
done

if [ "$mode" = docker ] && [ "$inside" = 0 ]; then
    if ! command -v docker >/dev/null || ! docker info >/dev/null 2>&1; then
        die "docker is not usable here; run with --local"
    fi
    script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
    say "running in a throwaway container ($image, no network)"
    # The binaries need the build machine's glibc or newer: pick --image to
    # match (ubuntu:24.04 for a build on Ubuntu 24.04).
    exec docker run --rm --network none --user "$(id -u):$(id -g)" \
        -v "$bin:/vessel/bin:ro" -v "$script:/vessel/join.sh:ro" \
        -e TERM=dumb "$image" \
        bash /vessel/join.sh --inside --bin /vessel/bin --deadline "$deadline"
fi

# ---------------------------------------------------------------------------
# The smoke itself, here (--local) or inside the container (--inside).

root=$(mktemp -d "${TMPDIR:-/tmp}/vessel-join-smoke.XXXXXX")
chmod 700 "$root"
pids=()
passed=0

cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
    if [ "$passed" = 0 ]; then
        for log in "$root"/*.log; do
            [ -f "$log" ] || continue
            printf '\n--- last lines of %s\n' "$(basename "$log")" >&2
            tail -n 40 "$log" >&2
        done
    fi
    if [ "$keep" = 1 ] && [ "$inside" = 0 ]; then
        say "kept $root"
    else
        rm -rf "$root"
    fi
}
trap cleanup EXIT

# The fakes under the harnesses' names, and the binaries, first on PATH.
fakes=$root/bin
mkdir -p "$fakes"
for name in claude codex; do ln -s "$bin/fake-tui" "$fakes/$name"; done
for name in claude-agent-acp codex-acp; do ln -s "$bin/fake-acp" "$fakes/$name"; done
for name in vessel vesseld; do ln -s "$bin/$name" "$fakes/$name"; done
path=$fakes:/usr/bin:/bin

# A real harness on PATH would get login checks under the temporary homes:
# refuse rather than run it.
for name in claude codex claude-agent-acp codex-acp gemini opencode; do
    found=$(env -i PATH="$path" bash -c "command -v $name" || true)
    case "$found" in
        ""|"$fakes/$name") ;;
        *) die "$found is on the smoke's PATH ($path); it would be run" ;;
    esac
done

# Ports: fixed inside the container (its own loopback), free ones here.
free_port() {
    local port
    for _ in $(seq 1 50); do
        port=$((20000 + RANDOM % 20000))
        if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            printf '%s' "$port"
            return
        fi
    done
    die "no free port found"
}
if [ "$inside" = 1 ]; then
    home_port=7177; member_port=7277; relay_port=7377
else
    home_port=$(free_port); member_port=$(free_port); relay_port=$(free_port)
fi

home_dir=$root/home
member_dir=$root/member
sources=$root/sources
work=$root/work
reports=$root/reports
mkdir -p "$home_dir" "$member_dir" "$sources/skills/smoke-skill" "$work" "$reports"

# Run as the home's or the member's user: nothing but these variables.
home_env=(env -i HOME="$home_dir" PATH="$path" LANG=C.UTF-8 TMPDIR="$root"
    VESSEL_NAME=smoke-home)
member_env=(env -i HOME="$member_dir" PATH="$path" LANG=C.UTF-8 TMPDIR="$root"
    VESSEL_NAME=fresh-machine)
as_home() { "${home_env[@]}" "$@"; }
as_member() { "${member_env[@]}" "$@"; }
# Long-running programs start as `( exec "${home_env[@]}" PROGRAM ... ) &`,
# never `as_home PROGRAM &`: a backgrounded function runs in a subshell
# that does not exec its last command, so `$!` would be that subshell and
# cleanup's kill would leave the node running after its directory is gone.
# With `exec`, the subshell becomes `env`, which execs the program, so `$!`
# is the program itself.

# `until_true WHAT COMMAND...`: run COMMAND every second until it succeeds,
# failing after the deadline.
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

started=$(date +%s)

# --- A relay on loopback, as the home's owner would run one: the fresh
# machine's node, started plainly, registers there too (`fleet.callback`),
# so the home can call it back. A throwaway token, never printed.
say "relay on 127.0.0.1:$relay_port"
printf 'smoke-relay-%s%s\n' "$RANDOM" "$RANDOM" >"$root/relay.token"
chmod 600 "$root/relay.token"
( exec "${home_env[@]}" vesseld relay --bind "127.0.0.1:$relay_port" \
    --token-file "$root/relay.token" ) >"$root/relay.log" 2>&1 &
pids+=($!)
relay_up() { (exec 3<>"/dev/tcp/127.0.0.1/$relay_port") 2>/dev/null; }
until_true "the relay listening" relay_up

# --- The home: sample sources, two MCP servers, a node with a direct route
# and the relay.
say "home: sources, fleet.toml, node on 127.0.0.1:$home_port"
printf '# Fleet instructions (smoke)\nfirst version\n' >"$sources/AGENTS.md"
printf -- '---\nname: smoke-skill\ndescription: shared by the smoke test\n---\nUse it.\n' \
    >"$sources/skills/smoke-skill/SKILL.md"
as_home vessel fleet init --instructions "$sources/AGENTS.md" --skills "$sources/skills" \
    >"$root/home-init.log" 2>&1
cat >>"$home_dir/.vessel/fleet.toml" <<'TOML'

[mcp.servers.docs]
type = "stdio"
command = "docs-mcp"
env = { DOCS_TOKEN = { secret = "DOCS_TOKEN" } }

[mcp.servers.plain]
type = "stdio"
command = "plain-mcp"
TOML
( exec "${home_env[@]}" vesseld --bind "127.0.0.1:$home_port" \
    --public "ws://127.0.0.1:$home_port" \
    --relay "ws://127.0.0.1:$relay_port" --relay-token-file "$root/relay.token" ) \
    >"$root/home.log" 2>&1 &
pids+=($!)
until_true "the home node answering" as_home vessel --node "127.0.0.1:$home_port" status

link=$(as_home vessel --node "127.0.0.1:$home_port" fleet invite --label fresh-machine 2>>"$root/home-init.log")
[ -n "$link" ] || die "the home printed no link"

# --- The fresh machine: four account homes with an empty login file each
# (only the names are looked at), no data directory, no node.
for dir in .claude .claude-second; do mkdir -p "$member_dir/$dir"; : >"$member_dir/$dir/.credentials.json"; done
for dir in .codex .codex-second; do mkdir -p "$member_dir/$dir"; : >"$member_dir/$dir/auth.json"; done

say "fresh machine: vessel init --join"
joined_at=$(date +%s)
printf '%s\n' "$link" | as_member vessel init --join --yes >"$root/member-init.log" 2>&1 \
    || die "vessel init --join failed (member-init.log above)"
sed 's/^/    /' "$root/member-init.log"
grep -q "joined the fleet" "$root/member-init.log" || die "init did not join the fleet"

# Without --service the node is started by hand, as init's "left to do" says.
( exec "${member_env[@]}" vessel node start --bind "127.0.0.1:$member_port" ) \
    >"$root/member.log" 2>&1 &
pids+=($!)
member() { as_member vessel --node "127.0.0.1:$member_port" "$@"; }
until_true "the fresh machine's node answering" member status

generation() {
    as_member vessel fleet status | sed -n 's/^generation \([0-9]*\), applied.*/\1/p'
}
applied() { [ -n "$(generation)" ]; }
until_true "the fresh machine applying the fleet" applied
say "applied generation $(generation), $(( $(date +%s) - joined_at ))s after the join"

# --- The home can call the fresh machine back: it redeemed the grant
# through the relay, so the fresh machine holds its device, and nothing
# says the grant was not taken.
home_device() { member node devices | grep -q "fleet home "; }
until_true "the home taking the fresh machine's grant through the relay" home_device
as_member vessel fleet status >"$root/status.txt"
if grep -q "not taken by the home" "$root/status.txt"; then
    die "the home did not take the grant: $(cat "$root/status.txt")"
fi
say "the home took the fresh machine's grant through the relay, $(( $(date +%s) - joined_at ))s after the join"

# --- Profiles: the defaults and one per other account.
member profiles >"$root/profiles.txt"
for profile in claude claude-second codex codex-second; do
    grep -q "^$profile " "$root/profiles.txt" || die "no profile $profile: $(cat "$root/profiles.txt")"
done
say "profiles: claude, claude-second, codex, codex-second"

# --- Instructions and skills in every profile's home.
check_files() {
    local dir=$1 instructions=$2
    cmp -s "$sources/AGENTS.md" "$member_dir/$dir/$instructions" \
        || die "$dir/$instructions is not the fleet's"
    cmp -s "$sources/skills/smoke-skill/SKILL.md" "$member_dir/$dir/skills/smoke-skill/SKILL.md" \
        || die "$dir/skills/smoke-skill is not the fleet's"
}
for dir in .claude .claude-second; do check_files "$dir" CLAUDE.md; done
for dir in .codex .codex-second; do check_files "$dir" AGENTS.md; done
say "instructions and skills are in all four homes"

# --- MCP servers: every session gets both, with this node's secret. The
# value is a throwaway and is never printed.
printf 'smoke-throwaway-value\n' | member fleet secret set DOCS_TOKEN >/dev/null 2>&1 \
    || die "vessel fleet secret set failed"
mcp_in() {
    local harness=$1 profile=$2 report=$reports/$2.json id
    id=$(member run "$harness" --structured --profile "$profile" --cwd "$work" --detach)
    until_true "a $profile session taking a prompt" member send "$id" "report $report"
    until_true "the $profile session's report" test -s "$report"
    member kill "$id" >/dev/null 2>&1 || true
    grep -q '"name":"docs"' "$report" && grep -q '"name":"plain"' "$report"
}
# The node reads secrets on its next look: the first session may predate it.
until_true "the docs server reaching a session" mcp_in claude claude
mcp_in claude claude-second || die "MCP servers missing in claude-second"
for profile in codex codex-second; do mcp_in codex "$profile" || die "MCP servers missing in $profile"; done
say "MCP servers docs and plain reach a session in every profile"

# --- A local edit is kept and listed as a conflict.
before=$(generation)
printf 'my local edit\n' >>"$member_dir/.claude-second/CLAUDE.md"
printf 'second version\n' >>"$sources/AGENTS.md"
newer() { [ "$(generation)" -gt "$before" ]; }
until_true "the fresh machine applying the home's edit" newer
edited=$member_dir/.claude-second/CLAUDE.md
grep -q 'my local edit' "$edited" || die "the local edit was lost"
if grep -q 'second version' "$edited"; then die "the local edit was overwritten"; fi
grep -q 'second version' "$edited.vessel-incoming" \
    || die "no incoming version beside the edited file"
grep -q 'second version' "$member_dir/.claude/CLAUDE.md" || die "an unedited home did not follow"
as_member vessel fleet status >"$root/status.txt"
grep -q "edited .*\.claude-second/CLAUDE\.md" "$root/status.txt" \
    || die "fleet status does not list the edit: $(cat "$root/status.txt")"
say "the local edit is kept, the home's version is beside it, and fleet status lists it:"
sed 's/^/    /' "$root/status.txt"

passed=1
say "PASS in $(( $(date +%s) - started ))s"
