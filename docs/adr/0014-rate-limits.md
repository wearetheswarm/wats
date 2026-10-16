# 0014. Per-peer rate limits

Status: Provisional (2026-10-16), a first robustness step before strangers
run nodes.

Code: `crates/wats/src/limits.rs`, `Node::handle_request`.

## Decision

- **Token buckets** per peer and request type on `/wats`: `hello`, `sync`,
  `replicate`, `crash_report`. A bucket refills at `per_min / 60` tokens
  a second, up to a burst of a quarter of a minute's worth (at least one).
  Defaults per peer and minute:

  | Request | Default | Key |
  |---|---|---|
  | `hello` | 60 | `limits.hello_per_min` |
  | `sync` (pages of up to 256 events) | 600 | `limits.sync_per_min` |
  | `replicate` | 120 | `limits.replicate_per_min` |
  | `crash_report` | 10 | `limits.report_per_min` |

  `0` means unlimited.
- **Over the limit**, the request gets `Refused { "rate limited (…): retry
  later" }`, and the node logs "peer throttled", at most once a minute per
  peer and request type.
- **Blob fetches:** at most `limits.blob_fetches_per_peer` (4) blobs that
  one peer makes us fetch at once (replication requests). Further blobs of
  a request are skipped and the peer is told which ones were stored.

## Consequences

- A new node catches up on a million events in about 7 minutes at the
  default `sync` rate.
- Peers are identified by their node id. One machine running many ids
  gets one budget per id: this bounds accidental floods, not a
  determined attacker. Sybil resistance comes with reputation and
  proof of work (Phase 2).
- Gossip is not covered here; Iroh bounds it separately.
