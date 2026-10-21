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
  Since 0.4.2 they first wait in a short queue, and publishers pace their
  pushes: see "Blob fetch queue and paced pushes" below.

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

### Load challenge (addendum, 2026-10-18)

Code: `crates/wats/src/challenge.rs`, `Node::admit`, `proto::call_solving`.

A proof-of-work challenge on `/wats`, in place of a refusal, when a node is
under load. Protocol only: events do not change.

- **Feature.** Nodes that can solve advertise `challenge` in
  `hello.features`. A node never sends a challenge to a peer that did not
  advertise it: an older peer over its rate limit gets
  `Refused { "rate limited (…): retry later" }` as before, and is served
  as before while the node is overloaded.
- **When.** A peer that advertised `challenge` gets one (1) when it is
  over its rate limit, or (2) when the node is overloaded, i.e. more than
  `overload_in_flight` (64) `/wats` requests in flight, unless it has an
  established reputation: it is in the persisted peer cache (met in an
  earlier run), or it completed `hello` with this node at least 10
  minutes ago. `limits.challenge = false` turns challenges off (refusals
  only); the node still solves the challenges of others.
- **Messages.** The response is
  `{"type":"challenge","alg":"b3","seed":"<64 hex>","effort":N,"expires":<unix s>}`.
  The requester resubmits the same request once, with a top-level
  `"solution":{"alg":"b3","seed":"<as received>","nonce":"<decimal>"}`.
  Older nodes ignore the field, like any unknown field; a malformed
  solution is ignored too. A requester gives up (as for `Refused`) when
  the `alg` is unknown, the effort is above 24 bits, or the challenge has
  expired. GREASE (ADR 0010, A8) applies: `_grease…` fields in a challenge
  or a solution are ignored.
- **Algorithm `b3`.** A BLAKE3 hashcash: find a decimal nonce (at most 20
  digits) such that `BLAKE3-derive_key("wats challenge v1", seed ||
  requester endpoint id (32 bytes) || responder endpoint id (32 bytes) ||
  nonce)` has at least `effort` leading zero bits. Both ids are hashed, so
  a solution is useless to another peer or at another node.
- **Seeds are stateless.** 32 bytes: `expires` (8, big endian), the
  effort (1), 7 random bytes, and a 16-byte MAC: a BLAKE3 keyed hash,
  under a per-process random key, of the first 16 bytes and the requester
  id. The responder keeps nothing per challenge, and the effort cannot be
  lowered. A seed is valid for 60 s and once: used seeds are kept until
  they expire, at most 4,096; past that the oldest is forgotten and every
  seed expiring no later is refused (the requester gets a new challenge),
  so a replay is never accepted.
- **Adaptive effort** (after Tor proposal 327). The effort starts at
  `limits.challenge_base_bits` (8). For each 10 s interval during which
  the node was under pressure (overloaded, or challenging a rate-limited
  peer), it rises by one bit, up to `limits.challenge_max_bits` (24); for
  each interval without pressure it falls by one bit, never below the
  base. Requests bringing more effort are not served first: kept simple.
- **What a solution buys.** A valid solution lets that one request
  through, past the per-peer limit and the overload, but within a global
  ceiling of `solved_per_min` (1,200) solved requests a minute, all peers
  together: solving puzzles cannot push a node past it.
- **Algorithm ids.** `alg` names the puzzle so that another one, e.g. a
  memory-hard puzzle, can be added later under a new id, advertised the
  same way. Equi-X (Tor's) is LGPL and was ruled out; BLAKE3 is already a
  dependency.

New keys: `limits.challenge` (true), `limits.challenge_base_bits` (8),
`limits.challenge_max_bits` (24). `overload_in_flight` and
`solved_per_min` are `Limits` fields without a key for now.

### Blob fetch queue and paced pushes (implementation note, 2026-10-21)

Code: `crates/wats/src/limits.rs` (`PeerSlots`), `Node::replicate`,
`Node::handle_request`. Protocol and events do not change.

A publisher pushes each post's blobs in a task of its own, so a burst of
posts (8 at once in `tests/blob_threads.rs`) sent as many `replicate`
requests to a peer at the same moment. The receiver fetched 4 and skipped
the others; it never fetches the blob of a post it got by gossip, so a
skipped push was a replica lost for good, unless the report (the hashes
the peer answered with, logged as `replication ... report=`) was read.

- **Receiver: a queue behind the limit.** A blob fetch over a peer's
  `blob_fetches_per_peer` (4) slots waits for one, first come first
  served, up to `BLOB_FETCH_WAIT` (60 s; with the 60 s a fetch from that
  peer may take, the answer still comes within the publisher's 150 s).
  At most `blob_fetch_queue` (16) fetches of one peer wait; past these,
  or after the wait, the blob is skipped and logged as before. Per peer
  and bounded both ways, so the limit still protects the node: a peer
  makes it run at most 4 fetches and keep at most 16 waiting requests,
  within its `replicate` rate limit and the overload challenge (waiting
  requests count as in flight). `blob_fetch_queue` is a `Limits` field
  without a key, like `overload_in_flight`. This covers bursts from 0.4.0
  and 0.4.1 publishers.
- **Publisher: no more pushes at once than receivers fetch.** A node
  runs at most `PUSHES_PER_PEER` (4, the default `blob_fetches_per_peer`)
  `replicate` requests at once to one peer; the receiver fetches the
  blobs of one request one after the other, so it never has more than 4
  fetches running for this publisher and skips none. Further pushes wait
  their turn (at most `PUSH_QUEUE`, 64, per peer, for up to `PUSH_WAIT`,
  10 min; past these a push is skipped with a WARN). This covers 0.4.0
  and 0.4.1 receivers, which skip instead of queueing.
- **Not done:** retrying a push the peer answered without some hashes.
  Paced pushes are not skipped for the limit any more; a refusal for
  another reason (storage full, quota, body dropped) would be refused
  again, and repair of lost replicas stays Phase 2 (ADR 0005; since
  0.4.3, ADR 0029).

## Consequences

- A new node catches up on a million events in about 7 minutes at the
  default `sync` rate.
- Peers are identified by their node id. One machine running many ids
  gets one budget per id: this bounds accidental floods, not a
  determined attacker. Sybil resistance comes with reputation and
  proof of work (Phase 2).
- Gossip is not covered here; Iroh bounds it separately.
