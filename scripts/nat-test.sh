#!/usr/bin/env bash
# Cross-NAT test for wats (SPEC §5): two machines on different networks (for
# example home Wi-Fi and a phone hotspot) exchange a signed post and a 1 MiB
# blob, and report how they reached each other: direct (hole punched) or
# through a relay. Standalone: copy this one file to any machine (macOS on
# Apple silicon, Linux or WSL on x86_64); the repository is not needed.
#
# Default, symmetric: run the same command on both machines.
#   ./nat-test.sh
# Both nodes find each other through the built-in router bootstrap, each
# publishes one post with a blob once it knows two peers (the router and the
# other machine), prints the other side's post and blob, and after DURATION
# seconds (or Ctrl-C) prints a summary: the network path to each peer that
# is not the router.
#   "direct  Ip(...)"   hole punching worked
#   "relay   Relay(...)" traffic went through a relay
# Through the router, posts and blobs can also arrive via the router (gossip
# forwarding, router replica): only the path lines prove A <-> B connectivity.
#
# Strict mode, direct dial without the router:
#   ./nat-test.sh a              machine A: prints a ticket
#   ./nat-test.sh b <ticket>     machine B: dials A with it and publishes
#
# Logs go to the terminal and to ./wats-nat-<role>-<time>.log: send that file.
#
# Environment:
#   WATS=/path/to/wats   binary to use. Otherwise: ./wats next to this script,
#                        then `wats` on PATH, then a cargo build when run from
#                        the repository, then the release asset for this
#                        platform (needs gh; VERSION=x.y.z, default latest).
#   DURATION=180         seconds before the summary (default mode and b); the
#                        post waits up to half of it for the other machine
#   VERBOSE=1            Iroh socket debug logs
#   NO_COLOR=1           plain terminal output (the log file is always plain)
#   WATS_INFRA=wats      relay and DNS profile (default: the binary's, wats+n0)
#   WATS_NAT_DIR=...     data directory (default: a temp dir per role)
set -euo pipefail

role="${1:-sym}"
case "$role" in
  sym|a|b) ;;
  -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
  *) echo "usage: $0 | $0 a | $0 b <ticket>   (see --help)" >&2; exit 2 ;;
esac
here="$(cd "$(dirname "$0")" && pwd)"
dir="${WATS_NAT_DIR:-${TMPDIR:-/tmp}/wats-nat-$role}"
log="$PWD/wats-nat-$role-$(date +%Y%m%d-%H%M%S).log"
duration="${DURATION:-180}"
started="$(date +%s)"

case "$(uname -s)/$(uname -m)" in
  Darwin/arm64)  target=aarch64-apple-darwin ;;
  Linux/x86_64)  target=x86_64-unknown-linux-musl ;;
  *) echo "unsupported platform: $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

# --- Binary -----------------------------------------------------------------
wats="${WATS:-}"
[ -z "$wats" ] && [ -x "$here/wats" ] && wats="$here/wats"
[ -z "$wats" ] && wats="$(command -v wats || true)"
if [ -z "$wats" ] && [ -f "$here/../Cargo.toml" ] && command -v cargo >/dev/null; then
  echo "building wats with cargo" >&2
  (cd "$here/.." && cargo build --release --quiet --locked -p wats)
  wats="$(cd "$here/.." && cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/wats"
fi
if [ -z "$wats" ]; then
  command -v gh >/dev/null || {
    echo "no wats binary found and no gh to download one: copy wats-<version>-$target.tar.gz" >&2
    echo "from https://github.com/wearetheswarm/wats/releases next to this script and extract its wats" >&2
    echo "binary here, or set WATS=/path/to/wats" >&2
    exit 1
  }
  version="${VERSION:-$(gh release view -R wearetheswarm/wats --json tagName -q .tagName | sed 's/^v//')}"
  name="wats-$version-$target"
  echo "downloading $name" >&2
  tmp="$(mktemp -d)"
  gh release download "v$version" -R wearetheswarm/wats -D "$tmp" -p "$name.tar.gz" -p "$name.tar.gz.sha256"
  (cd "$tmp" && sha256 -c "$name.tar.gz.sha256" >&2)
  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
  mv "$tmp/$name/wats" "$here/wats"
  rm -rf "$tmp"
  wats="$here/wats"
fi
if [ "$target" = aarch64-apple-darwin ]; then
  xattr -d com.apple.quarantine "$wats" 2>/dev/null || true
fi

# --- Logs -------------------------------------------------------------------
# wats always adds the per-peer path lines (iroh::_events::path::selected).
if [ "${VERBOSE:-0}" = 1 ]; then
  export RUST_LOG="${RUST_LOG:-wats=debug,iroh::socket=debug,iroh=info}"
else
  export RUST_LOG="${RUST_LOG:-wats=info,iroh::socket=info,iroh=warn}"
fi
export RUST_LOG_STYLE=never

lan="$( (ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | cut -d' ' -f1) || true)"
public4="$(curl -4 -s --max-time 4 https://ifconfig.me 2>/dev/null || true)"
public6="$(curl -6 -s --max-time 4 https://ifconfig.me 2>/dev/null || true)"
{
  echo "wats:    $("$wats" --version 2>/dev/null || echo "$wats") ($target, $wats)"
  echo "role:    $role   data dir: $dir   infra: ${WATS_INFRA:-default (wats+n0)}"
  echo "network: LAN ${lan:-?}   public IPv4 ${public4:-none}   IPv6 ${public6:-none}"
  echo "         (the public addresses must differ between the two machines)"
  echo "log:     $log"
} | tee "$log" >&2

mkdir -p "$dir"
blob="$dir/nat-test.bin"
host="$(hostname -s 2>/dev/null || hostname)"

# --- Run --------------------------------------------------------------------
args=(--dir "$dir")
node_args=(node)
post() {
  head -c 1048576 /dev/urandom > "$blob"
  echo "blob sha256: $(sha256 "$blob" | cut -d' ' -f1)" | tee -a "$log" >&2
  # Wait for the router and the other machine (2 peers) before posting.
  node_args+=(--post "NAT test from $host ($role)" --kind note --tag test:nat
    --blob "$blob" --wait-peers "$1" --wait-timeout "$((duration / 2))")
}
case "$role" in
  sym)
    post 2
    ;;
  a)
    args+=(--no-default-bootstrap)
    echo "Strict mode: send the ticket below to machine B. Ctrl-C to stop and see the summary." >&2
    duration=0
    ;;
  b)
    ticket="${2:?usage: $0 b <ticket from machine A>}"
    args+=(--no-default-bootstrap --peer "$ticket")
    post 1
    ;;
esac

# Terminal view: a colored, condensed rendering of the log, through awk.
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then color=1; else color=0; fi
# shellcheck disable=SC2016
pretty_awk='
    function c(code, text) { return color ? "\033[" code "m" text "\033[0m" : text }
    function str(k,   p) {
      p = "\"" k "\":\""
      if (match($0, p "[^\"]*\"")) return substr($0, RSTART + length(p), RLENGTH - length(p) - 1)
      return ""
    }
    function num(k,   p) {
      p = "\"" k "\":"
      if (match($0, p "[0-9]+")) return substr($0, RSTART + length(p), RLENGTH - length(p))
      return ""
    }
    function kv(k,   p) {
      p = k "="
      if (match($0, p "[^ ]+")) return substr($0, RSTART + length(p), RLENGTH - length(p))
      return ""
    }
    /"type":"ready"/ {
      me = str("author")
      print c("1", "READY  ") "endpoint " substr(str("endpoint"), 1, 10) "  author " substr(me, 1, 10)
      if (role == "a") print c("1;36", "TICKET ") str("ticket")
      fflush(); next
    }
    /"type":"event"/ {
      a = str("author"); t = str("text"); b = str("name")
      line = "from " substr(a, 1, 10) ": " t (b != "" ? "  [" b "]" : "")
      if (a == me) print c("2", "post   (mine) " t)
      else if (num("ts") + 0 < since) print c("2", "post   (history) " line)
      else print c("1;32", "POST   ") line
      fflush(); next
    }
    /"type":"blob"/ {
      print c("1;32", "BLOB   ") str("name") " " num("size") " bytes  " substr(str("hash"), 1, 12)
      fflush(); next
    }
    /"type":"published"/ { print c("1", "SENT   ") "post " substr(str("id"), 1, 12); fflush(); next }
    /bootstrap peer/ { r = substr(kv("peer"), 1, 10); router[r] = 1; print c("36", "ROUTER " r " " kv("host")); fflush(); next }
    /path::selected/ {
      # Iroh prints span fields first: read the fields after the event name.
      $0 = substr($0, index($0, "path::selected"))
      r = kv("remote"); p = kv("network_path")
      who = (r in router) ? r " (router)" : r
      if (p ~ /^Ip/) print c((r in router) ? "32" : "1;32", "PATH   " who " direct " p)
      else print c("1;33", "PATH   " who " relay  " p)
      fflush(); next
    }
    /home_changed/ { next }
    /home is now relay/ {
      if (match($0, /relay https?:\/\/[^,]*/)) print c("36", "RELAY  home " substr($0, RSTART + 6, RLENGTH - 6))
      fflush(); next
    }
    /neighbor up/ { print c("36", "PEER   up " substr(kv("peer"), 1, 10)); fflush(); next }
    /error while shutting down gossip/ { next }
    /node started/ { print c("1", "START  ") "id " substr(kv("id"), 1, 10) "  infra " kv("infra"); fflush(); next }
    /wats::node: replication/ {
      n = gsub(/PublicKey\(/, "&")
      print c(n > 0 ? "1;32" : "33", "REPL   ") "blob held by " n " peer(s)"
      fflush(); next
    }
    /blob fetched/ { next }
    / ERROR / { print c("31", $0); fflush(); next }
    / WARN / { print c("33", $0); fflush(); next }
    { print; fflush() }
'

# The node writes to the log; the terminal follows it through pretty().
header_lines="$(wc -l < "$log")"
"$wats" "${args[@]}" "${node_args[@]}" >> "$log" 2>&1 &
pid=$!
# $! is the awk process: killing it ends the view (tail exits on SIGPIPE).
tail -n "+$((header_lines + 1))" -f "$log" \
  | awk -v color="$color" -v role="$role" -v since="$((started - 30))" "$pretty_awk" >&2 &
viewer=$!

# The summary text. A function, not inline in $(...): bash 3.2 (macOS)
# cannot parse a case statement inside a command substitution.
build_report() {
  local me="$1" routers="$2" found=0 remote path kind
  echo
  echo "===== summary ($host, $role) ====="
  # Only posts published since this run started (older ones arrive by sync).
  echo "posts from the other side (published during this run):"
  grep '"type":"event"' "$log" | grep '"NAT test from' | grep -v "\"author\":\"$me\"" \
    | sed -n 's/.*"text":"\([^"]*\)".*"ts":\([0-9]*\).*/\2 \1/p' \
    | awk -v since="$((started - 30))" '$1 >= since { $1 = ""; print " " $0 }' | sort -u || true
  echo "blobs fetched: $(grep -c '"type":"blob"' "$log" || true)"
  echo "paths to peers (last selected; router excluded: ${routers:-none}):"
  while read -r remote path; do
    if printf '%s\n' "$routers" | grep -qx "$remote"; then continue; fi
    found=1
    case "$path" in
      Ip*) kind=DIRECT ;;
      Relay*) kind=RELAY ;;
      *) kind=other ;;
    esac
    printf '  %s  %-6s %s\n' "$remote" "$kind" "$path"
  done < <(grep 'iroh::_events::path::selected' "$log" \
    | sed -n 's/.*remote=\([0-9a-f]*\) network_path=\(.*\) prev_network_path=.*/\1 \2/p' \
    | awk '{ last[$1] = $0 } END { for (r in last) print last[r] }')
  if [ "$found" = 0 ]; then
    echo "  NO SECOND MACHINE SEEN: the other machine never connected, directly or through a relay"
  fi
  echo "log: $log"
}

summary() {
  trap - INT TERM
  kill "$pid" 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
  sleep 1
  kill "$viewer" 2>/dev/null || true
  local me routers report
  me="$(sed -n 's/.*"author":"\([0-9a-f]\{64\}\)".*"type":"ready".*/\1/p' "$log" | head -1)"
  routers="$(sed -n 's/.*bootstrap peer peer=\([0-9a-f]\{10\}\).*/\1/p' "$log" | sort -u)"
  report="$(build_report "$me" "$routers")"
  printf '%s\n' "$report" >> "$log"
  if [ "$color" = 1 ]; then
    printf '%s\n' "$report" | sed \
      -e $'s/ DIRECT /\033[1;32m DIRECT \033[0m/' \
      -e $'s/ RELAY  /\033[1;33m RELAY  \033[0m/' \
      -e $'s/NO SECOND MACHINE SEEN/\033[1;31mNO SECOND MACHINE SEEN\033[0m/' \
      -e $'s/^=====.*/\033[1m&\033[0m/'
  else
    printf '%s\n' "$report"
  fi
}
trap 'summary; exit 0' INT TERM

if [ "$duration" -gt 0 ]; then
  echo "running for ${duration}s (DURATION), then the summary; Ctrl-C for it now" >&2
  for _ in $(seq "$duration"); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
else
  while kill -0 "$pid" 2>/dev/null; do sleep 1; done
fi
summary
