#!/bin/sh
# Write the Homebrew formula of a published wats release, from its signed
# checksums.
#
#   sh scripts/homebrew-release.sh VERSION OUT
#
# Downloads each prebuilt archive's .sha256 and .sha256.minisig from the
# release, verifies the signature with the release key pinned in
# install.sh (and that its trusted comment names the file), then renders
# OUT/Formula/wats.rb with scripts/homebrew-formula.sh. Any failure stops
# before anything is written. Needs curl and minisign.
#
# WATS_RELEASE_BASE_URL overrides the download base (tests): files are read
# from <base>/v<version>/<file>.

set -eu

die() { printf 'homebrew-release.sh: %s\n' "$*" >&2; exit 1; }

[ $# -eq 2 ] || die "usage: sh scripts/homebrew-release.sh VERSION OUT"
VERSION="$1"
OUT="$2"
HERE="$(cd "$(dirname "$0")" && pwd)"
BASE="${WATS_RELEASE_BASE_URL:-https://github.com/wearetheswarm/wats/releases/download}"
PUBKEY="${WATS_INSTALL_PUBKEY:-$(sed -n 's/^PUBKEY="\(RW[A-Za-z0-9+/=]*\)"$/\1/p' "$HERE/../install.sh" | head -n 1)}"
[ -n "$PUBKEY" ] || die "cannot read the release key from install.sh"
command -v minisign >/dev/null 2>&1 || die "minisign is needed"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/wats-brew.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT INT TERM

for target in aarch64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    file="wats-$VERSION-$target.tar.gz.sha256"
    for f in "$file" "$file.minisig"; do
        curl --proto '=https,file' --tlsv1.2 -fsSL -o "$WORK/$f" "$BASE/v$VERSION/$f" \
            || die "cannot download $f"
    done
    out="$(minisign -Vm "$WORK/$file" -x "$WORK/$file.minisig" -P "$PUBKEY" 2>&1)" \
        || die "the signature of $file does not verify with the release key: $out"
    printf '%s\n' "$out" | grep -Eq "^Trusted comment: (.*[[:space:]])?file:$file([[:space:]]|\$)" \
        || die "the signature of $file was made for another file: $out"
done

formula="$(sh "$HERE/homebrew-formula.sh" "$VERSION" "$WORK")"
mkdir -p "$OUT/Formula"
printf '%s\n' "$formula" > "$OUT/Formula/wats.rb"
printf 'wrote %s/Formula/wats.rb for wats %s\n' "$OUT" "$VERSION"
