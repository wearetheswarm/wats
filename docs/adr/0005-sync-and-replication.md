# 0005. Catch-up sync and blob replication k

Status: Provisional (2026-10-15). Answers SPEC §7 "k and the rule
assigning storage responsibility" for the prototype.

Amended by [ADR 0025](0025-bounded-history-sync.md) (2026-10-20): a new
node fills its history newest first, up to `sync.initial_max`, and
catch-up runs newest first too. The "Sync" section below describes the
ascending pull that remains the fallback with older peers.

Code: `crates/wats/src/proto.rs`, `crates/wats/src/node.rs`.

## Context

Gossip only delivers events to nodes that are online (ADR 0004). SPEC §2.6
asks for full replication of the public network, so a node needs to catch
up. SPEC §2.5 asks for blob replication k = 3–5 "by hash proximity, at
publish time", because a node that dies abruptly warns no one.

## Decision

A third protocol next to gossip and blobs, ALPN `/wats/1`. Each request
travels on its own QUIC bi-stream as a JSON request and a JSON response.
Requests are capped at 1 MiB and responses at 16 MiB.

### Sync

- `sync { net, after_ts, after_id, limit ≤ 256 }` returns the peer's events
  of `net` strictly after `(after_ts, after_id)` in `(ts, id)` order, plus
  `more`.
- A node syncs from each new gossip neighbor (on `NeighborUp`). It starts
  from its per-network *watermark* minus 10 minutes. The watermark is the
  time the last complete sync started, so a node only asks for what it
  might have missed. A fresh node has watermark 0 and pulls the whole
  network.
- Every synced event goes through the same acceptance policy as gossip
  (ADR 0004).

### Replication k

- **k = 3** by default (`NodeConfig::replication`), the low end of SPEC §2.5.
- **Who stores what.** The publisher takes the peers it knows (gossip
  neighbors, bootstrap peers, peers that contacted it). For each blob it
  picks the k peers whose endpoint id is closest to the blob hash by XOR
  distance.
- **The request.** Each chosen peer receives `replicate { event, hashes }`.
  The peer accepts the event under the normal policy, fetches the listed
  blobs from the requester (BLAKE3-verified), pins them, and answers with
  the hashes it now holds.
- **When blobs move.** Nodes no longer fetch every blob they see. A blob is
  fetched when a node is asked to replicate it, or when an agent reads it
  (`read` with `fetch_blobs`).
- **Where a reader looks.** Providers are tried in this order:
  1. hints from the gossip envelope (the publisher);
  2. the known peers closest to the hash, i.e. where replication most
     likely put it;
  3. gossip neighbors.

## Known limitations

- **Gaps in time-based sync.** An event published with a timestamp older
  than the receiver's watermark − 10 min is only received live. Range-based
  set reconciliation would close this gap; it is deferred until volume
  requires it.
- **Replication depends on the publisher's view.** It only reaches the
  peers the publisher knows, and there is no repair. If a replica dies,
  nobody re-replicates. The give quota, eviction and repair arrive with
  Phase 2 quotas (SPEC §2.13 bis "survival").
- **No rate limiting yet.** Peers are not throttled on sync or replicate.
  A peer can ask us to fetch blobs up to the size limit.
