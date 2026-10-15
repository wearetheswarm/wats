#!/usr/bin/env bash
# Build the static Linux binary that releases ship (`x86_64-unknown-linux-musl`,
# release profile, mimalloc), locally, in Docker or OrbStack. Use it to try a
# change from `main` on Linux or WSL without cutting a release.
#
#   scripts/build-static.sh                      # x86_64, as in releases
#   scripts/build-static.sh --target aarch64-unknown-linux-musl   # native, fast on Apple silicon
#   scripts/build-static.sh --image              # also build the Dockerfile.release image
#
# Output, packaged like a release asset:
#   dist/wats-<version>-<target>.tar.gz and .sha256
#
# Note: on Apple silicon the default x86_64 build runs under emulation
# (linux/amd64), so it is slower: measured cold on an M-series Mac with
# OrbStack, about 2.5 min for x86_64 against 1 min for the native aarch64
# build. Slower emulators (QEMU) can take much longer. Rebuilds are
# incremental: the cargo registry and the target directory
# live in Docker volumes (wats-static-*). Remove them with
#   docker volume rm $(docker volume ls -q --filter name=wats-static-)
set -euo pipefail

target="x86_64-unknown-linux-musl"
image=false
while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="${2:?--target needs a value}"; shift 2 ;;
    --target=*) target="${1#--target=}"; shift ;;
    --image) image=true; shift ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done

case "$target" in
  x86_64-unknown-linux-musl) platform="linux/amd64"; arch="amd64" ;;
  aarch64-unknown-linux-musl) platform="linux/arm64"; arch="arm64" ;;
  *) echo "unsupported target $target: use x86_64-unknown-linux-musl or aarch64-unknown-linux-musl" >&2; exit 2 ;;
esac

repo="$(cd "$(dirname "$0")/.." && pwd)"
version="$(grep -m1 '^version = ' "$repo/Cargo.toml" | cut -d'"' -f2)"
name="wats-$version-$target"
# Same pinned toolchain image as ./Dockerfile.
rust_image="$(sed -n 's/^ARG RUST_IMAGE=//p' "$repo/Dockerfile")"
builder="wats-static-builder:$arch"

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }

say "Preparing the $platform builder ($rust_image + musl-tools)"
docker build -q --platform "$platform" -t "$builder" - >/dev/null <<DOCKERFILE
FROM $rust_image
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add $target
DOCKERFILE

say "Building $target (release profile, locked)"
mkdir -p "$repo/dist"
start=$SECONDS
docker run --rm --platform "$platform" \
  -v "$repo:/src:ro" \
  -v "wats-static-cargo-$arch:/usr/local/cargo/registry" \
  -v "wats-static-target-$arch:/target" \
  -v "$repo/dist:/out" \
  -e CARGO_TARGET_DIR=/target \
  -e RUSTUP_TOOLCHAIN=1.99.0 \
  -e TARGET="$target" \
  -e OWNER="$(id -u):$(id -g)" \
  -w /src "$builder" \
  sh -c 'cargo build --release --locked --target "$TARGET" -p wats \
    && install -m 0755 "/target/$TARGET/release/wats" /out/wats.tmp \
    && chown "$OWNER" /out/wats.tmp'
echo "    built in $((SECONDS - start)) s"

say "Packaging dist/$name.tar.gz"
cd "$repo/dist"
rm -rf "$name" "$name.tar.gz" "$name.tar.gz.sha256"
mkdir "$name"
mv wats.tmp "$name/wats"
cp "$repo/LICENSE" "$repo/README.md" "$name/"
tar -czf "$name.tar.gz" "$name"
if command -v sha256sum >/dev/null; then
  sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else
  shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
fi
file "$name/wats" 2>/dev/null || true
cat "$name.tar.gz.sha256"

if $image; then
  tag="wats:static-$version-$arch"
  say "Building the release image $tag (Dockerfile.release)"
  context="$(mktemp -d "${TMPDIR:-/tmp}/wats-image.XXXXXX")"
  trap 'rm -rf "$context"' EXIT
  mkdir "$context/data"
  cp "$name/wats" "$context/wats"
  docker build -q --platform "$platform" -f "$repo/Dockerfile.release" -t "$tag" "$context" >/dev/null
  echo "    $tag"
fi

say "Done: dist/$name.tar.gz"
