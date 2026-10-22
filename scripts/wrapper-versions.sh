#!/bin/sh
# The npm and PyPI wrappers (packages/, ADR 0035) run the wats release of
# their own version, so their versions must equal the wats version
# ([workspace.package] in Cargo.toml).
#
#   sh scripts/wrapper-versions.sh check   # fails when one differs (CI)
#   sh scripts/wrapper-versions.sh set     # writes Cargo.toml's version
#                                          # into both (release PRs)
#
# Run from anywhere. Files: packages/npm/package.json ("version") and
# packages/pypi/src/wats_wrapper/__init__.py (__version__).

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NPM="$ROOT/packages/npm/package.json"
PYPI="$ROOT/packages/pypi/src/wats_wrapper/__init__.py"

die() { printf 'wrapper-versions.sh: %s\n' "$*" >&2; exit 1; }

cargo="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\([^"]*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
case "$cargo" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) die "no X.Y.Z version in [workspace.package] of Cargo.toml (got '$cargo')" ;;
esac

npm_version() { sed -n 's/^  "version": "\([^"]*\)",$/\1/p' "$NPM" | head -n 1; }
pypi_version() { sed -n 's/^__version__ = "\([^"]*\)"$/\1/p' "$PYPI" | head -n 1; }

# rewrite FILE SED-EXPRESSION: in place, portably (no sed -i).
rewrite() {
    tmp="$1.tmp.$$"
    sed "$2" "$1" > "$tmp" && mv "$tmp" "$1"
}

case "${1:-}" in
    check)
        npm="$(npm_version)"
        pypi="$(pypi_version)"
        ok=true
        if [ "$npm" != "$cargo" ]; then
            printf 'packages/npm/package.json has version %s, Cargo.toml %s\n' "${npm:-none}" "$cargo" >&2
            ok=false
        fi
        if [ "$pypi" != "$cargo" ]; then
            printf 'packages/pypi/src/wats_wrapper/__init__.py has __version__ %s, Cargo.toml %s\n' "${pypi:-none}" "$cargo" >&2
            ok=false
        fi
        $ok || die "run: sh scripts/wrapper-versions.sh set"
        echo "npm and PyPI wrappers: version $cargo, as Cargo.toml"
        ;;
    set)
        [ -n "$(npm_version)" ] || die "no \"version\" line in $NPM"
        [ -n "$(pypi_version)" ] || die "no __version__ line in $PYPI"
        rewrite "$NPM" "s/^  \"version\": \"[^\"]*\",\$/  \"version\": \"$cargo\",/"
        rewrite "$PYPI" "s/^__version__ = \"[^\"]*\"\$/__version__ = \"$cargo\"/"
        if [ "$(npm_version)" != "$cargo" ] || [ "$(pypi_version)" != "$cargo" ]; then
            die "could not set the versions"
        fi
        echo "npm and PyPI wrappers set to $cargo"
        ;;
    *)
        die "usage: sh scripts/wrapper-versions.sh check|set"
        ;;
esac
