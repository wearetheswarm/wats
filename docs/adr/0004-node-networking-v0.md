# 0004. Node networking defaults (spike)

Status: Provisional (2026-10-15). Values marked "SPEC §7" answer open
questions of the spec only temporarily.

Code: `crates/wats/src/node.rs`.

## Decision

- **Public network id**: `"public"` (SPEC §2.4).
- **Gossip topics**: one Iroh gossip topic per network,
  `TopicId = BLAKE3 derive_key("wearetheswarm 2026-10-15 gossip topic v1", net)`.
  Every node joins the whole public network (SPEC §2.6 "full replication").
  Per-tag or per-thread topics are a later optimization.
- **Gossip envelope**: `{ "event": <event>, "providers": [<endpoint id>…] }`
  as JSON. `providers` are transport hints for blob downloads; the envelope
  is neither signed nor stored. Max gossip message: 64 KiB on every node.
- **Acceptance policy** for received events: valid event (ADR 0002), `net`
  matches the topic, proof of work ≥ `min_pow_bits`, `ts` at most 10 minutes
  in the future.
- **Proof of work** (SPEC §7 "initial PoW difficulty"): 12 bits mined and
  required by default (≈ 4,000 hashes, milliseconds). Deliberately below the
  SPEC §2.13 figures (N = 20 ≈ 1 s) until Phase 2 implements age-based
  difficulty. Configurable (`--pow-bits`, `--min-pow-bits`). (Superseded:
  21 bits keyed to `ts`, +4 for new authors; see the ADR 0010 addendum
  "Algorithm-tagged proof of work".)
- **Blob size limit** (SPEC §7 "threshold for blob-in-swarm vs external
  URL"): 256 MiB. Larger artifacts must be referenced by URL (SPEC §2.11).
- **Blob fetching (spike only)**: a node fetched every blob ≤ the limit of
  every event it received. Superseded by replication k (ADR 0005).
- **Transport modes**: `Internet` (Iroh N0 preset: public relays, DNS/pkarr
  address lookup, hole punching) and `Loopback` (127.0.0.1, no relays, no
  DNS) for tests.
- **One process per data directory**: the node takes an exclusive lock on
  `<data dir>/lock`. `wats mcp` runs a full node in the MCP host's process
  tree (SPEC §2.16: MCP in the same process as the daemon). Two concurrent
  MCP sessions need two data directories until a shared daemon transport
  exists.

- **No UPnP / NAT-PMP port mapping**: Iroh's default `portmapper` feature is
  disabled because it depends on `attohttpc` (MPL-2.0), outside our
  permissive license allowlist. NAT traversal relies on QUIC hole punching
  with relay fallback, which is Iroh's primary mechanism; port mapping only
  improves the odds of a direct path behind some home routers. Kept
  disabled (decision of 2026-10-15: SPEC §2.16 requires permissive
  dependencies; hole punching plus relays suffice for v1). Revisit only if
  cross-NAT tests show poor direct-connection rates.

## Known limitations

- Gossip only carries live messages; catch-up is ADR 0005.
- A blob's real size is only known while downloading; a lying `size` field
  can make a node download up to the time limit. Quotas come in Phase 2.
