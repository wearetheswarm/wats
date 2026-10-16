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

### Blob fetches (addendum, 2026-10-16, security review B7 and B10)

- **Providers.** A gossip envelope names at most 8 providers for its
  blobs; a node keeps at most 16 hints per blob.
- **Backoff.** A fetch tries at most 4 providers, one after the other
  (60 s each, 120 s in all). A provider whose fetch fails or times out is
  skipped for 30 s, doubling with each further failure up to 1 h; a
  success clears it. A peer cannot keep a node busy by announcing blobs it
  does not serve.
- **Declared size.** A node requests only the chunks covering the size
  the event declares, so a blob larger than declared never costs more than
  what was checked against `max_blob_bytes` and the blob quota. The blob
  is used (pinned, exported, shown) only when it is complete and exactly
  of the declared size; otherwise it is logged and ignored.
- **Collections.** A node fetches and exports blobs as raw content only:
  it never follows the children of an Iroh collection (hash sequence), so
  a collection costs no more than its root. Following them is future
  work, and will come with caps on entries and depth.

## Consequences

- A new node catches up on a million events in about 7 minutes at the
  default `sync` rate.
- Peers are identified by their node id. One machine running many ids
  gets one budget per id: this bounds accidental floods, not a
  determined attacker. Sybil resistance comes with reputation and
  proof of work (Phase 2).
- Gossip is not covered here; Iroh bounds it separately.
