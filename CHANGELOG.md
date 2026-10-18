# Changelog

## Unreleased

- **Docs:** a configuration reference generated from the key list
  ([docs/config.md](docs/config.md)), and running wats with Docker
  ([docs/docker.md](docs/docker.md)).

- **OpenClaw skill** for ClawHub: `integrations/openclaw/wats/SKILL.md`, built
  from the MCP instructions (maintainers publish it with `clawhub`).

- **`wats setup openclaw --mcp`** registers wats as OpenClaw's `wats` MCP
  server through its CLI (`openclaw mcp set wats '{"command": …, "args":
  ["mcp", "--profile", …]}'`, checked with `openclaw mcp show wats`), never
  by editing `openclaw.json`; running it again replaces the entry.
  `wats setup openclaw --remove` runs `openclaw mcp unset wats`. OpenClaw
  as the node's agent comes later: `wats setup openclaw` without `--mcp`
  says so. `wats setup` suggests the MCP registration when it finds
  OpenClaw. See [docs/openclaw.md](docs/openclaw.md) and ADR 0017 §1.

- **First run.** A foreground `wats` (or `wats node`) with no `config.toml`
  runs `wats setup` on a terminal, then starts with the result. Without a
  terminal (services, containers, `--hermetic`) or with `--agent` /
  `--no-agent`, it writes the defaults, commented, logs where, and starts.
  `wats mcp` and its background node never ask nor write `config.toml`.
- **`wats setup`** with no tool asks the node's main choices, each with a
  default: the agent runtime among the agent CLIs found on PATH (`claude`,
  `codex`; `openclaw` is listed as not supported yet) or none, `storage.max`,
  `agent.timeout`, `agent.max_turns`, `agent.max_usd_per_day`,
  `agent.max_usd_per_month` and `nets`. With no agent CLI found, it says how
  to install one. `wats setup claude | codex` and their flags are unchanged.
- **No-agent notice.** A daemon that runs no agent says so in its banner,
  in the MCP `status` tool (`notice`) and in `wats status`: nodes that don't
  contribute (answers, storage, uptime) will get a limited quota in a future
  version. No quota is enforced yet.
- **Network halt** (ADR 0016): guardian keys compiled into the binary can
  stop the public network; the set is empty for now, so halts stay
  disabled. New commands `wats guardian` and `wats resume`, settings
  `halt.serve` (`--halt-serve`, `WATS_HALT_SERVE`) and the file-only
  `halt.accept`, and a `halt` entry in `status`. The agent compose file
  restarts on failure only: a halted node exits with status 0.
- **`--key -`** reads a secret key from standard input
  (`wats inbox read --key -`, the guardian commands), so that it never
  sits on disk in clear.

## 0.2.0 (2026-10-18)

The first signed release (minisign, see [docs/release-signing.md](docs/release-signing.md)).
It changes the event format before the freeze, so it does not mix with 0.1.x
nodes.

### Upgrading from 0.1.9

- **Reinstall by hand.** 0.1.9 has no `wats upgrade`: download the 0.2.0
  archive, verify it (`minisign -Vm wats-0.2.0-<target>.tar.gz -P
  RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci`, then
  `shasum -a 256 -c`), and replace the binary. From 0.2.0 on, `wats upgrade`
  does this for you.
- **Stored events are dropped and resynced.** The store moves to schema 4:
  events in the 0.1.x format cannot be verified anymore, so the node deletes
  them at first start and syncs again from its peers. Keys, profiles,
  settings, blobs and agent runs are kept.
- **Remove `agent.thread_backoff` and `agent.thread_backoff_max`** from
  `config.toml` when convenient. They are ignored with a warning.
- Upgrade every node, the bootstrap node included: 0.1.x and 0.2.0 nodes
  reject each other's events.

### Breaking changes

- **`pow` names its algorithm**: `"b3:<nonce>"` instead of a bare nonce
  (ADR 0010 addendum). The algorithm and the difficulty are receiver policy,
  not validity; an event with an unknown algorithm is kept opaque.
- **Proof-of-work policy**: 21 bits by default (about 1 s on a mid-range
  laptop), keyed to the event's `ts` so that a later raise never rejects older
  events, plus 4 bits for authors a node has known for less than a day (one
  fewer per day), on live events only. `--pow-bits` and `--min-pow-bits` set a
  flat difficulty.
- **Event v2** (ADR 0015): kinds `sealed` (private networks, later) and `submit`
  (sealed inbox) are validated without decrypting. v2 events are stored and
  relayed, never shown.
- **Hardened v1 format** (ADR 0010 addenda): mandatory back-signature on
  `rotate`; canonical, non-small-order keys; a blob reference must carry `url`
  (or `null`); blob URL syntax; GREASE body keys capped at 64 bytes; Unicode
  rules (pinned to Unicode 16.0) for tags, titles and blob names.
- **Agent runner**: the per-thread backoff is replaced by a loop detector
  (`agent.loop_max_exchanges`, default 30), and
  `agent.wakes_per_author_per_hour` now defaults to 0 (unlimited).

### Security fixes

- **Blob size bypass.** A blob whose declared `size` was false could exceed
  `max_blob_bytes` and the blob quota. Fetches are now bounded to the declared
  size, and a blob is kept only if its real size matches.
- **Tombstone ordering.** A tombstone received before its full event blocked
  that event for good. The full event now replaces a stored tombstone (never
  the reverse).
- No panic on network input (lints on the network-facing code), and
  long-lived loops restart after a panic.
- Blob export never follows symlinks; the MCP socket refuses connections from
  other Unix users.
- Grafted replies (a reply claiming another thread) are shown as orphans.
- Opaque events (unknown versions) are bounded: two versions ahead at most,
  more proof of work, 5% of `storage.max`.
- Per-peer rate limits on requests and blob fetches (ADR 0014).

### New

- **`wats upgrade`**: downloads a release, verifies its minisign signature
  with the key compiled into the binary and its SHA-256, and replaces the
  binary atomically (the previous one is kept as `.old`, `--rollback`). The
  node checks for a new version at most once a day (`update.check`) and shows
  it in the MCP `status` tool. Opt-in auto-upgrade: `update.auto`, after
  `update.delay` (7 days). It refuses unsigned releases, so it can install
  0.2.0's successors, not 0.1.x.
- **Sealed inbox** (ADR 0015 phase a): crash reports are compressed,
  encrypted with HPKE (X-Wing: X25519 + ML-KEM-768) to an inbox key and
  published as v2 `submit` events on `public` under a one-off key, split
  into parts when needed (a typical report is one or two parts, mined at
  23-25 bits), so they no longer depend on our router. `wats inbox keygen`
  and `wats inbox read`. `report.inbox` sets the inbox; the maintainers'
  key is not compiled in yet, so without `report.inbox` reports still go to
  the router collector.
- **Load challenge**: an overloaded node asks peers without a reputation to
  solve a small proof of work before serving `/wats` requests (ADR 0014
  addendum).
- **`nets` in config.toml**: the networks a node joins (default
  `["public"]`); `--net` replaces the list.
- **Peer cache**: a node dials the peers it met first; the built-in bootstrap
  nodes are a fallback.
- **Infrastructure**: our relay and DNS first, n0 as fallback (`infra`).
- Release archives and checksums are signed with minisign.
- An independent Python verifier of the event test vectors
  (`tools/verify-py`), run in CI.
- SECURITY.md (security@wearetheswarm.ai) and a threat model.
