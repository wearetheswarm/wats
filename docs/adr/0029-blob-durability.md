# 0029. Blob durability: replica repair, knowledge replicas, last copy

Status: Provisional (2026-10-21). Ships in 0.4.3, a robustness release
before wider distribution.

Amends [ADR 0005](0005-sync-and-replication.md) ("Replication k": there
is now repair). Builds on [ADR 0014](0014-rate-limits.md) (the paced
pushes of 0.4.2), [ADR 0020](0020-bodies-tombstones-erasure-eviction.md)
(drop reasons), [ADR 0019](0019-knowledge-curation-verification-reputation.md)
§5 (knowledge entries) and [ADR 0025](0025-bounded-history-sync.md) §7
(rotation).

Code: `crates/wats/src/node/durability.rs`, `Node::handle_request`
(`replicate`), `Node::drop_body`, `drop_body_offline`,
`crates/wats/src/attach.rs` (`Control::Drop`), `crates/wats/src/proto.rs`
(`call_solving_within`), `Store::blob_listings`, `Store::blobs_freed_by`.
Tests: `crates/wats/tests/durability.rs`.

## Context

A blob is pushed to the k = 3 peers closest to its hash at publish time
(ADR 0005). Nothing happens after that:

- **No repair.** If a replica's node disappears, nobody re-replicates.
  ADR 0005 left it to Phase 2; ADR 0014's 0.4.2 note left it too.
- **No difference by value.** A blob that a knowledge entry rests on is
  kept like any other, although ADR 0019 §4 says "replication k is kept
  for confirmed posts and their blobs".
- **Deletion knows nothing of other copies.** A body dropped for storage
  pressure (`evicted`, rotation in ADR 0025 PR B) or by `wats drop`
  untags the blobs no kept event lists, and the next collection deletes
  them, even if this node held the last copy in the swarm.

Vincent's decisions, the baseline of this ADR:

- Decided by Vincent (2026-10-21): "A file is on 3 nodes; if one
  disappears we must keep that number of replicas — that's the
  baseline." → replica repair.
- Decided by Vincent (2026-10-21): "Yes keep the file if responsible." →
  a node keeps a blob it is responsible for (one of the k closest by
  XOR, ADR 0005), even when the body listing it was dropped.
- Decided by Vincent (2026-10-21): "Yes 5 for knowledge" → blobs listed by
  knowledge entries get k = 5 replicas instead of 3. He asked for the
  storage ratio: see §3.
- Decided by Vincent (2026-10-21): "OK for last-copy protection" → a
  node never deletes a blob it believes may be the last copy without
  first pushing it to another node; erasure and retraction still delete.

## Decision

### 1. Who is responsible, and how replicas are counted

- **The responsible set** R(H) of blob H is the k closest nodes to H by
  XOR distance (ADR 0005) **among the peers this node knows, plus
  itself**. Known peers are those in `Node::known_peers`: gossip
  neighbors, configured and bootstrap peers, peers that contacted this
  node, at most 10,000. Peers found unreachable lately are left out (§2),
  so a node that disappeared is replaced in R(H) by the next closest.
- **k** is `replication` (3), or 5 when the blob is knowledge (§3).
- **A node watches** a blob it holds whole (complete, of its declared
  size, readable) when it is in R(H), or when a post of its own lists it
  (the publisher keeps an eye on its own files whether or not it is one
  of the k closest).
- **A replica counts** when a node of the walk (below) answers that it
  holds the blob. The watcher counts itself when it is in R(H). So a
  blob is complete when the watcher (if responsible) plus the confirming
  peers make k.
- **Honest limits.** This node does not know the whole swarm. Two nodes
  with different views compute different sets: each pushes to its own,
  which costs an extra replica, never fewer. A peer that comes back after
  its replicas were re-made keeps its copy too: repair never deletes a
  replica, so the excess is at most one copy per node that came back.
  Holders outside R(H) (readers that fetched a blob, a publisher not
  among the closest) are sources, not counted replicas.

### 2. Replica repair

**How to check without a wire change.** The `replicate { event, hashes }`
request every release answers (ADR 0005) is both the probe and the push:

- a peer that holds the blob answers with its hash at once;
- a peer that does not fetches it from the requester (BLAKE3-verified),
  pins it, and answers with its hash;
- a peer that is full, over its quota, dropped the body or refuses,
  answers without it.

One change on the receiving side (no wire change): a blob already held
whole is confirmed at once and pinned, **before** the fetch slots and the
storage checks. A probe of a held blob then costs the receiver an event
check and a status lookup, and a node at `storage.max` still confirms
what it holds. A 0.4.0–0.4.2 receiver answers the old way: it confirms a
held blob through its fetch path (cheap, the downloader finds it local),
except at `storage.max`, where it answers without it and repair moves on
to the next peer (an extra replica at worst).

**One check of blob H** (the event that lists it comes from the store):

1. Compute R(H) and k. If this node is neither in R(H) nor the author,
   reschedule (it may become responsible later) and stop.
2. Walk the known peers in XOR order from H, nearest first, asking each
   to hold H, until k replicas are counted, or `need + 3` peers were asked
   (`need` is k, minus one when this node is in R(H)): room to replace 3
   gone, full or refusing peers per check.
3. A peer with no connection within 5 s (`REPAIR_CONNECT_LIMIT`) is
   unreachable: left out of R(H) for 10 min, doubling to 24 h, until it
   contacts this node again (any request, any gossip message).
4. Complete: checked again after `recheck` (6 h, ±50 %). Short: after
   `retry` (10 min), doubling with each short check, up to `recheck`;
   logged at INFO ("blob short of replicas"). A check that adds a peer to
   a blob that had holders before logs "replica repaired".

**Cadence and bounds** (constants, not config keys):

| What | Value |
|---|---|
| First pass after start | 3 min, ±50 % |
| Between passes | 10 min, ±50 % |
| Blobs checked per pass | at most 64, the most overdue first |
| Checks at once | 4 |
| Requests to one peer per pass | at most 16 (receivers allow 120/min, a burst of 30) |
| Requests per check | at most `need + 3` (2 + 3 for k = 3, 4 + 3 for k = 5) |
| Pushes at once to one peer | 4, shared with publishing (ADR 0014, 0.4.2); a slot waited for at most 60 s |
| A complete blob | checked again after 6 h, ±50 % |
| A short blob | after 10 min, doubling up to 6 h |
| The list of held blobs | read from the store at most every hour (one scan, as the blob collection does) |

So a node sends at most 64 × 7 = 448 requests per pass in the worst case,
at most 16 to any peer, and typically 2 per blob checked. It checks at
most 9,216 blobs a day: a node responsible for more than about 2,300
blobs rechecks each less often than every 6 h, and a lost replica is
noticed later. The pass size can rise once measured.

**No storm.** Nothing is triggered by a peer: passes run on the node's
own jittered timer, never on `NeighborUp`, a join or a request. A blob
first seen (every blob after a restart, a replica just taken) is due at a
random time within `recheck`, so a node that comes back spreads its
checks over 6 h; the nodes it comes back to do nothing on its return. A
pass is bounded as above whatever the number of blobs due.

**What repair cannot do.** It pushes a blob with the event that lists it,
held in full here. A blob whose body this node dropped (kept because it
is responsible, §5) is still served to readers, but this node does not
push it: ADR 0020 §3, "a node never pushes a body it dropped". The other
holders, which still hold the body, repair it.

### 3. k = 5 for knowledge

**The rule.** A blob wants `knowledge_replication` = 5 replicas when an
event that lists it is, as this node computes knowledge entries (ADR
0019 §5.2, `knowledge::compute`, the same code as `search`):

- a knowledge entry itself: a promoted digest, or a thread's canonical
  answer; or
- a source an entry rests on: a digest entry's `refs`, a canonical
  answer's thread root (the ask).

**Accepted answers** count as knowledge only when they make their thread
an entry, that is, as canonical answers (accepted by the asker plus
backed by an independent key, or backed with evidence by two keys). An
accept alone is one key's call, and the rule stays the one `search` shows
as `knowledge: true`.

**How a node learns it.** It holds the events (every node holds the
events of its networks, ADR 0025), so each pass computes the entries of
the threads of the blobs it checks (at most 64 threads). A digest is not
an entry when it is published, so publishing still pushes to 3; repair
raises it to 5 once the digest is promoted, at the next check of a node
that sees it promoted. If the entry later loses its status, the blob
wants 3 again; the 2 extra replicas stay (repair never deletes a
replica).

**The storage ratio** (Vincent: "ratio ?"). Let B be the bytes of unique
blobs in the swarm and s the share of those bytes that knowledge lists.

- Per knowledge blob: 5 replicas instead of 3, a factor of 5/3 ≈ 1.67
  (+67 %).
- In all: 3·B·(1 − s) + 5·B·s = 3·B·(1 + 2s/3). The overhead is **2/3 ×
  s** of the baseline.
- Example: B = 10 GiB of unique files, s = 15 % of the bytes behind
  knowledge entries. Baseline 3 × 10 = 30 GiB across the swarm; with k = 5
  for knowledge, 30 × (1 + 0.10) = 33 GiB: +3 GiB, +10 %. Spread over 100
  nodes: about 330 MiB of replicas each instead of 300 MiB.
- s = 100 % (everything is knowledge) is the bound: +67 %.
- Not counted: the publisher's own copy when it is not among the k
  closest (one more copy per blob, as today), and the excess copies of
  §1.

### 4. Last-copy protection

**The rule.** Before a node deletes a blob for a reason that is not a
reason to erase the content, at least one other node must confirm it
holds the blob, or take it first (`Node::hand_off_before_drop`):

- for each blob the drop would free (listed by no other kept event,
  `Store::blobs_freed_by`) and that this node holds whole,
- ask peers to hold it, with `replicate`: the closest to the blob first
  (those responsible for it), then `history_full` peers (durable nodes:
  the router, bootstrap nodes), then the others, at most 6, through the
  paced push slots;
- the first that confirms ends it; if none does, the blob is **stranded**.

**Where it applies:**

| Deletion | Last-copy protection |
|---|---|
| `evicted` (storage pressure: `Node::drop_body`, rotation) | yes: a stranded blob refuses the whole drop (`KeptLastCopy`), body and blob stay; eviction picks something else |
| `wats drop` with a running node (`own`) | yes: refused if a blob is stranded, nothing dropped; `--purge` skips the hand-off |
| `wats drop` with no node running | nothing can be handed off: refused when the drop would free a blob held here, unless `--purge` |
| the blob collection (`storage.gc`) | by construction: it deletes only blobs with no tag and no kept event listing them, and every untagging for the reasons above goes through the hand-off |
| `retraction`, `erasure`, `label` | **no: they delete regardless, last copy or not** (ADR 0020 is unchanged) |

Retractions, erasure labels and moderation labels are reasons why the
content must not be stored or relayed. Pushing it to another node first
would defeat them. `--purge` is the same for a human: for illegal content,
personal data or a leaked secret, the files are deleted without being
handed to anyone. Without `--purge`, `wats drop` hands the files to
another node before deleting them here; its help says so.

**Belief.** "May be the last copy" is checked live at deletion time, not
from memory: a confirmation hours old proves little. It costs one request
per freed blob when a responsible peer still holds it.

### 5. Interaction with rotation (ADR 0025 PR B)

- **Responsible blobs are not evicted by rotation.** Rotation drops a
  body to free store bytes; a blob listed by that body, which this node is
  responsible for, keeps its tag (Vincent's "keep the file if
  responsible"). The shared helpers: `Node::is_responsible(hash, k)`,
  `Node::blob_target_k(hash)` (5 when a kept event listing the blob is
  knowledge, else `replication`) and `Node::responsible_blobs(event)`.
  The rotation PR's `Node::blob_responsible(hash)` becomes
  `is_responsible(hash, blob_target_k(hash))` when the two meet: one
  definition of "responsible" (k closest of the known peers and this
  node, unreachable peers left out).
- **Other blobs a rotation drop frees** go through last-copy protection:
  rotation calls `Node::drop_body(.., Evicted, ..)`, or
  `Node::hand_off_before_drop` before its own drop, and skips the post
  when a blob is stranded. (Rotation is a separate PR: this is what it
  must do to keep the rule.)
- **Blob storage full.** As today (ADR 0012): at `storage.max` the node
  fetches nothing more, replicas included. It keeps the replicas it holds
  and confirms them when asked (§2), but cannot take new ones. Repair on
  the other nodes reads its refusal as "declined" and moves to the next
  closest: in effect, a full node drops out of R(H) for new blobs, and the
  next closest takes its place. Its own repair still runs: pushing costs
  it no storage.

### 6. Wire

**No wire change.** No new request, field or feature: repair and the
hand-off use `replicate` as every release answers it. Mixed versions:

- a 0.4.0–0.4.2 peer is a valid repair target and confirms what it holds
  (through its fetch path), except at `storage.max` (an extra replica);
- it never repairs, so its blobs rely on newer holders;
- an older publisher's pushes are answered as before.

The control socket's `drop` gains an optional `purge`, sent only when
true: an older running node still takes `wats drop` without `--purge`.

## Consequences

- A blob keeps k replicas as long as one responsible holder runs a
  release with repair, within its view of the swarm, and within about
  6 h (± jitter, more for nodes responsible for over ~2,300 blobs) of a
  loss.
- Knowledge blobs cost 2/3 × s more storage in all (§3).
- Eviction and `wats drop` can be refused: a node with no reachable peer
  keeps what may be the last copy. At `storage.max` that means it keeps
  refusing new blobs rather than losing old ones.
- Repair adds a low, bounded background load: about 2 requests per blob
  every 6 h, at most 16 per peer per pass.

### Test evidence

`crates/wats/tests/durability.rs`, loopback nodes only:

- `a_lost_replica_is_restored`: 6 nodes; the publisher and one of the 3
  replicas disappear; the 2 left push to the 4th closest within 60 s
  (looped 20+ times, some under CPU load, before merging).
- `a_blob_of_knowledge_reaches_five_replicas`: a digest accepted by the
  asker reaches the 5 closest of 7 nodes; a plain blob stays at 3.
- `the_last_copy_is_handed_off_before_it_is_deleted`: eviction refused
  and the collection keeps the blob while no other node is reachable;
  once one appears, it takes the blob, the eviction goes through and the
  blob is untagged here; an offline `wats drop` is refused, then done
  with `--purge`.
- `erasure_and_retraction_delete_the_last_copy`: both delete the only
  copy, and nobody is asked to take it.
- `a_node_that_rejoins_starts_no_repair_storm`: 40 blobs; a node coming
  back sends fewer than 40 requests in its first 3 s; with every blob due
  at once, one pass checks at most `blobs_per_pass` and sends at most 16
  requests to any peer.
- `an_older_peer_takes_a_lost_replica`: a peer answering `replicate` as
  0.4.2 does takes a lost replica and confirms it later; it runs no pass.

## Open questions

- **Proof of holding.** A peer that says it holds a blob is believed. A
  liar (or a Sybil placed close to a hash) makes repair stop early and a
  hand-off delete a real copy. A random-chunk probe over the blobs
  protocol would check it without a wire change; deferred.
- **Excess replicas** are never removed. Measuring them first.
- **An evicted body comes back by sync.** The store accepts an `evicted`
  body again on any insert, a sync page included, not only on `read`
  (ADR 0020 decision 4), and its blob is then kept again. Seen while
  testing the hand-off; rotation (ADR 0025 PR B) decides whether that
  stays.
- **Pass size and recheck** are guesses, to be measured on the alpha
  swarm (blobs per node, requests per minute).
