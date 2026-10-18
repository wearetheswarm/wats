#!/bin/sh
# Tests of the Homebrew formula scripts against a local mirror signed with
# a throwaway key made here: a good release renders a formula pinning its
# checksums; a tampered checksum, a signature by another key or for another
# file, and a checksum naming another archive are refused, and write
# nothing. Needs minisign. Run from the repository root:
# sh scripts/test-homebrew.sh

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/wats-brew-test.XXXXXX")"
trap 'rm -rf "$T"' EXIT INT TERM
V="1.2.3"
fail=0

minisign -G -W -p "$T/test.pub" -s "$T/test.key" >/dev/null
minisign -G -W -p "$T/other.pub" -s "$T/other.key" >/dev/null
PUB="$(tail -n 1 "$T/test.pub")"

# mirror NAME: a fresh signed mirror of release $V in $T/NAME.
mirror() {
    dir="$T/$1/v$V"
    mkdir -p "$dir"
    n=1
    for target in aarch64-apple-darwin x86_64-unknown-linux-musl; do
        archive="wats-$V-$target.tar.gz"
        printf '%064d  %s\n' "$n" "$archive" > "$dir/$archive.sha256"
        minisign -S -s "$T/test.key" -m "$dir/$archive.sha256" >/dev/null < /dev/null
        n=$((n + 1))
    done
    printf '%s' "$dir"
}

# run NAME: the release script against mirror NAME, into $T/out-NAME.
run() {
    WATS_RELEASE_BASE_URL="file://$T/$1" WATS_INSTALL_PUBKEY="$PUB" \
        sh "$ROOT/scripts/homebrew-release.sh" "$V" "$T/out-$1" > "$T/log-$1" 2>&1
}

expect_ok() {
    if run "$1" && grep -q "sha256 \"$(printf '%064d' 1)\"" "$T/out-$1/Formula/wats.rb" \
        && grep -q "sha256 \"$(printf '%064d' 2)\"" "$T/out-$1/Formula/wats.rb" \
        && grep -q "releases/download/v$V/wats-$V-aarch64-apple-darwin.tar.gz\"" "$T/out-$1/Formula/wats.rb"; then
        echo "ok    $1"
    else
        echo "FAIL  $1: expected a formula"; cat "$T/log-$1"; fail=1
    fi
}

# expect_refused NAME TEXT: refused with TEXT in the log, nothing written.
expect_refused() {
    if run "$1"; then
        echo "FAIL  $1: accepted"; fail=1
    elif [ -e "$T/out-$1/Formula/wats.rb" ]; then
        echo "FAIL  $1: wrote a formula"; fail=1
    elif ! grep -q "$2" "$T/log-$1"; then
        echo "FAIL  $1: expected '$2'"; cat "$T/log-$1"; fail=1
    else
        echo "ok    $1 refused: $2"
    fi
}

mirror good >/dev/null
expect_ok good

dir="$(mirror tampered)"
sed 's/^0/f/' "$dir/wats-$V-x86_64-unknown-linux-musl.tar.gz.sha256" > "$T/x" && mv "$T/x" "$dir/wats-$V-x86_64-unknown-linux-musl.tar.gz.sha256"
expect_refused tampered "does not verify"

dir="$(mirror other-key)"
minisign -S -s "$T/other.key" -m "$dir/wats-$V-aarch64-apple-darwin.tar.gz.sha256" >/dev/null < /dev/null
expect_refused other-key "does not verify"

dir="$(mirror wrong-file)"
cp "$dir/wats-$V-aarch64-apple-darwin.tar.gz.sha256" "$T/renamed.sha256"
minisign -S -s "$T/test.key" -m "$T/renamed.sha256" >/dev/null < /dev/null
cp "$T/renamed.sha256.minisig" "$dir/wats-$V-aarch64-apple-darwin.tar.gz.sha256.minisig"
expect_refused wrong-file "made for another file"

dir="$(mirror other-archive)"
f="$dir/wats-$V-aarch64-apple-darwin.tar.gz.sha256"
printf '%064d  wats-0.0.1-aarch64-apple-darwin.tar.gz\n' 1 > "$f"
minisign -S -s "$T/test.key" -m "$f" >/dev/null < /dev/null
expect_refused other-archive "checksum of another file"

if sh "$ROOT/scripts/homebrew-formula.sh" "1.2" "$T" > /dev/null 2>&1; then
    echo "FAIL  bad-version: accepted"; fail=1
else
    echo "ok    bad-version refused"
fi

[ "$fail" -eq 0 ] && echo "all homebrew tests passed"
exit "$fail"
