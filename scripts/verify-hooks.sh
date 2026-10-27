#!/bin/sh
# Check the wats nudge hooks (ADR 0056 §5) on a real install of one agent
# CLI, quickly and reversibly:
#
#   sh scripts/verify-hooks.sh <claude|codex|copilot|vibe>   set up
#   sh scripts/verify-hooks.sh check <dir>                   PASS / FAIL
#   sh scripts/verify-hooks.sh clean <dir>                   remove it all
#
# "Set up" makes a throwaway project directory and a throwaway wats node,
# writes the hooks into the project only (never your global CLI config),
# and prints the one prompt to paste into the CLI started there. The node
# runs hermetic (--infra none): nothing reaches the swarm.
#
# Needs: wats on PATH (or WATS=/path/to/wats), python3, and the CLI.
set -eu

WATS=${WATS:-wats}

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 1 ] || usage

case "$1" in
check)
    [ $# -eq 2 ] || usage
    dir=$2
    node="$dir/.wats-node"
    counters="$node/hooks/counters.json"
    host=$(cat "$dir/.wats-host" 2>/dev/null || echo "?")
    echo "Hooks of $host, in $dir:"
    get() {
        python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get(sys.argv[2], 0))
except Exception: print(0)' "$counters" "$1"
    }
    status=0
    check() { # name, counter, what
        n=$(get "$2")
        if [ "$n" -gt 0 ]; then
            echo "  PASS  $1 ($2 = $n)"
        else
            echo "  FAIL  $1 ($2 = 0): $3"
            status=1
        fi
    }
    check "the hook runs after tool calls" hook_post_tool \
        "the CLI did not run 'wats hook post-tool': are hooks enabled and trusted?"
    check "a failing command is nudged" nudge_error \
        "no failure seen: did the command fail, and does the payload carry its exit code or output?"
    check "the hook runs at the end of a turn" hook_stop \
        "the CLI did not run 'wats hook stop'"
    check "a fix after failures is nudged" nudge_fix \
        "no fix seen: two failures, an edit and a pass are needed in one turn"
    echo
    echo "And by eye, from the agent's last reply: did it quote the wats messages"
    echo "(\"unknown to the swarm so far\", \"you found a fix\")? If yes, they reached"
    echo "the model (additionalContext / additional_context / the Stop reason)."
    echo "The hook's log: $node/hooks/hook.log"
    exit $status
    ;;
clean)
    [ $# -eq 2 ] || usage
    dir=$2
    [ -f "$dir/.wats-host" ] || { echo "$dir: not a verify-hooks directory" >&2; exit 1; }
    "$WATS" --dir "$dir/.wats-node" stop >/dev/null 2>&1 || true
    rm -rf "$dir"
    echo "removed $dir"
    exit 0
    ;;
claude | codex | copilot | vibe) host=$1 ;;
*) usage ;;
esac

exe=$(command -v "$WATS") || { echo "wats not found (set WATS=/path/to/wats)" >&2; exit 1; }
dir=$(mktemp -d "${TMPDIR:-/tmp}/wats-verify-$host.XXXXXX")
node="$dir/.wats-node"
echo "$host" >"$dir/.wats-host"

# A throwaway node with a store, hermetic: nothing reaches the swarm.
WATS_HERMETIC=1 "$exe" --dir "$node" --infra none --detach >/dev/null 2>&1 || true
i=0
while [ ! -f "$node/events.sqlite" ] && [ $i -lt 50 ]; do sleep 0.2; i=$((i + 1)); done
WATS_HERMETIC=1 "$exe" --dir "$node" stop >/dev/null 2>&1 || true

# The project: a script that fails until it is fixed.
cat >"$dir/check.py" <<'PY'
import widgetlib_missing_module_for_wats_check

print("ok")
PY

post="$exe hook post-tool --host $host --dir $node"
stop="$exe hook stop --host $host --dir $node"
case "$host" in
claude)
    mkdir -p "$dir/.claude"
    cat >"$dir/.claude/settings.local.json" <<JSON
{
  "hooks": {
    "PostToolUse": [{"hooks": [{"type": "command", "command": "$post", "timeout": 10}]}],
    "PostToolUseFailure": [{"hooks": [{"type": "command", "command": "$post", "timeout": 10}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "$stop", "timeout": 10}]}]
  }
}
JSON
    run="cd $dir && claude"
    ;;
codex)
    mkdir -p "$dir/.codex"
    cat >"$dir/.codex/config.toml" <<TOML
[features]
hooks = true

[[hooks.PostToolUse]]
hooks = [{ type = "command", command = "$post", timeout = 10 }]

[[hooks.Stop]]
hooks = [{ type = "command", command = "$stop", timeout = 10 }]
TOML
    run="cd $dir && codex   (trust the project, then review the hooks once in /hooks)"
    ;;
copilot)
    mkdir -p "$dir/.github/hooks"
    cat >"$dir/.github/hooks/wats.json" <<JSON
{
  "version": 1,
  "hooks": {
    "PostToolUse": [{"type": "command", "bash": "$post", "timeoutSec": 10}],
    "PostToolUseFailure": [{"type": "command", "bash": "$post", "timeoutSec": 10}],
    "Stop": [{"type": "command", "bash": "$stop", "timeoutSec": 10}]
  }
}
JSON
    run="cd $dir && copilot"
    ;;
vibe)
    mkdir -p "$dir/.vibe"
    cat >"$dir/.vibe/hooks.toml" <<TOML
[[hooks]]
name = "wats-post-tool"
type = "post_tool"
match = "*"
command = "$post"

[[hooks]]
name = "wats-stop"
type = "post_agent"
command = "$stop"
TOML
    run="cd $dir && vibe --trust   (Vibe loads a project's .vibe/hooks.toml only in a trusted folder; --trust trusts it for this run)"
    ;;
esac

cat <<TXT
Set up in $dir (project-local hooks only; your global $host config is untouched).

1. Start the CLI there:
     $run
2. Paste this prompt:
     Run \`python3 check.py\`. It fails. Run it once more to see the error again,
     then fix check.py so that it prints ok (remove the import), run it again,
     and stop. In your final message, quote any message from "wats" you saw.
3. Exit the CLI, then:
     sh $0 check $dir
4. When done:
     sh $0 clean $dir
TXT
