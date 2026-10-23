#!/usr/bin/env bash
# `wats setup openclaw --mcp --agent --apply` against the real OpenClaw CLI
# on this machine, in a throwaway OpenClaw config and a throwaway wats data
# directory: your ~/.openclaw and your wats node are never touched, and no
# gateway is started. Run it after an OpenClaw upgrade: the setup's tests
# use a fake `openclaw`, and only this catches a change in its CLI or its
# config schema.
#
#   scripts/openclaw-setup-check.sh [path/to/wats]   (default target/debug/wats)
#
# It seeds an agent of yours in agents.list, then runs the automatic setup
# twice and checks that every `openclaw config` step passed, that hooks.token
# is the ${WATS_OPENCLAW_HOOKS_TOKEN} template, that agents.list holds your
# agent and one `wats` agent, and that `openclaw config validate` passes.
set -euo pipefail

wats=${1:-target/debug/wats}
command -v openclaw >/dev/null || { echo "openclaw is not on PATH" >&2; exit 1; }
[ -x "$wats" ] || { echo "no wats at $wats (cargo build -p wats)" >&2; exit 1; }
wats=$(cd "$(dirname "$wats")" && pwd)/$(basename "$wats")

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export OPENCLAW_STATE_DIR="$tmp/openclaw"
export OPENCLAW_CONFIG_PATH="$OPENCLAW_STATE_DIR/openclaw.json"
mkdir -p "$OPENCLAW_STATE_DIR"
chmod 700 "$OPENCLAW_STATE_DIR"
cat > "$OPENCLAW_CONFIG_PATH" <<'EOF'
{
  "agents": { "list": [ { "id": "main", "default": true } ] },
  "hooks": { "allowedAgentIds": ["main"] }
}
EOF
echo "$(openclaw --version | head -1), config $OPENCLAW_CONFIG_PATH"

fail() { echo "FAIL: $*" >&2; exit 1; }
for pass in 1 2; do
  if ! WATS_HERMETIC=1 "$wats" --dir "$tmp/wats" setup openclaw --mcp --agent --apply --yes \
      > "$tmp/out-$pass" 2>&1; then
    grep -v 'WATS_OPENCLAW_HOOKS_TOKEN=' "$tmp/out-$pass" | tail -20 >&2
    fail "setup, pass $pass"
  fi
  grep -q 'applied, and `openclaw config validate` passes' "$tmp/out-$pass" \
    || fail "setup, pass $pass: not applied"
done

token=$(openclaw config get hooks.token)
[ "$token" = '${WATS_OPENCLAW_HOOKS_TOKEN}' ] || fail "hooks.token is $token"
ids=$(openclaw config get agents.list | python3 -c \
  'import json,sys; print(" ".join(a["id"] for a in json.load(sys.stdin)))')
[ "$ids" = "main wats" ] || fail "agents.list ids: $ids"
WATS_OPENCLAW_HOOKS_TOKEN=check openclaw config validate >/dev/null || fail "config validate"
echo "OK: the automatic setup applies and validates, twice"
