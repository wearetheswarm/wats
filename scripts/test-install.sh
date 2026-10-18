#!/bin/sh
# Tests of install.sh against a local mirror of signed fixtures
# (crates/wats/tests/fixtures/upgrade, signed with a throwaway key): a good
# release installs; a tampered archive, a signature by another key or for
# another file, and a malformed version are refused, and install nothing.
# Needs minisign. Run from the repository root: sh scripts/test-install.sh

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIX="$ROOT/crates/wats/tests/fixtures/upgrade"
NAME="wats-9.9.9-x86_64-unknown-linux-musl.tar.gz"
T="$(mktemp -d "${TMPDIR:-/tmp}/wats-install-test.XXXXXX")"
trap 'rm -rf "$T"' EXIT

failures=0
pass() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; failures=$((failures + 1)); }

# mirror DIR: a fresh mirror holding the good release files.
mirror() {
    rm -rf "$1"
    mkdir -p "$1/v9.9.9"
    for f in "$NAME" "$NAME.minisig" "$NAME.sha256" "$NAME.sha256.minisig"; do
        cp "$FIX/$f" "$1/v9.9.9/$f"
    done
}

# run CASE MIRROR [ARGS...]: install.sh against MIRROR into a fresh prefix.
run() {
    case_dir="$T/case-$1"; mirror_dir="$2"; shift 2
    rm -rf "$case_dir"
    WATS_INSTALL_BASE_URL="file://$mirror_dir" \
    WATS_INSTALL_PUBKEY="$(cat "$FIX/test.pub")" \
    WATS_INSTALL_TARGET="x86_64-unknown-linux-musl" \
        sh "$ROOT/install.sh" --prefix "$case_dir/bin" "$@" >"$case_dir.log" 2>&1
}

# expect_refused CASE MIRROR WHY [ARGS...]
expect_refused() {
    c="$1"; m="$2"; why="$3"; shift 3
    if run "$c" "$m" "$@"; then
        fail "$c: installed, but must be refused ($why)"
    elif [ -e "$T/case-$c/bin/wats" ]; then
        fail "$c: refused, but left a binary behind"
    elif ! grep -q "$why" "$T/case-$c.log"; then
        fail "$c: refused for another reason: $(tail -n 1 "$T/case-$c.log")"
    else
        pass "$c refused: $why"
    fi
}

mirror "$T/m-good"
if run good "$T/m-good" --version 9.9.9 && [ "$("$T/case-good/bin/wats" --version)" = "wats 9.9.9" ]; then
    pass "good release installs"
else
    fail "good release: $(cat "$T/case-good.log")"
fi

mirror "$T/m-tampered"
printf 'x' >> "$T/m-tampered/v9.9.9/$NAME"
expect_refused tampered-archive "$T/m-tampered" "does not verify" --version 9.9.9

mirror "$T/m-otherkey"
cp "$FIX/$NAME.other-key.minisig" "$T/m-otherkey/v9.9.9/$NAME.minisig"
expect_refused other-key "$T/m-otherkey" "does not verify" --version 9.9.9

mirror "$T/m-wrongfile"
cp "$FIX/$NAME.wrong-file.minisig" "$T/m-wrongfile/v9.9.9/$NAME.minisig"
expect_refused wrong-file "$T/m-wrongfile" "another file" --version 9.9.9

mirror "$T/m-nosig"
rm "$T/m-nosig/v9.9.9/$NAME.sha256.minisig"
expect_refused missing-signature "$T/m-nosig" "cannot download" --version 9.9.9

expect_refused bad-version "$T/m-good" "must look like" --version latest
expect_refused mirror-needs-version "$T/m-good" "needs --version"

[ "$failures" -eq 0 ] || { printf '%s failure(s)\n' "$failures"; exit 1; }
printf 'all install.sh tests passed\n'
