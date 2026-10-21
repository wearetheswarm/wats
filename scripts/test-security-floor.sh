#!/bin/sh
# Tests of scripts/security-floor.sh: against the repository's CHANGELOG
# (0.4.2 has a Security section), and against a made-up one (none at all,
# a security section newer than the release, one under Unreleased, a
# "Security fixes" heading, a two-digit version). Run from the repository
# root: sh scripts/test-security-floor.sh

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/wats-floor-test.XXXXXX")"
trap 'rm -rf "$T"' EXIT INT TERM
fail=0

expect() {
  got="$(sh "$ROOT/scripts/security-floor.sh" "$1" "$2")"
  if [ "$got" = "$3" ]; then
    printf 'ok   %s %s -> %s\n' "$1" "$(basename "$2")" "$got"
  else
    printf 'FAIL %s %s -> %s, expected %s\n' "$1" "$(basename "$2")" "$got" "$3"
    fail=1
  fi
}

expect 0.4.2 "$ROOT/CHANGELOG.md" 0.4.2
expect 0.4.3 "$ROOT/CHANGELOG.md" 0.4.2
expect v0.4.3 "$ROOT/CHANGELOG.md" 0.4.2
expect 0.4.1 "$ROOT/CHANGELOG.md" 0.2.0

printf '# Changelog\n\n## 1.0.0 (2026-01-01)\n\n### Changes\n\n- a\n' > "$T/none.md"
expect 1.0.0 "$T/none.md" 0.0.0

cat > "$T/made-up.md" <<'EOF'
# Changelog

## Unreleased

### Security

- not released yet

## 0.10.0 (2026-03-01)

### Security

- after 0.9.x

## 0.9.1 (2026-02-02)

### Changes

- no security section

## 0.9.0 (2026-02-01)

### Security fixes

- counts too

## 0.2.0 (2026-01-01)

### Security

- old
EOF
expect 0.9.1 "$T/made-up.md" 0.9.0
expect 0.9.0 "$T/made-up.md" 0.9.0
expect 0.8.0 "$T/made-up.md" 0.2.0
expect 0.10.0 "$T/made-up.md" 0.10.0
expect 0.11.0 "$T/made-up.md" 0.10.0
expect 0.1.0 "$T/made-up.md" 0.0.0

if sh "$ROOT/scripts/security-floor.sh" 1.2 "$T/none.md" >/dev/null 2>&1; then
  echo "FAIL a version that is not X.Y.Z is refused"
  fail=1
fi

[ "$fail" -eq 0 ] && echo "all security floor tests passed"
exit "$fail"
