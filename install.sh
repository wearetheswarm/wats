#!/bin/sh
# Install wats from a signed GitHub release.
#
#   sh install.sh [--version X.Y.Z] [--prefix DIR]
#
# Downloads the release archive for this machine (macOS arm64, Linux
# x86_64 or arm64), verifies its minisign signature and its checksum's signature
# against the release key pinned below, checks the file names in the signed
# comments and the SHA-256, then installs `wats` into ~/.local/bin (or
# --prefix). Any failure stops before anything is installed. Never uses
# sudo. Afterwards, `wats upgrade` keeps it up to date.
#
# Read this script before running it:
#
#   curl -fsSLO https://github.com/wearetheswarm/wats/releases/latest/download/install.sh
#   less install.sh && sh install.sh
#
# GH_TOKEN (or GITHUB_TOKEN) is optional: when set, GitHub's API is used
# with it, which only helps with GitHub's rate limits.
#
# Needs: curl, tar, minisign, and shasum or sha256sum.
# Docs: docs/release-signing.md.

set -eu

REPO="wearetheswarm/wats"
# The release signing key (docs/release-signing.md, README.md).
PUBKEY="RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci"

# Overrides for tests and mirrors. A mirror serves the release files as
# <base>/v<version>/<file>, and needs --version.
PUBKEY="${WATS_INSTALL_PUBKEY:-$PUBKEY}"
BASE_URL="${WATS_INSTALL_BASE_URL:-}"
TARGET="${WATS_INSTALL_TARGET:-}"

VERSION=""
PREFIX="${HOME}/.local/bin"
TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

usage() {
    say "usage: sh install.sh [--version X.Y.Z] [--prefix DIR]"
    say "  --version  a release, without the v (default: the latest)"
    say "  --prefix   where to put wats (default: ~/.local/bin)"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="${2#v}"; shift 2 ;;
        --version=*) VERSION="${1#--version=}"; VERSION="${VERSION#v}"; shift ;;
        --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; PREFIX="$2"; shift 2 ;;
        --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
done

case "$VERSION" in
    ""|[0-9]*.[0-9]*.[0-9]*) ;;
    *) die "--version must look like X.Y.Z, got $VERSION" ;;
esac

# --- The machine ---------------------------------------------------------

if [ -z "$TARGET" ]; then
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Darwin/arm64) TARGET="aarch64-apple-darwin" ;;
        Linux/x86_64|Linux/amd64) TARGET="x86_64-unknown-linux-musl" ;;
        Linux/aarch64|Linux/arm64) TARGET="aarch64-unknown-linux-musl" ;;
        *) die "no prebuilt wats for $os $arch yet (macOS arm64, Linux x86_64 and arm64 only). Build it from source (Rust and a C compiler): cargo install --locked --git https://github.com/wearetheswarm/wats wats" ;;
    esac
fi

# --- Tools ---------------------------------------------------------------

have() { command -v "$1" >/dev/null 2>&1; }

if ! have minisign; then
    die "minisign is needed to verify the release signature. Install it (macOS: brew install minisign; Debian/Ubuntu: sudo apt install minisign; others: https://jedisct1.github.io/minisign/), then run this script again."
fi
if have shasum; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
elif have sha256sum; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
else
    die "shasum or sha256sum is needed to check the archive"
fi
have tar || die "tar is needed"
have curl || die "curl is needed"
# fetch URL OUT [CURL ARGS...]
fetch() {
    url="$1"; out="$2"; shift 2
    curl --proto '=https,file' --tlsv1.2 -fsSL -o "$out" "$@" "$url"
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/wats-install.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT INT TERM

# --- Which release -------------------------------------------------------

API="https://api.github.com/repos/$REPO"
auth_api() {
    # auth_api PATH OUT: a GitHub API call with the token
    fetch "$API/$1" "$2" -H "Authorization: Bearer $TOKEN" -H "Accept: application/vnd.github+json"
}

if [ -z "$VERSION" ]; then
    [ -z "$BASE_URL" ] || die "a mirror (WATS_INSTALL_BASE_URL) needs --version"
    if [ -n "$TOKEN" ]; then
        auth_api "releases/latest" "$WORK/latest.json" \
            || die "cannot read the latest release from GitHub (is GH_TOKEN valid? unset it to go without)"
        VERSION="$(tr ',' '\n' < "$WORK/latest.json" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)"
    else
        # Without a token: the latest release page redirects to its tag.
        VERSION="$(curl -fsSI "https://github.com/$REPO/releases/latest" 2>/dev/null \
            | sed -n 's#^[Ll]ocation: .*/tag/v\{0,1\}\([0-9][^[:space:]]*\).*#\1#p' | tr -d '\r' | head -n 1)" || true
        [ -n "$VERSION" ] || die "cannot find the latest release on github.com/$REPO: check your connection, or pass --version"
    fi
    [ -n "$VERSION" ] || die "cannot find the latest release version"
fi

NAME="wats-$VERSION-$TARGET"
ARCHIVE="$NAME.tar.gz"
FILES="$ARCHIVE $ARCHIVE.minisig $ARCHIVE.sha256 $ARCHIVE.sha256.minisig"
say "Installing wats $VERSION for $TARGET into $PREFIX"

# --- Download ------------------------------------------------------------

if [ -n "$BASE_URL" ]; then
    for f in $FILES; do
        fetch "$BASE_URL/v$VERSION/$f" "$WORK/$f" || die "cannot download $f from $BASE_URL"
    done
elif [ -n "$TOKEN" ]; then
    # With a token: release assets come through the API, by id.
    auth_api "releases/tags/v$VERSION" "$WORK/release.json" \
        || die "release v$VERSION not found, or GH_TOKEN is not valid (unset it to go without)"
    # Each asset object starts with its API "url" and later has its "name".
    # shellcheck disable=SC2020 # three separators, each to a newline
    tr ',{}' '\n\n\n' < "$WORK/release.json" > "$WORK/release.lines"
    for f in $FILES; do
        asset="$(awk -v want="$f" '
            /"url": *"https:\/\/api\.github\.com\/repos\/[^"]*\/releases\/assets\/[0-9]+"/ {
                u = $0; sub(/^[^"]*"url": *"/, "", u); sub(/".*$/, "", u); last = u
            }
            $0 ~ "\"name\": *\"" want "\"" { print last; exit }
        ' "$WORK/release.lines")"
        [ -n "$asset" ] || die "release v$VERSION has no $f (no prebuilt binary for $TARGET, or not signed)"
        fetch "$asset" "$WORK/$f" -H "Authorization: Bearer $TOKEN" -H "Accept: application/octet-stream" \
            || die "cannot download $f"
    done
else
    for f in $FILES; do
        fetch "https://github.com/$REPO/releases/download/v$VERSION/$f" "$WORK/$f" \
            || die "cannot download $f: check that release v$VERSION exists and is signed, and your connection"
    done
fi

# --- Verify --------------------------------------------------------------

# verify FILE: the signature must verify with the release key, and its
# trusted (signed) comment must name FILE, so an older signed archive
# served under another name is refused.
verify() {
    out="$(minisign -Vm "$WORK/$1" -x "$WORK/$1.minisig" -P "$PUBKEY" 2>&1)" \
        || die "the signature of $1 does not verify with the release key: $out"
    printf '%s\n' "$out" | grep -Eq "^Trusted comment: (.*[[:space:]])?file:$1([[:space:]]|\$)" \
        || die "the signature of $1 was made for another file: $out"
}
verify "$ARCHIVE"
verify "$ARCHIVE.sha256"

expected="$(cut -d ' ' -f 1 < "$WORK/$ARCHIVE.sha256")"
named="$(sed -n '1s/^[0-9a-fA-F]* [ *]*//p' < "$WORK/$ARCHIVE.sha256")"
[ "$named" = "$ARCHIVE" ] || die "$ARCHIVE.sha256 is the checksum of another file ($named)"
actual="$(sha256 "$WORK/$ARCHIVE")"
[ "$(printf '%s' "$expected" | tr 'A-F' 'a-f')" = "$actual" ] \
    || die "SHA-256 mismatch for $ARCHIVE: expected $expected, got $actual"
say "Signature and checksum verified."

# --- Extract and check ---------------------------------------------------

mkdir "$WORK/x"
tar -xzf "$WORK/$ARCHIVE" -C "$WORK/x" "$NAME/wats" 2>/dev/null \
    || die "the archive has no $NAME/wats"
BIN="$WORK/x/$NAME/wats"
if [ ! -f "$BIN" ] || [ -L "$BIN" ]; then
    die "$NAME/wats is not a regular file"
fi
chmod 755 "$BIN"
got="$("$BIN" --version 2>/dev/null || true)"
[ "$got" = "wats $VERSION" ] || die "the downloaded binary does not run here, or is not wats $VERSION (it says: ${got:-nothing})"

# --- Install -------------------------------------------------------------

mkdir -p "$PREFIX" 2>/dev/null || die "cannot create $PREFIX (this script never uses sudo: choose a --prefix you can write)"
[ -w "$PREFIX" ] || die "cannot write to $PREFIX (this script never uses sudo: choose a --prefix you can write)"
tmp="$PREFIX/.wats.install.$$"
if ! { cp "$BIN" "$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$PREFIX/wats"; }; then
    rm -f "$tmp"
    die "cannot install into $PREFIX"
fi

say "Installed: $("$PREFIX/wats" --version)"
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) say "Note: $PREFIX is not on your PATH. Add it, e.g. in your shell profile:"
       say "  export PATH=\"$PREFIX:\$PATH\"" ;;
esac
say "Next: wats setup   (then \`wats upgrade\` keeps it up to date)"
