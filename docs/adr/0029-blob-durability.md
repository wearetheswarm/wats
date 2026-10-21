# 0029. Blob durability: replica repair, knowledge replicas, last copy

Status: Provisional (2026-10-21). Ships in 0.4.3, a robustness release
before wider distribution. Revised the same day after an independent
review of PR #183 (two blockers, nine should-fix points; "Decisions on
the review" below).

Amends [ADR 0005](0005-sync-and-replication.md) ("Replication k": there
is now repair). Builds on [ADR 0014](0014-rate-limits.md) (the paced
pushes of 0.4.2), [ADR 0020](0020-bodies-tombstones-erasure-eviction.md)
(drop reasons), [ADR 0019](0019-knowledge-curation-verification-reputation.md)
§5 (knowledge entries) and [ADR 0025](0025-bounded-history-sync.md) §7
(rotation).

Code: `crates/wats/src/node/durability.rs` (repair, eviction plan),
`crates/wats/src/node/proof.rs` (possession proofs, the blob request
guard), `Node::serve_replicate`, `Node::drop_body`,
`crates/wats/src/proto.rs` (`call_solving_within`), `crates/wats/src/store.rs`
(`event_blobs`, `evicted_replicas`). Tests: `crates/wats/tests/durability.rs`.

## Context

A blob is pushed to the k = 3 peers closest to its hash at publish time
(ADR 0005). Nothing happens after that:

- **No repair.** If a replica's node disappears, nobody re-replicates.
  ADR 0005 left it to Phase 2; ADR 0014's 0.4.2 note left it too.
- **No difference by value.** A blob that a knowledge entry rests on is
  kept like any other, although ADR 0019 §4 says "replication k is kept
  for confirmed posts and their blobs".
- **Eviction knows nothing of other copies.** A body dropped for storage
  pressure (`evicted`, rotation in ADR 0025 PR B) untags the blobs no kept
  event lists, and the next collection deletes them, even if this node
  held the last copy in the swarm.

Vincent's decisions, the baseline of this ADR:

- Decided by Vincent (2026-10-21): "A file is on 3 nodes; if one
  disappears we must keep that number of replicas — that's the
  baseline." → replica repair.
- Decided by Vincent (2026-10-21): "Yes keep the file if responsible." →
  a node keeps a blob it holds as a replica, even when the body listing
  it was dropped.
- Decided by Vincent (2026-10-21): "Yes 5 for knowledge" → blobs listed by
  knowledge entries get k = 5 replicas instead of 3. He asked for the
  storage ratio: see §3.
- Decided by Vincent (2026-10-21): "OK for last-copy protection" → a
  node never deletes a blob it believes may be the last copy without
  first pushing it to another node; erasure and retraction still delete.

## Decision

### 1. Who is a replica, and how replicas are counted

**Intent at fetch time is the source of truth.** A node is a replica of
blob H when H came to it by `replicate` (a publisher's push, a repair, a
hand-off) and it was then **among the 2k closest of its own view** of the
post's network. The receiver records it then, durably: a
`wats/replica/<hash>` tag in its blob store (`Node::record_replica_intent`).
Outside the 2k closest, a pushed blob is kept as a read cache, with no
record. Deleting the blob for any reason deletes the record with its
other tags.

Why not recompute "the k closest" later? The publisher chose the replicas
from **its own view** of the swarm: the peers it knew at that moment
(gossip neighbors, configured and bootstrap peers, peers that contacted
it, at most 10,000). The receiver's view differs, and keeps changing: it
knows other peers, peers come and go, and a node that knows few peers
sees itself among the "k closest" of almost everything, one that knows
many of almost nothing. A recomputed XOR set would make a replica forget
it is one, or a reader believe it is one. So:

- **The record decides.** The XOR estimate (the k closest of this node's
  known peers plus itself, `Node::is_responsible`) may only add: a node
  that holds a blob and finds itself among the k closest treats it as a
  replica too (that covers replicas received from 0.4.2 publishers before
  the record existed). It never removes a record. When unsure, a node
  keeps the blob.
- **Bounded.** The 2k rule keeps a peer from making any node a replica of
  anything. Replicas may take at most `REPLICA_SHARE_PERCENT` (60 %) of
  the blob part of `storage.max` (what `storage.events_share` leaves);
  past it, new replicas are refused for good ("replica share of
  storage.max reached"). Without `storage.max`, no cap, as for blobs
  today.
- **k** is `replication` (3), or 5 when the blob is knowledge (§3).
- **A node watches** a blob it holds whole (complete, of its declared
  size, readable) when it is a replica (record or estimate), or when a
  post of its own lists it (the publisher keeps an eye on its own files).
  A copy fetched on read is neither: a source for others, not watched.
- **Replicas are counted** from **proven** holders: the watcher counts
  itself when it is a replica, plus every peer that, during the check,
  answers that it holds the blob and then proves it (§4, "Possession
  proofs"). The blob is complete when they make k.
- **Where a missing replica goes** is the one place XOR distance still
  decides: the closest peers this node knows **in the post's network**
  (heard from on it in the last 24 hours, ADR 0026, or a gossip neighbor
  on it) that do not hold the blob, as the publisher chose. The receiver
  records intent the same way.
- **Honest limits.** This node does not know the whole swarm. Two
  watchers with different views may push to different peers: an extra
  replica, never fewer. A replica that comes back after it was replaced
  keeps its copy too: repair never deletes a replica; eviction releases
  one only as §4 says.

### 2. Replica repair

**How to check without a wire change.** The `replicate { event, hashes }`
request every release answers (ADR 0005) is both the probe and the push:

- a peer that holds the blob answers with its hash at once;
- a peer that does not fetches it from the requester (BLAKE3-verified),
  pins it, and answers with its hash;
- a peer that refuses says so, or answers without it.

Receiving side (no wire change):

- A blob **already held under a tag** is confirmed at once and pinned,
  before the fetch slots and the storage checks: a probe of a held blob
  costs the receiver an event check and two lookups, and a node at
  `storage.max` still confirms what it holds. The tag must exist before
  the request (see "Blob collection" below); a blob held but untagged
  takes the fetch path.
- A refusal for good is said as one when no blob was taken: `Refused`
  with "storage full", "blob quota reached" or "replica share of
  storage.max reached". 0.4.0–0.4.2 answer those with a bare
  `Replicated` without the hash.
- A `replicate` for a post whose body was evicted here confirms the
  blobs held under a tag without taking the body back (S6): the
  tombstone stays. (That `Store::insert` takes an evicted body back on
  any insert is changed in the rotation PR, #182.)
- A retraction or erasure that landed while the blobs came in (S8): the
  receiver checks the post again after pinning, and untags what the drop
  would have freed.

**One check of blob H** (the event that lists it comes from the store; the
one knowledge prefers when several do, else the oldest, S9):

1. Compute k. If this node is neither a replica of H (§1) nor its
   author, it is a cheap reject: rescheduled, not counted in the pass.
2. Ask peers to hold H, first the holders proven at the last check, then
   the members of the post's network in XOR order from H, nearest first,
   until k replicas are counted, or `need + 3` peers were asked (`need`
   is k, minus one when this node is a replica). Before each request, the
   post is checked again: a retraction or erasure ends the check.
3. **Moving on, or not** (S2). Each answer is one of:
   - **proven holder**: counted;
   - **refusal for good** (`Refused` for full, quota, replica share,
     body dropped there, not a member): this peer is skipped for this
     blob from now on, and the next peer is asked;
   - **bare decline** (`Replicated` without the hash, no reason: a
     0.4.2 peer full, over quota or out of fetch slots; or a failed
     possession proof, see §4), **unreachable**
     (no connection within 5 s), **busy** (no push slot within 60 s,
     rate limited, a challenge, a timeout), or this pass's **16 requests
     to that peer** spent: the check **ends here, "not checked"**, and
     comes back within 10 minutes. A peer that declined or was unreachable
     twice, the two misses at least an hour apart (`miss_gap`), is then
     replaced by the next peer. So a slow or busy peer never makes repair
     push a copy to a farther one; the probe of the review that left a
     copy outside the 3 closest is a test now.
4. An unreachable peer is not dialed again for 10 min, doubling to 24 h,
   until it contacts this node again (any request, any gossip message).
5. Complete: checked again after `recheck` (6 h, ±50 %). Short (every
   peer refused for good): after `retry` (10 min), doubling up to
   `recheck`, logged at INFO ("blob short of replicas"). A check that adds
   a proven holder to a blob that had holders before logs "replica
   repaired".

**Cadence and bounds** (constants, not config keys):

| What | Value |
|---|---|
| First pass after start | 3 min, ±50 % |
| Between passes | 10 min, ±50 % |
| Blobs checked per pass | at most 64, the most overdue first; cheap rejects not counted (at most 1,024 looked at) |
| Checks at once | 4 |
| Requests to one peer per pass | at most 16 (receivers allow 120/min, a burst of 30) |
| Requests per check | at most `need + 3` (2 + 3 for k = 3, 4 + 3 for k = 5) |
| Possession proofs | one per confirmation: two chunks, about 2 KiB |
| Pushes at once to one peer | 4, shared with publishing (ADR 0014, 0.4.2); a slot waited for at most 60 s |
| A complete blob | checked again after 6 h, ±50 % |
| A check ended early | again within 10 min |
| A short blob | after 10 min, doubling up to 6 h |
| A peer replaced | after two misses at least 1 h apart |
| The inventory | the tagged blobs that kept events list, from the `event_blobs` index and the tags, at most every hour, off the async runtime |

**Capacity, honestly.** A node runs about 144 passes a day. Each checks at
most 64 blobs, so at most 9,216 checks a day. The 16 requests per peer
per pass bound it further: a check sends about `need` requests (2 for
k = 3), each to one of the blob's co-replicas, so a pass completes at most
16 × P / 2 checks, where P is the number of distinct peers it asks. In a
small swarm where the same 2 peers hold every blob a node holds, that is
16 checks a pass, 2,304 a day: with a 6 h recheck, about 576 watched
blobs are rechecked on time, and more are rechecked less often. In a
large swarm (P ≥ 8) the 64-check limit binds: about 2,300 watched blobs
on time. A lost replica is noticed at the next check of the blob, then
replaced after its second miss an hour later: within about 7 h at the
defaults, later past these sizes. The numbers rise once measured.

**No storm.** Nothing is triggered by a peer: passes run on the node's
own jittered timer, never on `NeighborUp`, a join or a request. A blob
first seen (every blob after a restart, a replica just taken) is due at a
random time within `recheck`, so a node that comes back spreads its
checks over 6 h; the nodes it comes back to do nothing on its return. A
pass is bounded as above whatever the number of blobs due, and over its
budget a check ends instead of pushing elsewhere.

**What repair cannot do.** It pushes a blob with the event that lists it,
held in full here. A replica whose body this node evicted (§4) is still
served to readers and confirmed to repairers, but this node does not push
it: ADR 0020 §3, "a node never pushes a body it dropped". The other
holders, which still hold the body, repair it.

### 3. k = 5 for knowledge

**The rule.** A blob wants `knowledge_replication` = 5 replicas when an
event that lists it is, as this node computes knowledge entries (ADR
0019 §5.2, `knowledge::compute`, the same code as `search`):

- a knowledge entry itself: a promoted digest, or a thread's canonical
  answer; or
- a source an entry rests on: a digest entry's `refs`, a canonical
  answer's thread root (the ask).

When several kept events list the blob (at most 8 are read), it wants 5
if any of them is knowledge, and the check pushes it with that event
(S9). `Node::blob_target_k(hash)` computes the same.

**Accepted answers** count as knowledge only when they make their thread
an entry, that is, as canonical answers (accepted by the asker plus
backed by an independent key, or backed with evidence by two keys). An
accept alone is one key's call, and the rule stays the one `search` shows
as `knowledge: true`.

**How a node learns it.** It holds the events (every node holds the
events of its networks, ADR 0025), so each pass computes, off the async
runtime, the entries of the threads of the blobs it checks (at most 64
threads). A digest is not an entry when it is published, so publishing
still pushes to 3; repair raises it to 5 once the digest is promoted, at
the next check of a node that sees it promoted. If the entry later loses
its status, the blob wants 3 again; the 2 extra replicas stay.

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

**The rule, for evictions.** Before a node evicts a body for storage
pressure (`Node::drop_body(.., Evicted, ..)`, rotation), for each blob the
eviction would free (listed by no other kept event) that it holds whole:

- **A replica** (record) **stays**: the body goes, the blob is kept, and
  the post is recorded (`evicted_replicas`) so that a later retraction,
  erasure, label or `wats drop` of it releases the blob (B1). It is
  released by an eviction only once this node is no longer among the 2k
  closest of its view **and** k other holders are proven (S4).
- **Any other copy** (a read cache, an own post's file) needs **other
  holders proven: two when k > 1**, one otherwise. Peers are asked with
  `replicate`, the closest to the blob in the post's network first (those
  responsible), then `history_full` peers (durable nodes), then the
  rest, at most 6, through the paced push slots. If too few prove it, the
  blob is **stranded**: the whole eviction is refused
  (`KeptLastCopy`), body and blob stay, and eviction picks something else.
- **A late freed blob** (S7): if the store frees a blob the plan did not
  cover (another event listing it was dropped meanwhile), it is handed off
  then, with the event read before the drop, or kept and recorded like a
  replica.

**Possession proofs** (S1). A peer that says it holds a blob is not
believed on its word: one liar, or a Sybil placed close to a hash, would
otherwise make a node delete the last copy. After a `replicate` answer
with the hash, this node fetches from that peer alone, through the
`iroh_blobs::get` state machine (not the downloader), a random chunk and
the last chunk of the blob, into memory, each verified against the hash
(BAO): about 2 KiB, and the last chunk proves the size.

- While the proof runs, this node **refuses that peer's blob requests
  for that hash**: the blobs protocol runs with an event handler that
  intercepts `get` and `get_many` (`RequestMode::Intercept`) and notes
  which connection is which peer. The peer cannot answer by fetching the
  chunks from this node's own copy.
- It can still fetch them from a third holder: then a copy exists there,
  which is what a release needs, but it may be counted twice. And a proof
  is a snapshot: a peer may delete right after. Both stay open (see
  "Open questions").
- Repair counts only proven holders. A failed proof is logged at WARN
  and counts as a miss, like a bare decline: two holders that check the
  same blob at the same moment prove each other, and each refuses the
  other's requests meanwhile, so one failure proves no lie. Two failures
  an hour apart replace the peer. A hand-off never counts a failed proof,
  so a liar never makes this node release a copy.

**Where it applies:**

| Deletion | Last-copy protection |
|---|---|
| `evicted` (storage pressure: `Node::drop_body`, rotation) | yes, as above |
| the blob collection (`storage.gc`) | by construction: it deletes only blobs with no tag and no kept event listing them, and an eviction untags only what the plan released |
| `wats drop` (`own`), running node or not | **no: deletes at once**, replicas kept by an earlier eviction included |
| `retraction`, `erasure`, `label` | **no: they delete regardless, last copy or not** (ADR 0020 is unchanged) |

`wats drop` is a human's compliance tool (personal data, illegal content,
a leaked secret: `docs/security-and-compliance.md`): it deletes at once,
as before this ADR, and hands nothing to anyone (decided on the review,
B2). Retractions, erasure labels and moderation labels are reasons why
the content must not be stored or relayed; pushing it first would defeat
them. A held erasure label applied later upgrades an `evicted` record to
`erasure` and releases the kept replicas the same way.

**Belief.** "May be the last copy" is checked live at eviction time, not
from memory: a confirmation hours old proves little.

**Blob collection** (iroh-blobs). A collection runs in three steps: our
protect callback (the blobs kept events list, those being fetched), its
mark (tags and temporary tags), its sweep (deletes what is in neither and
was not written since the run began). A tag set after a run's mark is not
seen by that run's sweep, so the receiver's fast path confirms only a
blob that was **already tagged before the request**: one past its mark
read the tag then, a later one reads it now. A blob held but untagged
takes the fetch path, whose hold (`BlobHold`) and writes the collection
respects, as before.

### 5. Interaction with rotation (ADR 0025 PR B)

- **Replicas are not evicted by rotation.** `Node::drop_body(.., Evicted,
  ..)` keeps them and records them (§4). Rotation calls it, or
  `plan_eviction` before its own drop.
- **One record.** The rotation PR (#182) records replica intent at fetch
  time too, as a `blob_retained` row with reason `replica`. Whichever PR
  lands second makes `record_replica_intent` and `has_replica_intent`
  read and write that row (the `wats/replica/` tags of nodes already
  running are read once, or kept as a second source: either only adds
  retention), and folds `evicted_replicas` into it if it fits. The XOR
  estimate (`Node::is_responsible(hash, k)`, with `Node::blob_target_k`
  for k) may add retention, never remove it.
- **The hash → events index** (S10) is created here, for #182 to reuse:

  ```sql
  CREATE TABLE IF NOT EXISTS event_blobs (
      hash TEXT NOT NULL,
      seq  INTEGER NOT NULL REFERENCES events (seq),
      PRIMARY KEY (hash, seq)
  ) WITHOUT ROWID;
  CREATE INDEX IF NOT EXISTS event_blobs_seq ON event_blobs (seq);
  CREATE TABLE IF NOT EXISTS event_blobs_filled (upto INTEGER NOT NULL);
  ```

  One row per (blob hash, kept full event listing it, by `blobs[].hash`,
  URL or not); tombstones and opaque events have none. Filled on insert,
  emptied when a body is dropped or a row deleted, backfilled at open in
  transactions of 5,000 events from `event_blobs_filled.upto`. No schema
  bump: 0.4.2 still opens the store, and what it inserts meanwhile is
  filled at the next open. Every lookup by hash (`blob_referenced`,
  `blobs_freed_by`, `events_listing_blob`, `blob_listings`) uses it; the
  blob collection's `referenced_blobs` keeps reading the events
  themselves, hourly, as the one safety-critical reader.
- **Blob storage full.** As today (ADR 0012): at `storage.max` the node
  fetches nothing more, replicas included. It keeps the replicas it holds
  and confirms them when asked (§2), but cannot take new ones, and now
  says so ("storage full"). Repair on the other nodes takes that as a
  refusal for good and moves to the next closest: in effect, a full node
  drops out of the target set for new blobs, and the next closest takes
  its place. Its own repair still runs: pushing costs it no storage.

### 6. Wire

**No wire change.** No new request, field or feature: repair and the
hand-off use `replicate` as every release answers it, and proofs are
ordinary blob requests. Mixed versions:

- a 0.4.0–0.4.2 peer is a valid repair target and proves what it holds;
  it records nothing, and answers "full" or "over quota" with a bare
  decline, which repair takes as a miss (replaced after a second one an
  hour later);
- it never repairs, so its blobs rely on newer holders;
- an older publisher's pushes are answered as before.

`Control::Drop` (the local socket of `wats drop`) is unchanged from 0.4.2
(`--purge` of the first version of this PR is gone). It is
`deny_unknown_fields`: a CLI and a running node of different releases
must send the same fields, which they do.

## Consequences

- A blob keeps k replicas as long as one replica holder runs a release
  with repair, within its view of the swarm, within about 7 h of a loss
  at the defaults (§2, "Capacity").
- Knowledge blobs cost 2/3 × s more storage in all (§3).
- An eviction can be refused: a node with no reachable peer keeps what
  may be the last copy. At `storage.max` that means it keeps refusing new
  blobs rather than losing old ones.
- Repair adds a low, bounded background load: about 2 requests and 2
  proofs per watched blob every 6 h, at most 16 requests per peer per
  pass.
- Every incoming blob request now passes through an event handler (a
  map lookup), so that proofs can refuse the peer under proof.

### Test evidence

`crates/wats/tests/durability.rs`, loopback nodes only:

- `a_lost_replica_is_restored`: 6 nodes; the publisher and one of the 3
  replicas disappear; after two misses a second apart (`miss_gap` in
  tests), the 2 left push to the 4th closest within 60 s, which records
  itself as a replica; 3 copies, no more.
- `a_blob_of_knowledge_reaches_five_replicas`: a digest accepted by the
  asker reaches the 5 closest of 7 nodes; a plain blob stays at 3;
  `blob_target_k` says 5 and 3.
- `the_last_copy_is_handed_off_before_it_is_deleted`: a read copy's
  eviction is refused, and the collection keeps it, while no holder is
  reachable; once the replica's node is back and proves it holds the
  blob, the eviction goes through. Evicting the replica's body keeps the
  replica.
- `a_lying_peer_does_not_release_the_last_copy`: a peer that answers
  every `replicate` with its hashes, holding nothing, fails the proof;
  the eviction is refused.
- `an_evicted_replica_is_released_by_erasure_or_retraction` (B1): a
  replica kept through an eviction is released, record included, by a
  later erasure label, and another by a later retraction.
- `erasure_retraction_and_wats_drop_delete_at_once`: with a peer present,
  an erasure label, a retraction and a `wats drop` through the running
  node's socket delete the only copies, and the peer is never asked.
- `a_node_that_rejoins_starts_no_repair_storm`: 60 blobs on 6 nodes
  (about 36 per node, more than the 16 requests per peer per pass); a
  node coming back sends fewer than 60 requests in its first 3 s; with
  every blob due at once, each pass checks at most `blobs_per_pass`,
  sends at most 16 requests to any peer, and every blob still has
  exactly 3 copies (no overshoot).
- `an_older_peer_takes_a_lost_replica`: a peer answering `replicate` as
  0.4.2 does takes a lost replica, records nothing, proves it later; it
  runs no pass.
- Store: `event_blobs_follow_inserts_drops_and_older_releases`,
  `a_replica_kept_through_eviction_is_freed_by_a_later_drop`. Proofs:
  chunk choice and coverage.

## Decisions on the review (2026-10-21)

The coordinator's decisions on the review of PR #183, folded in above:

- **B1.** A replica kept through an eviction is recorded with its post;
  a later retraction, erasure, label or `wats drop` releases it unless a
  kept event lists it; a held erasure upgrades an `evicted` record.
- **B2.** `wats drop` deletes at once, as before this PR: no hand-off, no
  `--purge`. Eviction keeps its hand-off.
- **S1.** Possession proofs before any release and for repair's counts,
  two proven holders when k > 1, the peer's requests for the blob refused
  meanwhile; the collection's mark/sweep and the fast path as in §4.
- **S2.** Only refusals for good move repair on; busy, throttled, slow,
  over budget end the check.
- **S3.** The inventory is the tagged blobs; cheap rejects do not count.
- **S4.** Replica intent within 2k, a replica share of 60 %, release by
  eviction once outside 2k with k proven holders.
- **S5.** With B2, `wats drop` no longer waits on peers.
- **S6.** A `replicate` for an evicted post confirms without restoring
  the body (the `Store::insert` side is #182's).
- **S7, S8, S9, S10** as written in §2, §3, §4, §5. Repair targets are
  members of the post's network; a holder is replaced only after two
  misses an hour apart.

## Open questions

- **Proofs are snapshots, and may be relayed** from a third holder (§4).
  Repeated proofs at each check limit the first; the second would need
  the peer to sign what it serves, a wire change.
- **Excess replicas** are never removed but by eviction outside 2k.
  Measuring them first.
- **Pass size, recheck and `miss_gap`** are guesses, to be measured on
  the alpha swarm (blobs per node, requests per minute).
