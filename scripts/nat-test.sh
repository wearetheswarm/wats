#!/usr/bin/env bash
# Cross-NAT check for the Phase 0 spike (SPEC §5): two machines on
# different networks (e.g. home Wi-Fi and a phone hotspot) exchange a signed
# event and a blob through Iroh (hole punching, relay fallback).
#
# Machine A:  scripts/nat-test.sh a
#             -> prints a ticket; send it to machine B (chat, email...).
# Machine B:  scripts/nat-test.sh b <ticket>
#             -> publishes a post with a 1 MiB random blob.
# Success: machine A prints an "event" line, then a "blob" line whose hash
# matches the one machine B printed. Stop both with Ctrl-C.
#
# Set WATS=/path/to/wats to use a prebuilt binary instead of cargo.
set -euo pipefail

role="${1:-}"
dir="${WATS_NAT_DIR:-${TMPDIR:-/tmp}/wats-nat-$role}"
if [[ -n "${WATS:-}" ]]; then
  wats=("$WATS")
else
  cargo build --release --quiet
  wats=("$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/release/wats")
fi
export RUST_LOG="${RUST_LOG:-wats=info,iroh::socket=info}"

case "$role" in
  a)
    echo "Data dir: $dir. Waiting for peers; the ticket is in the \"ready\" line below." >&2
    exec "${wats[@]}" --dir "$dir" node
    ;;
  b)
    ticket="${2:?usage: $0 b <ticket from machine A>}"
    mkdir -p "$dir"
    blob="$dir/nat-test.bin"
    head -c 1048576 /dev/urandom > "$blob"
    echo "Blob BLAKE3 (look for it on machine A): $(b3sum --no-names "$blob" 2>/dev/null || echo 'install b3sum to print it')" >&2
    exec "${wats[@]}" --dir "$dir" --peer "$ticket" node \
      --post "Cross-NAT test from $(hostname)" --kind note --tag test:nat --blob "$blob"
    ;;
  *)
    echo "usage: $0 a | $0 b <ticket>" >&2
    exit 2
    ;;
esac
