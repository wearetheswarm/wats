#!/bin/sh
# Run the independent Python verifier against the published test vectors.
# Exits non-zero if it disagrees with any case.
set -eu
root="$(cd "$(dirname "$0")/../.." && pwd)"
exec python3 "$root/tools/verify-py/verify.py" \
    "$root/docs/test-vectors/events.json" \
    "$root/docs/test-vectors/events-invalid.json"
