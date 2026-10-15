#!/usr/bin/env bash
# One-command local check of the bootstrap path with docker compose
# (deploy/compose/compose.yaml): build the image, make a throwaway router
# key outside the repository, start router + peer, post from a third node
# with a blob, and check that:
#   1. the router and the peer receive the event (gossip),
#   2. the blob is replicated to both (replication k),
#   3. the router listens on UDP 7842 on IPv4 and IPv6,
#   4. a node that joins later, bootstrapping through the router's IPv6
#      address, gets the event (catch-up sync).
# Everything is torn down at the end, volumes included. Exit 0 on success.
#
# Not run in CI (Actions minutes budget). Needs Docker (OrbStack on macOS).
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
compose_file="$repo/deploy/compose/compose.yaml"
project="wats-compose-test-$$"
work="$(mktemp -d "${TMPDIR:-/tmp}/wats-compose-test.XXXXXX")"
timeout="${WATS_TEST_TIMEOUT:-90}"

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
failure=""
fail() { failure="$*"; printf '\033[31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

dc() { docker compose -p "$project" -f "$compose_file" --profile post --profile late "$@"; }

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- logs (last 40 lines per service) ---" >&2
    dc logs --no-color --tail 40 >&2 2>/dev/null || true
    printf '\033[31mFAIL: %s\033[0m\n' "${failure:-unexpected error (exit $status)}" >&2
  fi
  dc down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

# wait_for <service> <grep -E pattern> <description>: poll the service's
# stdout (JSON lines) until a line matches.
wait_for() {
  local service="$1" pattern="$2" what="$3" deadline=$((SECONDS + timeout))
  until dc logs --no-color --no-log-prefix "$service" 2>/dev/null | grep -qE "$pattern"; do
    [ "$SECONDS" -lt "$deadline" ] || fail "$what (no match after ${timeout}s)"
    sleep 1
  done
  echo "    ok: $what"
}

say "Building the image (wats:dev)"
docker build -q -t wats:dev "$repo" >/dev/null

say "Generating a throwaway router key in $work"
mkdir -p "$work/key" "$work/files"
chmod 777 "$work/key"
id_output="$(docker run --rm -v "$work/key:/key" wats:dev --dir /key id 2>/dev/null)"
router_id="$(printf '%s\n' "$id_output" | sed -n 's/^endpoint_id: //p')"
[ -n "$router_id" ] || fail "could not read the router id"
echo "    router id: $router_id"
head -c 65536 /dev/urandom > "$work/files/test.bin"

export WATS_ROUTER_ID="$router_id"
export WATS_ROUTER_KEY="$work/key/secret.key"
export WATS_TEST_FILES="$work/files"
export WATS_TEST_TEXT="compose test $$ $(date +%s)"
# Avoid clashing with a router already running on the host.
export WATS_ROUTER_HOST_PORT="${WATS_ROUTER_HOST_PORT:-17842}"

say "Starting router and peer (peer bootstraps via $router_id@router:7842)"
dc up -d router peer >/dev/null 2>&1
wait_for router '"type":"ready"' "router is up"
wait_for router "\"endpoint\":\"$router_id\"" "router runs with the pinned key"
wait_for peer '"type":"ready"' "peer is up"

say "Checking that the router listens on 0.0.0.0:7842 and [::]:7842"
router_container="$(dc ps -q router)"
sockets="$(docker run --rm --network "container:$router_container" busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e netstat -uln)"
grep -qE '0\.0\.0\.0:7842' <<<"$sockets" || fail "router does not listen on IPv4 UDP 7842"
echo "    ok: IPv4 UDP 7842"
grep -qE ':::7842' <<<"$sockets" || fail "router does not listen on IPv6 UDP 7842"
echo "    ok: IPv6 UDP 7842"

say "Posting from a third node with a 64 KiB blob"
dc up -d poster >/dev/null 2>&1
wait_for poster '"type":"published"' "poster published"
event_id="$(dc logs --no-color --no-log-prefix poster | grep -m1 '"type":"published"' | sed -E 's/.*"id":"([0-9a-f]{64})".*/\1/')"
[ -n "$event_id" ] || fail "could not read the event id"
echo "    event id: $event_id"

say "Checking gossip delivery"
wait_for router "\"id\":\"$event_id\"" "router received the event"
wait_for peer "\"id\":\"$event_id\"" "peer received the event"
blob_hash="$(dc logs --no-color --no-log-prefix router | grep -m1 "\"id\":\"$event_id\"" | sed -E 's/.*"blobs":\[\{"hash":"([0-9a-f]{64})".*/\1/')"
[ -n "$blob_hash" ] || fail "could not read the blob hash"

say "Checking replication of blob $blob_hash"
wait_for router "\"type\":\"blob\".*\"hash\":\"$blob_hash\"|\"hash\":\"$blob_hash\".*\"type\":\"blob\"" "router holds the blob"
wait_for peer "\"type\":\"blob\".*\"hash\":\"$blob_hash\"|\"hash\":\"$blob_hash\".*\"type\":\"blob\"" "peer holds the blob"

say "Checking catch-up sync with a late joiner"
dc up -d late >/dev/null 2>&1
wait_for late "\"id\":\"$event_id\"" "late joiner (bootstrapped via IPv6) synced the event"

say "PASS: bootstrap by DNS name with pinned id, dual-stack listener, gossip, replication, sync"
