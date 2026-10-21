#!/bin/sh
# Print the security floor of release VERSION (ADR 0030): the newest
# version <= VERSION whose CHANGELOG section (a "## X.Y.Z (date)" heading)
# has a "### Security" heading ("### Security fixes" counts too), or 0.0.0
# when none has. Stateless: it reads only the CHANGELOG given, which the
# release workflow reads at the release's tag.
#
#   sh scripts/security-floor.sh VERSION [CHANGELOG]
#
# A node older than the floor counts the latest release as a security
# release (`security.json`, signed by the release workflow).

set -eu

die() { printf 'security-floor.sh: %s\n' "$*" >&2; exit 1; }

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  die "usage: sh scripts/security-floor.sh VERSION [CHANGELOG]"
fi
VERSION="${1#v}"
CHANGELOG="${2:-$(dirname "$0")/../CHANGELOG.md}"
printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "$1 is not X.Y.Z"
[ -r "$CHANGELOG" ] || die "cannot read $CHANGELOG"

awk -v want="$VERSION" '
  function key(v,  p) { split(v, p, "."); return sprintf("%09d%09d%09d", p[1], p[2], p[3]) }
  /^## / {
    section = ""
    if ($2 ~ /^[0-9]+\.[0-9]+\.[0-9]+$/) section = $2
    next
  }
  /^### Security([ \t]|$)/ {
    if (section != "" && key(section) <= key(want) && (best == "" || key(section) > key(best)))
      best = section
  }
  END { print (best == "" ? "0.0.0" : best) }
' "$CHANGELOG"
