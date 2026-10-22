#!/bin/sh
# Regenerates the signed fixtures of the npm and PyPI wrapper tests
# (packages/npm/test, packages/pypi/tests; ADR 0035), with a throwaway
# minisign key whose secret is deleted at the end. Never the release key.
#
#   sh packages/fixtures/make-fixtures.sh
#
# Writes, next to this script, for a made-up release 9.9.9:
#
#   test.pub                  the throwaway public key (base64 line only)
#   good/                     wats-9.9.9-<target>.tar.gz, its .sha256 and
#                             both .minisig, for x86_64-unknown-linux-musl
#                             (a ustar archive, as GNU tar on the Linux
#                             release leg makes) and aarch64-apple-darwin
#                             (pax, with a long-named entry, carried by a
#                             pax header, and a decoy `wats` at the root
#                             of the archive, both before the binary)
#   other-key/                the Linux archive's .minisig by another key
#   wrong-file/               a .minisig of the same archive whose trusted
#                             comment names wats-9.9.8-…: a replay
#   bad-sha256/               a .sha256 with another hash, correctly signed
#   legacy/                   a legacy (non-prehashed, `minisign -l`)
#                             .minisig, which wats refuses too
#   wrong-version/            a correctly signed 9.9.9 archive whose binary
#                             says `wats 9.9.8`
#   security-high/            security.json and its .minisig (ADR 0030) of a
#                             latest release 9.9.10 whose floor is 9.9.10
#   security-low/             the same with a floor of 9.9.0
#   security-other-key/       security-high's file, signed by another key
#
# The "binary" is a shell script (fake-wats.sh) standing in for wats.
# Needs minisign (0.12, as the release workflow), tar, and shasum or
# sha256sum.

set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/wats-wrapper-fixtures.XXXXXX")"
trap 'rm -rf "$T"' EXIT INT TERM

VERSION=9.9.9
LINUX=x86_64-unknown-linux-musl
DARWIN=aarch64-apple-darwin

if command -v shasum >/dev/null 2>&1; then
    sha256line() { shasum -a 256 "$1"; }
else
    sha256line() { sha256sum "$1"; }
fi

# Throwaway keys, without password, in the temporary directory only.
minisign -G -W -p "$T/test.pub" -s "$T/test.key" >/dev/null
minisign -G -W -p "$T/other.pub" -s "$T/other.key" >/dev/null

# sign KEY DIR FILE [-l]: FILE.minisig, made in DIR so that the trusted
# comment names the bare file name (file:FILE), as in the release workflow.
sign() {
    key="$1"; dir="$2"; file="$3"; shift 3
    (cd "$dir" && minisign -S -s "$key" -m "$file" "$@" </dev/null >/dev/null)
}

# archive DIR NAME FORMAT SAYS [EXTRA]: DIR/NAME.tar.gz holding NAME/wats, a
# copy of fake-wats.sh printing `wats SAYS` on --version.
archive() {
    dir="$1"; name="$2"; format="$3"; says="$4"; extra="${5:-}"
    work="$T/build-$name-$says"
    rm -rf "$work"
    mkdir -p "$work/$name"
    sed "s/@VERSION@/$says/" "$HERE/fake-wats.sh" > "$work/$name/wats"
    chmod 755 "$work/$name/wats"
    printf 'Apache-2.0 (fixture)\n' > "$work/$name/LICENSE"
    members="$name/wats $name/LICENSE"
    if [ "$extra" = decoys ]; then
        long="$name/a-file-name-longer-than-the-hundred-bytes-of-a-ustar-name-field-so-that-pax-has-to-carry-it.txt"
        printf 'decoy\n' > "$work/$long"
        printf '#!/bin/sh\necho decoy\n' > "$work/wats"
        members="$long wats $name/LICENSE $name/wats"
    fi
    # shellcheck disable=SC2086 # members is a list of paths without spaces
    (cd "$work" && COPYFILE_DISABLE=1 tar --format "$format" -czf "$dir/$name.tar.gz" $members)
}

# release DIR TARGET FORMAT SAYS [EXTRA]: the four release files.
release() {
    dir="$1"; target="$2"; format="$3"; says="$4"; extra="${5:-}"
    name="wats-$VERSION-$target"
    mkdir -p "$dir"
    archive "$dir" "$name" "$format" "$says" "$extra"
    (cd "$dir" && sha256line "$name.tar.gz" > "$name.tar.gz.sha256")
    sign "$T/test.key" "$dir" "$name.tar.gz"
    sign "$T/test.key" "$dir" "$name.tar.gz.sha256"
}

out="$T/out"
mkdir -p "$out"
tail -n 1 "$T/test.pub" > "$out/test.pub"

release "$out/good" "$LINUX" ustar "$VERSION"
release "$out/good" "$DARWIN" pax "$VERSION" decoys
release "$out/wrong-version" "$LINUX" ustar 9.9.8

N="wats-$VERSION-$LINUX.tar.gz"

mkdir -p "$out/other-key" "$out/wrong-file" "$out/bad-sha256" "$out/legacy"
cp "$out/good/$N" "$T/$N"
sign "$T/other.key" "$T" "$N"
mv "$T/$N.minisig" "$out/other-key/$N.minisig"

replay="wats-9.9.8-$LINUX.tar.gz"
cp "$out/good/$N" "$T/$replay"
sign "$T/test.key" "$T" "$replay"
mv "$T/$replay.minisig" "$out/wrong-file/$N.minisig"

sign "$T/test.key" "$T" "$N" -l
mv "$T/$N.minisig" "$out/legacy/$N.minisig"

# security DIR FLOOR [KEY]: DIR/security.json for a latest 9.9.10, signed.
security() {
    mkdir -p "$1" "$T/sec"
    printf '{"version":"9.9.10","security_floor":"%s"}\n' "$2" > "$T/sec/security.json"
    sign "${3:-$T/test.key}" "$T/sec" security.json
    mv "$T/sec/security.json" "$T/sec/security.json.minisig" "$1/"
}
security "$out/security-high" 9.9.10
security "$out/security-low" 9.9.0
security "$out/security-other-key" 9.9.10 "$T/other.key"

printf '%064d  %s\n' 0 "$N" > "$T/$N.sha256"
sign "$T/test.key" "$T" "$N.sha256"
mv "$T/$N.sha256" "$T/$N.sha256.minisig" "$out/bad-sha256/"

# Check what was made with minisign itself before replacing the fixtures.
pub="$(cat "$out/test.pub")"
for f in "$out/good/"*.tar.gz "$out/good/"*.sha256 "$out/wrong-version/"*.tar.gz; do
    minisign -V -q -P "$pub" -m "$f" >/dev/null
done

rm -f "$T/test.key" "$T/other.key"
for d in good other-key wrong-file bad-sha256 legacy wrong-version security-high security-low security-other-key; do
    rm -rf "${HERE:?}/$d"
    cp -R "$out/$d" "$HERE/$d"
done
cp "$out/test.pub" "$HERE/test.pub"
echo "fixtures written to $HERE (secret keys deleted)"
