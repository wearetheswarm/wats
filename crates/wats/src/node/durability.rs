//! Blob durability (ADR 0029): replica repair, more replicas for
//! knowledge, and last-copy protection.
//!
//! - **Replica repair.** Every node walks, at a low and jittered rate, the
//!   blobs it holds as a replica (or that its own posts list). For each one
//!   it asks the holders it last confirmed, then the peers of the post's
//!   network closest to the hash, to hold it, with the `replicate` request
//!   every release answers: a peer that holds the blob confirms at once,
//!   one that does not fetches it from this node and records itself as a
//!   replica. A confirmation counts only once the peer proved it holds the
//!   blob ([`super::proof`]). It stops once the blob has its k replicas,
//!   itself included. Only a definitive refusal (full, quota, body dropped
//!   there), or a peer missing twice an hour apart (unreachable, a bare
//!   decline, a failed proof), makes it move on to a farther peer; a busy, throttled or slow peer ends the
//!   check, which comes back soon. No wire change.
//! - **Knowledge replicas.** A blob listed by a knowledge entry (ADR 0019
//!   §5.2: a promoted digest, a thread's canonical answer), or by one of
//!   the sources an entry rests on, wants [`KNOWLEDGE_REPLICATION`] (5)
//!   replicas instead of `replication` (3), as this node computes entries.
//! - **Last copy.** Before a node evicts a blob for storage pressure,
//!   other nodes must prove they hold it (two of them when k > 1), or take
//!   it first. A replica stays when its body is evicted, until this node is
//!   no longer among the 2k closest and k other holders are proven.
//!   `wats drop`, retractions, erasure labels and moderation labels delete
//!   at once (ADR 0020): those are reasons why the content must not be
//!   kept, or a human's compliance decision.
//!
//! A node is a replica of a blob when the blob came to it by `replicate`
//! and it was then among the 2k closest of its own view: the receiver
//! records that intent at fetch time (a `wats/replica/` tag), because the
//! pusher chose it from its own view of the swarm, which no other node can
//! recompute. The XOR estimate (the k closest among the peers this node
//! knows, plus itself) may only add to it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::EndpointId;
use iroh_blobs::Hash;
use iroh_blobs::api::proto::BlobStatus;
use n0_future::StreamExt;
use serde::Serialize;
use tracing::{debug, info, warn};

use super::{Node, PUSH_TIMEOUT, REPAIR_CONNECT_LIMIT};
use crate::event::{BlobRef, Event};
use crate::proto::{self, Request, Response};
use crate::supervise::supervise;

/// Replicas a blob of knowledge wants (ADR 0029, decided by Vincent on
/// 2026-10-21), instead of `replication`.
pub const KNOWLEDGE_REPLICATION: usize = 5;

/// Share of the blob part of `storage.max` (what the events share leaves)
/// that replicas may take, in percent; past it, new replicas are refused
/// (ADR 0029 §1).
pub const REPLICA_SHARE_PERCENT: u64 = 60;

/// Peers asked for one blob in one check beyond the replicas it lacks:
/// room to replace that many peers that refused for good.
const EXTRA_TRIES: usize = 3;
/// Requests one pass sends to one peer, all blobs together: far under a
/// receiver's `limits.replicate_per_min` (120, a burst of 30).
const PER_PEER_PER_PASS: usize = 16;
/// Blobs checked at once in a pass.
const CONCURRENT_CHECKS: usize = 4;
/// Blobs looked at in one pass, cheap rejects included (blobs this node
/// holds but does not watch).
const MAX_EXAMINED: usize = 1024;
/// Kept events read per blob for its k (the knowledge rule).
const LISTINGS_PER_BLOB: usize = 8;
/// Peers asked to take a blob before it is evicted (last copy).
const HANDOFF_TRIES: usize = 6;
/// The longest a repair or a hand-off waits for one of the paced push
/// slots to a peer ([`super::PUSHES_PER_PEER`]); past it the peer counts
/// as busy for this check.
const SLOT_WAIT: Duration = Duration::from_secs(60);
/// A peer found unreachable is not dialed again for this long, doubling
/// with each further failure up to [`UNREACHABLE_MAX`], or until it
/// contacts this node again.
const UNREACHABLE_FIRST: Duration = Duration::from_secs(600);
const UNREACHABLE_MAX: Duration = Duration::from_secs(24 * 3600);
/// A check that could not finish (a busy peer, a first miss) comes back
/// after at most this.
const NOT_CHECKED_RETRY: Duration = Duration::from_secs(600);
/// Two misses count as "missing twice" only within this many `miss_gap`s
/// of each other; past it, a miss starts a new sequence.
const MISS_WINDOW_GAPS: u32 = 6;
/// A refusal for good (full, quota, replica share) is remembered this
/// long: space may be freed.
const REFUSED_FOR: Duration = Duration::from_secs(6 * 3600);
/// Rows of `evicted_replicas` read in one pass (N4), the most overdue
/// first: [`Tuning::evicted_per_pass`].
const EVICTED_PER_PASS: usize = 256;

/// How replica repair runs ([`NodeConfig::durability`]); tests shorten it.
/// Not config keys: constants (ADR 0029).
///
/// [`NodeConfig::durability`]: super::NodeConfig::durability
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Run replica repair. Test only: `false` behaves as 0.4.2 (no
    /// repair).
    pub repair: bool,
    /// The first pass, after the node starts (then jittered, ±50 %).
    pub first_pass: Duration,
    /// Between two passes (jittered, ±50 %).
    pub every: Duration,
    /// A blob found with all its replicas is checked again after this
    /// (jittered); a blob first seen, at a random time within it.
    pub recheck: Duration,
    /// A blob found short of replicas is checked again after this,
    /// doubling with each check that leaves it short, up to `recheck`.
    pub retry: Duration,
    /// Most blobs checked in one pass (cheap rejects not counted).
    pub blobs_per_pass: usize,
    /// The list of the blobs this node holds is read again this often.
    pub inventory_every: Duration,
    /// Replicas of a blob of knowledge: [`KNOWLEDGE_REPLICATION`].
    pub knowledge_replication: usize,
    /// A peer is replaced only after two misses (unreachable, or a bare
    /// decline) at least this far apart.
    pub miss_gap: Duration,
    /// Rows of `evicted_replicas` read per pass: [`EVICTED_PER_PASS`];
    /// tests lower it.
    pub evicted_per_pass: usize,
    /// Test only: answer `replicate` as 0.4.2 does (a blob already held
    /// goes through the fetch slots and the storage checks; nothing
    /// recorded).
    pub legacy_replicate: bool,
    /// Test only: answer every `replicate` with its hashes, holding
    /// nothing (a lying peer).
    pub lie_replicate: bool,
    /// The most time the blobs of one `replicate` request may take here,
    /// fetch slot waits included: [`super::REPLICATE_BUDGET`]; tests
    /// shorten it.
    pub replicate_budget: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            repair: true,
            first_pass: Duration::from_secs(180),
            every: Duration::from_secs(600),
            recheck: Duration::from_secs(6 * 3600),
            retry: Duration::from_secs(600),
            blobs_per_pass: 64,
            inventory_every: Duration::from_secs(3600),
            knowledge_replication: KNOWLEDGE_REPLICATION,
            miss_gap: Duration::from_secs(3600),
            evicted_per_pass: EVICTED_PER_PASS,
            legacy_replicate: false,
            lie_replicate: false,
            replicate_budget: super::REPLICATE_BUDGET,
        }
    }
}

/// What repair has done since the node started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// Passes run.
    pub passes: u64,
    /// Blob checks made (blobs this node watches).
    pub checked: u64,
    /// Checks ended early (a busy or throttled peer, a first miss).
    pub not_checked: u64,
    /// `replicate` requests sent by repair and hand-offs.
    pub asked: u64,
    /// Holders proven (a confirmation and its possession proof).
    pub confirmed: u64,
    /// Confirmations whose possession proof failed.
    pub proofs_failed: u64,
    /// Replicas proven by a peer that had not been proven for this blob
    /// at the previous check: a replica restored (or one first seen).
    pub added: u64,
    /// Checks that left a blob short of its replicas.
    pub short: u64,
    /// Blobs released by an eviction once other holders were proven.
    pub handed_off: u64,
    /// Evictions refused: this node may hold the last copy.
    pub kept: u64,
    /// `replicate` requests this node served (from any peer).
    pub served: u64,
}

#[derive(Debug, Default)]
struct Counters {
    passes: AtomicU64,
    checked: AtomicU64,
    not_checked: AtomicU64,
    asked: AtomicU64,
    confirmed: AtomicU64,
    proofs_failed: AtomicU64,
    added: AtomicU64,
    short: AtomicU64,
    handed_off: AtomicU64,
    kept: AtomicU64,
    served: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Two misses at least `gap` apart.
#[derive(Debug, Clone, Copy)]
struct Misses {
    first: Instant,
    last: Instant,
}

impl Misses {
    fn new() -> Self {
        let now = Instant::now();
        Misses {
            first: now,
            last: now,
        }
    }

    /// One more miss. Misses further apart than [`MISS_WINDOW_GAPS`]
    /// gaps start a new sequence: two transient failures days apart are
    /// not "missing twice".
    fn again(&mut self, gap: Duration) {
        let now = Instant::now();
        if now.saturating_duration_since(self.last) > gap.saturating_mul(MISS_WINDOW_GAPS) {
            self.first = now;
        }
        self.last = now;
    }

    fn for_good(&self, gap: Duration) -> bool {
        self.last.saturating_duration_since(self.first) >= gap
    }
}

/// When a blob is checked next, and what the last check found.
#[derive(Debug, Clone)]
struct Check {
    due: Instant,
    /// Checks in a row that left it short.
    short: u32,
    /// The peers proven holders at the last check.
    holders: Vec<EndpointId>,
    /// Peers that refused it for good (full, quota, replica share), and
    /// when: forgotten after [`REFUSED_FOR`].
    refused: HashMap<EndpointId, Instant>,
    /// Peers that answered without it and no reason (0.4.2 when full),
    /// or failed their proof.
    declined: HashMap<EndpointId, Misses>,
}

impl Check {
    fn new(due: Instant) -> Self {
        Check {
            due,
            short: 0,
            holders: Vec::new(),
            refused: HashMap::new(),
            declined: HashMap::new(),
        }
    }
}

/// An unreachable peer: its misses, and when to dial it again.
#[derive(Debug, Clone, Copy)]
struct Unreachable {
    misses: Misses,
    failures: u32,
    until: Instant,
}

/// The blobs held under a tag that kept full events list, with the ids of
/// at most [`LISTINGS_PER_BLOB`] of those events each.
type Inventory = Arc<Vec<(Hash, Vec<String>)>>;

/// Repair's state ([`Node::durability`]): in memory only, rebuilt after a
/// restart (every blob is then due at a random time within `recheck`).
#[derive(Debug, Default)]
pub struct State {
    checks: Mutex<HashMap<Hash, Check>>,
    inventory: Mutex<Option<(Instant, Inventory)>>,
    unreachable: Mutex<HashMap<EndpointId, Unreachable>>,
    /// Bytes of the blobs held as replicas: counted at start and at each
    /// inventory, kept up to date on every tag and untag (N9).
    replica_bytes: AtomicU64,
    stats: Counters,
}

impl State {
    /// A `replicate` request served, for [`Stats::served`].
    pub(super) fn note_served(&self) {
        bump(&self.stats.served);
    }

    /// `peer` contacted this node: it is reachable again.
    pub(super) fn peer_seen(&self, peer: EndpointId) {
        if let Ok(mut unreachable) = self.unreachable.lock()
            && !unreachable.is_empty()
        {
            unreachable.remove(&peer);
        }
    }

    fn mark_unreachable(&self, peer: EndpointId, gap: Duration) {
        if let Ok(mut unreachable) = self.unreachable.lock() {
            let now = Instant::now();
            if unreachable.len() >= 10_000 {
                unreachable.retain(|_, u| u.until > now);
            }
            let entry = unreachable.entry(peer).or_insert(Unreachable {
                misses: Misses::new(),
                failures: 0,
                until: now,
            });
            entry.misses.again(gap);
            entry.failures = entry.failures.saturating_add(1);
            let doublings = entry.failures.saturating_sub(1).min(16);
            let wait = UNREACHABLE_FIRST
                .saturating_mul(1 << doublings)
                .min(UNREACHABLE_MAX);
            entry.until = now.checked_add(wait).unwrap_or(now);
        }
    }

    /// Not to be dialed now (it failed lately); a miss is noted.
    fn skip_dial(&self, peer: &EndpointId, gap: Duration) -> bool {
        self.unreachable.lock().is_ok_and(|mut u| {
            u.get_mut(peer).is_some_and(|entry| {
                let skip = Instant::now() < entry.until;
                if skip {
                    entry.misses.again(gap);
                }
                skip
            })
        })
    }

    /// Missed twice at least `gap` apart: replaced by the next peer.
    fn gone(&self, peer: &EndpointId, gap: Duration) -> bool {
        self.unreachable
            .lock()
            .is_ok_and(|u| u.get(peer).is_some_and(|e| e.misses.for_good(gap)))
    }

    fn stats(&self) -> Stats {
        let s = &self.stats;
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Stats {
            passes: get(&s.passes),
            checked: get(&s.checked),
            not_checked: get(&s.not_checked),
            asked: get(&s.asked),
            confirmed: get(&s.confirmed),
            proofs_failed: get(&s.proofs_failed),
            added: get(&s.added),
            short: get(&s.short),
            handed_off: get(&s.handed_off),
            kept: get(&s.kept),
            served: get(&s.served),
        }
    }
}

/// What a receiver's refusal for good says (`Node::serve_replicate`).
pub const NOT_TAKING: &str = "not taking replicas";

/// The tag that records a blob held as a replica, followed by its hash.
pub const REPLICA_TAG: &str = "wats/replica/";
/// The tag that keeps a blob (an own post's, a fetch's, a replica's).
pub const BLOB_TAG: &str = "wats/blob/";

fn replica_tag(hash: &Hash) -> String {
    format!("{REPLICA_TAG}{}", hash.to_hex())
}

/// The `k` nodes closest to `hash` by XOR distance among `peers` and `me`
/// (ADR 0005, ADR 0029).
pub fn responsible_set(
    hash: &[u8; 32],
    me: EndpointId,
    peers: &[EndpointId],
    k: usize,
) -> Vec<EndpointId> {
    let mut all = peers.to_vec();
    all.push(me);
    proto::closest(hash, &all, k)
}

/// A random duration in `[d/2, 3d/2]`.
fn jittered(d: Duration) -> Duration {
    let half = d.checked_div(2).unwrap_or_default();
    half.saturating_add(within(d))
}

/// A random duration in `[0, d]`.
fn within(d: Duration) -> Duration {
    let millis = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    if millis == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(
        getrandom::u64()
            .unwrap_or(0)
            .checked_rem(millis.saturating_add(1))
            .unwrap_or(0),
    )
}

/// What a peer answered when asked to hold a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// It holds the blob (had it, or fetched it from us), and proved it.
    Holds,
    /// A refusal for good: full, quota, replica share, body dropped
    /// there, not a member of the network.
    Refused,
    /// It answered without the blob and gave no reason (0.4.2 when full,
    /// over its quota, or out of fetch slots), or failed its proof.
    Declined,
    /// Busy: no push slot, rate limited, a challenge, a timeout.
    Busy,
    /// No connection to it.
    Unreachable,
}

/// How one check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Not watched here (a cheap reject): not counted in the pass.
    Skipped,
    /// Checked to the end, complete or short.
    Checked,
    /// Ended early: checked again soon.
    NotChecked,
}

/// An eviction refused because this node may hold the last copy of some
/// of the post's blobs and no other node proved it holds them (ADR 0029).
#[derive(Debug)]
pub struct KeptLastCopy {
    pub id: String,
    pub blobs: Vec<String>,
}

impl std::fmt::Display for KeptLastCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "kept {}: this node may hold the last copy of {} of its files, and no other node \
             proved it holds them (none reachable, all full, or the proofs failed)",
            self.id,
            self.blobs.len()
        )
    }
}

impl std::error::Error for KeptLastCopy {}

/// What an eviction does with the blobs it would free (ADR 0029 §4).
#[derive(Debug, Default)]
pub(super) struct EvictionPlan {
    /// Replicas kept: the body goes, the blob stays; with the k each
    /// wants, recorded with it.
    pub kept: Vec<(String, usize)>,
    /// The k of every blob of the post, computed while it is still full.
    pub ks: HashMap<String, usize>,
    /// Blobs other nodes proved they hold: released.
    pub released: Vec<String>,
    /// Blobs no other node proved it holds: the eviction is refused.
    pub stranded: Vec<String>,
}

impl Node {
    /// What replica repair has done since the node started.
    pub fn repair_stats(&self) -> Stats {
        self.durability.stats()
    }

    /// Replicas a blob wants: `replication`, or the knowledge factor when
    /// a knowledge entry, or a source of one, lists it.
    pub fn replicas_wanted(&self, knowledge: bool) -> usize {
        if knowledge {
            self.config
                .durability
                .knowledge_replication
                .max(self.config.replication)
        } else {
            self.config.replication
        }
    }

    /// Whether this node is one of the `k` closest to blob `hash` (hex),
    /// among the peers it knows: an estimate that may only add to the
    /// replica record ([`Node::has_replica_intent`]), never remove it (ADR
    /// 0029 §1).
    pub fn is_responsible(&self, hash: &str, k: usize) -> bool {
        let Ok(hash) = crate::event::parse_hex::<32>(hash, "hash") else {
            return false;
        };
        let me = self.endpoint_id();
        responsible_set(&hash, me, &self.known_peers(), k).contains(&me)
    }

    /// The replicas blob `hash` (hex) wants (ADR 0029): the knowledge
    /// factor when one of the kept events listing it (the first 8) is
    /// knowledge, else `replication`. The k of rotation's
    /// `blob_responsible` (ADR 0025 PR B).
    pub async fn blob_target_k(&self, hash: &str) -> usize {
        let store = Arc::clone(&self.store);
        let hex = hash.to_string();
        let events: Vec<Arc<Event>> = match tokio::task::spawn_blocking(move || {
            store.events_listing_blob(&hex, LISTINGS_PER_BLOB)
        })
        .await
        {
            Ok(Ok(rows)) => rows.into_iter().map(|s| s.event).collect(),
            Ok(Err(e)) => {
                debug!(%hash, "events listing a blob not read: {e:#}");
                Vec::new()
            }
            Err(e) => {
                debug!(%hash, "events listing a blob not read: {e}");
                Vec::new()
            }
        };
        self.replicas_wanted(!self.knowledge_listed(&events).await.is_empty())
    }

    /// The known peers of network `net`: heard from on it in the last
    /// 24 hours (ADR 0026), or a gossip neighbor on it now.
    fn members(&self, net: &str) -> Vec<EndpointId> {
        let now = crate::node::now_secs();
        let neighbors: HashSet<EndpointId> = self
            .neighbors
            .lock()
            .ok()
            .and_then(|n| n.get(net).cloned())
            .unwrap_or_default();
        let swarm = self.swarm.lock().ok();
        self.known_peers()
            .into_iter()
            .filter(|p| {
                neighbors.contains(p) || swarm.as_ref().is_some_and(|s| s.seen(net, p, now))
            })
            .collect()
    }

    /// Test only: the members of `net` as this node sees them.
    #[doc(hidden)]
    pub fn members_for_tests(&self, net: &str) -> Vec<EndpointId> {
        self.members(net)
    }

    /// Whether this node is among the 2k closest to `hash` of its view of
    /// `net`: where a replica is recorded (ADR 0029 §1).
    fn within_twice_k(&self, hash: &Hash, net: &str) -> bool {
        let me = self.endpoint_id();
        let k = self.config.replication.saturating_mul(2);
        responsible_set(hash.as_bytes(), me, &self.members(net), k).contains(&me)
    }

    /// The ids, among `events`, of those a knowledge entry lists (ADR
    /// 0019 §5.2, as this node computes entries): an entry itself (a
    /// promoted digest, a canonical answer), or a source an entry rests on
    /// (a digest's `refs`, a canonical answer's thread root). Computed off
    /// the async runtime. Empty, and logged, when the store fails.
    async fn knowledge_listed(&self, events: &[Arc<Event>]) -> HashSet<String> {
        if events.is_empty() {
            return HashSet::new();
        }
        let mut threads: Vec<String> = events.iter().map(|e| e.thread_root().to_string()).collect();
        threads.sort();
        threads.dedup();
        let mut nets: Vec<&str> = events.iter().map(|e| e.net.as_str()).collect();
        nets.sort_unstable();
        nets.dedup();
        let mut grouping = crate::signals::Grouping::new(self.local_authors());
        for net in nets {
            match self.reputation(net) {
                Ok(table) => grouping = grouping.with_table(table),
                Err(e) => debug!(%net, "reputation not computed for repair: {e:#}"),
            }
        }
        let store = Arc::clone(&self.store);
        let computed = tokio::task::spawn_blocking(move || {
            crate::knowledge::compute(&store, &grouping, &threads, &HashMap::new())
        })
        .await;
        let knowledge = match computed {
            Ok(Ok(knowledge)) => knowledge,
            Ok(Err(e)) => {
                warn!("knowledge entries not computed for repair: {e:#}");
                return HashSet::new();
            }
            Err(e) => {
                warn!("knowledge entries not computed for repair: {e}");
                return HashSet::new();
            }
        };
        let mut listed = knowledge.ids();
        for entry in knowledge.digests.values().chain(knowledge.threads.values()) {
            listed.extend(entry.sources.iter().cloned());
        }
        events
            .iter()
            .filter(|e| listed.contains(&e.id))
            .map(|e| e.id.clone())
            .collect()
    }

    /// Record that this node holds blob `hash` of `size` bytes as a
    /// replica: it came by `replicate` (a publisher's push, a repair, a
    /// hand-off). Intent at fetch time is the source of truth for "this
    /// node is a replica" (ADR 0029 §1): the pusher chose this node from
    /// its own view of the swarm, which this node cannot recompute.
    /// Durable: a tag in the blob store ([`REPLICA_TAG`]). Deleting the
    /// blob deletes the record ([`super::unpin_blob`] drops every tag of a
    /// hash).
    pub(super) async fn record_replica_intent(&self, hash: Hash, size: u64) {
        if self.has_replica_intent(&hash).await {
            return;
        }
        if !self.replica_fits(size) {
            debug!(%hash, "replica share full: kept as a read cache");
            return;
        }
        match self.blobs.tags().set(replica_tag(&hash), hash).await {
            Ok(()) => {
                self.durability
                    .replica_bytes
                    .fetch_add(size, Ordering::Relaxed);
            }
            Err(e) => warn!(%hash, "replica not recorded: {e:#}"),
        }
    }

    /// Delete every tag of blob `hash` (the next collection deletes it,
    /// unless a kept event lists it), the replica record included, which
    /// leaves the replica bytes (N9).
    pub(super) async fn untag_blob(&self, hash: Hash) -> Result<()> {
        if self.has_replica_intent(&hash).await
            && let Ok(BlobStatus::Complete { size }) = self.blobs.status(hash).await
        {
            let bytes = &self.durability.replica_bytes;
            let mut current = bytes.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_sub(size);
                match bytes.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(seen) => current = seen,
                }
            }
        }
        super::unpin_blob(&self.blobs, hash).await
    }

    /// Untag the blobs a drop freed (best effort: a failure is logged, the
    /// body is gone anyway).
    pub(super) async fn untag_freed(&self, freed: &[String]) {
        for hex in freed {
            let Ok(hash) = hex.parse::<Hash>() else {
                continue;
            };
            if let Err(e) = self.untag_blob(hash).await {
                warn!(hash = %hex, "blob of a dropped body not untagged: {e:#}");
            }
        }
    }

    /// Count the bytes of the replicas held (at start, before `replicate`
    /// is served, and at each inventory).
    pub(super) async fn count_replica_bytes(&self) -> Result<()> {
        let mut replicas: Vec<Hash> = Vec::new();
        let mut tags = self.blobs.tags().list_prefix(REPLICA_TAG).await?;
        while let Some(tag) = tags.next().await {
            replicas.push(tag?.hash);
        }
        let mut bytes: u64 = 0;
        for hash in replicas {
            if let Ok(BlobStatus::Complete { size }) = self.blobs.status(hash).await {
                bytes = bytes.saturating_add(size);
            }
        }
        self.durability
            .replica_bytes
            .store(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Whether this node recorded blob `hash` as a replica
    /// ([`Node::record_replica_intent`]).
    pub async fn has_replica_intent(&self, hash: &Hash) -> bool {
        matches!(self.blobs.tags().get(replica_tag(hash)).await, Ok(Some(_)))
    }

    /// Whether blob `hash` is under one of this node's tags (an own post,
    /// a fetch, a replica): what keeps it from every blob collection.
    pub(super) async fn blob_tagged(&self, hash: &Hash) -> bool {
        let pinned = format!("{BLOB_TAG}{}", hash.to_hex());
        matches!(self.blobs.tags().get(pinned).await, Ok(Some(_)))
            || self.has_replica_intent(hash).await
    }

    /// The most bytes replicas may take here: [`REPLICA_SHARE_PERCENT`] of
    /// the blob part of `storage.max`. None without `storage.max`.
    pub(super) fn replica_cap(&self) -> Option<u64> {
        let max = self.config.storage_max?;
        let blob_share = max
            .saturating_mul(100u64.saturating_sub(self.config.events_share.min(100)))
            .checked_div(100)
            .unwrap_or(0);
        blob_share
            .saturating_mul(REPLICA_SHARE_PERCENT)
            .checked_div(100)
    }

    /// Whether a new replica of `size` bytes fits under
    /// [`Node::replica_cap`].
    pub(super) fn replica_fits(&self, size: u64) -> bool {
        self.replica_cap().is_none_or(|cap| {
            self.durability
                .replica_bytes
                .load(Ordering::Relaxed)
                .saturating_add(size)
                <= cap
        })
    }

    /// Whether blob `hash` should be recorded as a replica here when it
    /// comes by `replicate` for a post of `net`: within the 2k closest of
    /// this node's view (else it is kept as a read cache).
    pub(super) fn takes_as_replica(&self, hash: &Hash, net: &str) -> bool {
        self.within_twice_k(hash, net)
    }

    /// Whether this node holds `blob` whole: complete, of its declared
    /// size, and readable.
    pub(super) async fn holds_blob(&self, blob: &BlobRef) -> bool {
        let Ok(hash) = blob.hash.parse::<Hash>() else {
            return false;
        };
        matches!(
            self.blobs.status(hash).await,
            Ok(BlobStatus::Complete { size }) if size == blob.size
        ) && !self.blob_unreadable(hash).await
    }

    /// Whether event `id` is still held in full, with no drop record: a
    /// retraction or an erasure may land while a repair runs.
    async fn still_full(&self, id: &str) -> bool {
        let store = Arc::clone(&self.store);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            matches!(store.get(&id), Ok(Some(_))) && matches!(store.drop_record(&id), Ok(None))
        })
        .await
        .unwrap_or(false)
    }

    /// Ask `peer` to hold blob `hash` of `event`: `replicate`, through the
    /// paced push slots (ADR 0014, 0.4.2), giving up on a peer with no
    /// connection within [`REPAIR_CONNECT_LIMIT`]. No proof here.
    async fn ask_to_hold(&self, peer: EndpointId, event: &Event, hash: &str) -> Answer {
        let request = Request::Replicate {
            event: serde_json::to_value(event).unwrap_or_default(),
            hashes: vec![hash.to_string()],
        };
        let Ok(slot) = self.pushes.acquire(peer, SLOT_WAIT).await else {
            debug!(peer = %peer.fmt_short(), "repair: no push slot to this peer now");
            return Answer::Busy;
        };
        bump(&self.durability.stats.asked);
        let call =
            proto::call_solving_within(self.endpoint(), peer, &request, Some(REPAIR_CONNECT_LIMIT));
        let result = tokio::time::timeout(PUSH_TIMEOUT, call).await;
        drop(slot);
        if matches!(result, Ok(Ok(_))) {
            // It answered: reachable again (N2).
            self.durability.peer_seen(peer);
        }
        match result {
            Ok(Ok(Response::Replicated { hashes })) if hashes.iter().any(|h| h == hash) => {
                Answer::Holds
            }
            Ok(Ok(Response::Replicated { .. })) => Answer::Declined,
            Ok(Ok(Response::Refused { reason })) if reason.contains("rate limited") => Answer::Busy,
            // A refusal (storage full, quota, replica share, the event
            // rejected by its policy) means that peer will not hold it: the
            // next peer is asked at once, for 6 hours (S2, review 3).
            Ok(Ok(Response::Refused { reason })) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: refused: {reason}");
                Answer::Refused
            }
            Ok(Ok(Response::Challenge { .. })) => Answer::Busy,
            Ok(Ok(other)) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: declined: {other:?}");
                Answer::Declined
            }
            Ok(Err(e)) if e.downcast_ref::<proto::ConnectFailed>().is_some() => {
                debug!(peer = %peer.fmt_short(), "repair: unreachable: {e:#}");
                self.durability
                    .mark_unreachable(peer, self.config.durability.miss_gap);
                Answer::Unreachable
            }
            Ok(Err(e)) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: failed: {e:#}");
                Answer::Busy
            }
            Err(_) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: timed out");
                Answer::Busy
            }
        }
    }

    /// [`Node::ask_to_hold`], then a possession proof of a holder. A
    /// failed proof counts as a bare decline, a miss: two holders that
    /// prove each other at the same moment refuse each other's requests
    /// (the guard of [`super::proof`]), so one failure proves no lie; two
    /// an hour apart replace the peer. A hand-off never counts it.
    async fn confirm(&self, peer: EndpointId, event: &Event, blob: &BlobRef) -> Answer {
        let answer = self.ask_to_hold(peer, event, &blob.hash).await;
        if answer != Answer::Holds {
            return answer;
        }
        let Ok(hash) = blob.hash.parse::<Hash>() else {
            return Answer::Refused;
        };
        if self.prove_holder(peer, hash, blob.size).await {
            bump(&self.durability.stats.confirmed);
            Answer::Holds
        } else {
            bump(&self.durability.stats.proofs_failed);
            warn!(peer = %peer.fmt_short(), hash = %blob.hash, "a peer said it holds a blob but did not prove it");
            Answer::Declined
        }
    }

    /// Run replica repair in the background, if on.
    pub(super) fn spawn_repair(self: &Arc<Self>) {
        if !self.config.durability.repair {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(supervise("replica repair", move || {
            let weak = weak.clone();
            async move {
                let Some(first) = weak.upgrade().map(|n| n.config.durability.first_pass) else {
                    return;
                };
                tokio::time::sleep(jittered(first)).await;
                loop {
                    let Some(node) = weak.upgrade() else { break };
                    node.repair_pass().await;
                    let every = node.config.durability.every;
                    drop(node);
                    tokio::time::sleep(jittered(every)).await;
                }
            }
        }));
    }

    /// The blobs this node holds under a tag that kept full events list,
    /// read again when older than `inventory_every` (the store's listing
    /// off the async runtime). Recounts the bytes of the replicas.
    async fn repair_inventory(&self) -> Result<Inventory> {
        let every = self.config.durability.inventory_every;
        if let Ok(cached) = self.durability.inventory.lock()
            && let Some((at, list)) = cached.as_ref()
            && at.elapsed() < every
        {
            return Ok(Arc::clone(list));
        }
        let store = Arc::clone(&self.store);
        let listings =
            tokio::task::spawn_blocking(move || store.blob_listings(LISTINGS_PER_BLOB)).await??;
        let mut tagged: HashSet<Hash> = HashSet::new();
        let mut tags = self.blobs.tags().list_prefix("wats/").await?;
        while let Some(tag) = tags.next().await {
            tagged.insert(tag?.hash);
        }
        self.count_replica_bytes().await?;
        let list: Vec<(Hash, Vec<String>)> = listings
            .into_iter()
            .filter_map(|(hash, ids)| {
                let hash = hash.parse::<Hash>().ok()?;
                tagged.contains(&hash).then_some((hash, ids))
            })
            .collect();
        let list = Arc::new(list);
        if let Ok(mut cached) = self.durability.inventory.lock() {
            *cached = Some((Instant::now(), Arc::clone(&list)));
        }
        Ok(list)
    }

    /// One repair pass: at most `blobs_per_pass` blobs checked (blobs not
    /// watched here are skipped without counting), at most
    /// [`PER_PEER_PER_PASS`] requests to any one peer. Returns the blobs
    /// checked.
    pub async fn repair_pass(&self) -> usize {
        bump(&self.durability.stats.passes);
        let inventory = match self.repair_inventory().await {
            Ok(inventory) => inventory,
            Err(e) => {
                warn!("replica repair: blobs not listed: {e:#}");
                return 0;
            }
        };
        let tuning = &self.config.durability;
        let now = Instant::now();
        let due: Vec<(Hash, Vec<String>)> = {
            let Ok(mut checks) = self.durability.checks.lock() else {
                return 0;
            };
            let listed: HashSet<Hash> = inventory.iter().map(|(h, _)| *h).collect();
            checks.retain(|hash, _| listed.contains(hash));
            for (hash, _) in inventory.iter() {
                // First seen (a replica just taken, or every blob after a
                // restart): due at a random time within `recheck`.
                checks.entry(*hash).or_insert_with(|| {
                    Check::new(now.checked_add(within(tuning.recheck)).unwrap_or(now))
                });
            }
            let mut due: Vec<(Instant, Hash, Vec<String>)> = inventory
                .iter()
                .filter_map(|(hash, ids)| {
                    let check = checks.get(hash)?;
                    (check.due <= now).then(|| (check.due, *hash, ids.clone()))
                })
                .collect();
            due.sort_by_key(|(at, _, _)| *at);
            due.truncate(MAX_EXAMINED);
            due.into_iter().map(|(_, hash, ids)| (hash, ids)).collect()
        };
        if due.is_empty() {
            return self.review_evicted_replicas(tuning.blobs_per_pass).await;
        }
        // Cheap rejects first, not counted: blobs held here that are
        // neither a replica (record or estimate) nor an own post's.
        let mut work: Vec<(Hash, Vec<Arc<Event>>)> = Vec::new();
        let widest = self.replicas_wanted(true);
        let store = Arc::clone(&self.store);
        let read: Vec<(Hash, Vec<Arc<Event>>)> = tokio::task::spawn_blocking(move || {
            due.into_iter()
                .map(|(hash, ids)| {
                    let events = ids
                        .iter()
                        .filter_map(|id| store.get(id).ok().flatten().map(|s| s.event))
                        .collect();
                    (hash, events)
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        for (hash, events) in read {
            if work.len() >= tuning.blobs_per_pass {
                break;
            }
            let Some(first) = events.first() else {
                self.reschedule(hash, true, Vec::new());
                continue;
            };
            let me = self.endpoint_id();
            let watched = self.has_replica_intent(&hash).await
                || events.iter().any(|e| self.is_local_author(&e.author))
                || responsible_set(hash.as_bytes(), me, &self.members(&first.net), widest)
                    .contains(&me);
            if watched {
                work.push((hash, events));
            } else {
                self.reschedule(hash, true, Vec::new());
            }
        }
        let all: Vec<Arc<Event>> = work.iter().flat_map(|(_, e)| e.iter().cloned()).collect();
        let knowledge = self.knowledge_listed(&all).await;
        let budget: Mutex<HashMap<EndpointId, usize>> = Mutex::default();
        let gate = tokio::sync::Semaphore::new(CONCURRENT_CHECKS);
        let checks = work.iter().map(|(hash, events)| {
            // The listing knowledge prefers, else the oldest (S9).
            let event = events
                .iter()
                .find(|e| knowledge.contains(&e.id))
                .or_else(|| events.first())
                .cloned();
            let k = self.replicas_wanted(events.iter().any(|e| knowledge.contains(&e.id)));
            let (budget, gate) = (&budget, &gate);
            async move {
                let Some(event) = event else {
                    return Outcome::Skipped;
                };
                let Ok(_permit) = gate.acquire().await else {
                    return Outcome::Skipped;
                };
                self.check_blob(*hash, &event, k, budget).await
            }
        });
        let outcomes = n0_future::join_all(checks).await;
        let checked = outcomes.iter().filter(|o| **o != Outcome::Skipped).count();
        let left = tuning.blobs_per_pass.saturating_sub(checked);
        checked.saturating_add(self.review_evicted_replicas(left).await)
    }

    /// Replicas kept through an eviction (N4): released once this node is
    /// no longer among the 2k closest of its view of the post's network and
    /// k other holders are proven. Never pushed: their post is a tombstone
    /// here (ADR 0020 §3). At most `budget` looked at; returns those.
    async fn review_evicted_replicas(&self, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }
        let store = Arc::clone(&self.store);
        let now_secs = crate::node::now_secs();
        let limit = self.config.durability.evicted_per_pass.min(budget);
        let rows =
            match tokio::task::spawn_blocking(move || store.evicted_replica_rows(now_secs, limit))
                .await
            {
                Ok(Ok(rows)) => rows,
                Ok(Err(e)) => {
                    warn!("replica repair: evicted replicas not listed: {e:#}");
                    return 0;
                }
                Err(_) => return 0,
            };
        let recheck = self.config.durability.recheck;
        let mut done = 0usize;
        for (id, hex, net, row_k) in rows {
            if done >= budget {
                break;
            }
            let Ok(hash) = hex.parse::<Hash>() else {
                continue;
            };
            done = done.saturating_add(1);
            // Due again after `recheck` (jittered), whatever happens below:
            // the next rows come first meanwhile.
            let next = now_secs.saturating_add(jittered(recheck).as_secs());
            let store = Arc::clone(&self.store);
            let (due_id, due_hash) = (id.clone(), hex.clone());
            if let Ok(Err(e)) =
                tokio::task::spawn_blocking(move || store.set_evicted_due(&due_id, &due_hash, next))
                    .await
            {
                warn!(%hash, "evicted replica not rescheduled: {e:#}");
            }
            // Not held any more (a fault, a manual cleanup): nothing to keep.
            let Ok(BlobStatus::Complete { size }) = self.blobs.status(hash).await else {
                self.forget_evicted_replica(&id, &hex, hash).await;
                continue;
            };
            if self.within_twice_k(&hash, &net) {
                debug!(%hash, %id, "evicted replica kept: within the 2k closest");
                continue;
            }
            // The k recorded at eviction (5 for knowledge): the post is a
            // tombstone, so knowledge cannot be computed again (review 2).
            let k = if row_k > 0 {
                row_k
            } else {
                self.blob_target_k(&hex).await
            };
            let proven = self.prove_others(&hash, size, &net, k).await;
            if proven < k {
                debug!(%hash, %id, proven, k, "evicted replica kept: too few holders proven");
                continue;
            }
            if self.forget_evicted_replica(&id, &hex, hash).await {
                bump(&self.durability.stats.handed_off);
                info!(%hash, %id, k, "replica released: outside the 2k closest, k holders proven");
            }
        }
        done
    }

    /// Forget the replica `hex` kept for post `id`, and untag the blob
    /// when nothing else keeps it. True when it was untagged.
    async fn forget_evicted_replica(&self, id: &str, hex: &str, hash: Hash) -> bool {
        let store = Arc::clone(&self.store);
        let (row_id, row_hash) = (id.to_string(), hex.to_string());
        match tokio::task::spawn_blocking(move || store.release_evicted_replica(&row_id, &row_hash))
            .await
        {
            Ok(Ok(true)) => match self.untag_blob(hash).await {
                Ok(()) => true,
                Err(e) => {
                    warn!(%hash, "evicted replica not untagged: {e:#}");
                    false
                }
            },
            Ok(Ok(false)) => false,
            Ok(Err(e)) => {
                warn!(%hash, "evicted replica not released: {e:#}");
                false
            }
            Err(_) => false,
        }
    }

    /// Prove `need` holders of blob `hash` among the closest members of
    /// `net`, without asking them to take it: proofs only, at most
    /// [`HANDOFF_TRIES`] peers.
    async fn prove_others(&self, hash: &Hash, size: u64, net: &str, need: usize) -> usize {
        let peers = self.members(net);
        let gap = self.config.durability.miss_gap;
        // Where it probably is: the providers this node heard of (who
        // pushed it here), then the closest members.
        let mut order: Vec<EndpointId> = self
            .providers_for_hash(&hash.to_hex())
            .into_iter()
            .filter(|p| peers.contains(p))
            .collect();
        for p in proto::closest(hash.as_bytes(), &peers, peers.len()) {
            if !order.contains(&p) {
                order.push(p);
            }
        }
        order.truncate(HANDOFF_TRIES);
        let mut proven = 0usize;
        for peer in order {
            if proven >= need {
                break;
            }
            if self.durability.skip_dial(&peer, gap) {
                continue;
            }
            if self.prove_holder(peer, *hash, size).await {
                proven = proven.saturating_add(1);
            }
        }
        proven
    }

    /// Check one blob (see the module doc), and schedule its next check.
    async fn check_blob(
        &self,
        hash: Hash,
        event: &Arc<Event>,
        k: usize,
        budget: &Mutex<HashMap<EndpointId, usize>>,
    ) -> Outcome {
        let hex = hash.to_hex().to_string();
        let Some(blob) = event
            .blobs
            .iter()
            .find(|b| b.hash == hex && b.url.is_none())
        else {
            self.reschedule(hash, true, Vec::new());
            return Outcome::Skipped;
        };
        // Not held here (refused when full, never fetched): nothing to
        // repair from.
        if !self.holds_blob(blob).await {
            self.reschedule(hash, true, Vec::new());
            return Outcome::Skipped;
        }
        let me = self.endpoint_id();
        let peers = self.members(&event.net);
        // A replica by intent (it came by `replicate`), or by this node's
        // XOR estimate, which may only add (ADR 0029 §1).
        let replica = self.has_replica_intent(&hash).await
            || responsible_set(hash.as_bytes(), me, &peers, k).contains(&me);
        if !replica && !self.is_local_author(&event.author) {
            self.reschedule(hash, true, Vec::new());
            return Outcome::Skipped;
        }
        bump(&self.durability.stats.checked);
        let need = k.saturating_sub(usize::from(replica));
        let (previous, refused, declined) = self
            .durability
            .checks
            .lock()
            .ok()
            .and_then(|c| {
                c.get(&hash)
                    .map(|c| (c.holders.clone(), c.refused.clone(), c.declined.clone()))
            })
            .unwrap_or_default();
        let gap = self.config.durability.miss_gap;
        // The holders proven last time first, then the closest peers:
        // where a replica is pushed when one is missing.
        let mut order: Vec<EndpointId> = previous
            .iter()
            .filter(|p| peers.contains(p))
            .copied()
            .collect();
        for peer in proto::closest(hash.as_bytes(), &peers, peers.len()) {
            if !order.contains(&peer) {
                order.push(peer);
            }
        }
        let mut holders = Vec::new();
        let mut asked = 0usize;
        let mut not_checked = false;
        let mut new_refused: Vec<EndpointId> = Vec::new();
        let mut new_declines: Vec<EndpointId> = Vec::new();
        for peer in order {
            if holders.len() >= need || asked >= need.saturating_add(EXTRA_TRIES) {
                break;
            }
            if refused
                .get(&peer)
                .is_some_and(|at| at.elapsed() < REFUSED_FOR)
                || self.durability.gone(&peer, gap)
                || declined
                    .get(&peer)
                    .is_some_and(|m| m.for_good(gap) && m.last.elapsed() < REFUSED_FOR)
            {
                continue;
            }
            if self.durability.skip_dial(&peer, gap) {
                // Failed lately: a miss, not yet a replacement.
                if self.durability.gone(&peer, gap) {
                    continue;
                }
                not_checked = true;
                break;
            }
            // A retraction or an erasure may have landed meanwhile.
            if !self.still_full(&event.id).await {
                self.reschedule(hash, true, Vec::new());
                return Outcome::Checked;
            }
            let allowed = budget.lock().is_ok_and(|mut b| {
                let used = b.entry(peer).or_insert(0);
                *used = used.saturating_add(1);
                *used <= PER_PEER_PER_PASS
            });
            if !allowed {
                not_checked = true;
                break;
            }
            asked = asked.saturating_add(1);
            match self.confirm(peer, event, blob).await {
                Answer::Holds => holders.push(peer),
                Answer::Refused => new_refused.push(peer),
                Answer::Declined => {
                    let for_good = declined.get(&peer).is_some_and(|m| {
                        let mut m = *m;
                        m.again(gap);
                        m.for_good(gap)
                    });
                    new_declines.push(peer);
                    if !for_good {
                        not_checked = true;
                        break;
                    }
                }
                Answer::Busy => {
                    not_checked = true;
                    break;
                }
                Answer::Unreachable => {
                    if !self.durability.gone(&peer, gap) {
                        not_checked = true;
                        break;
                    }
                }
            }
        }
        if let Ok(mut checks) = self.durability.checks.lock() {
            let check = checks
                .entry(hash)
                .or_insert_with(|| Check::new(Instant::now()));
            let now = Instant::now();
            check.refused.retain(|_, at| at.elapsed() < REFUSED_FOR);
            check.declined.retain(|_, m| m.last.elapsed() < REFUSED_FOR);
            for peer in new_refused {
                check.refused.insert(peer, now);
            }
            for peer in new_declines {
                check
                    .declined
                    .entry(peer)
                    .and_modify(|m| m.again(gap))
                    .or_insert_with(Misses::new);
            }
            // A proven holder's misses are forgotten (N2).
            for peer in &holders {
                check.declined.remove(peer);
                check.refused.remove(peer);
            }
            if check.refused.len() > 64 {
                check.refused.clear();
            }
        }
        let added: Vec<EndpointId> = holders
            .iter()
            .filter(|p| !previous.contains(p))
            .copied()
            .collect();
        for _ in &added {
            bump(&self.durability.stats.added);
        }
        if not_checked && holders.len() < need {
            bump(&self.durability.stats.not_checked);
            debug!(%hash, holders = holders.len(), k, "replica check ended early: checked again soon");
            self.reschedule_soon(hash, holders.iter().chain(&previous).copied().collect());
            return Outcome::NotChecked;
        }
        let complete = holders.len() >= need;
        let replicas = holders.len().saturating_add(usize::from(replica));
        if complete {
            if !previous.is_empty() && !added.is_empty() {
                info!(%hash, replicas, k, "replica repaired");
            } else {
                debug!(%hash, replicas, k, "replicas confirmed");
            }
        } else {
            bump(&self.durability.stats.short);
            info!(%hash, replicas, k, asked, "blob short of replicas: no more peer took it this time");
        }
        self.reschedule(hash, complete, holders);
        Outcome::Checked
    }

    /// When blob `hash` is checked next: after `recheck` when `complete`,
    /// else after `retry`, doubling per check left short.
    fn reschedule(&self, hash: Hash, complete: bool, holders: Vec<EndpointId>) {
        let tuning = &self.config.durability;
        let Ok(mut checks) = self.durability.checks.lock() else {
            return;
        };
        let now = Instant::now();
        let check = checks.entry(hash).or_insert_with(|| Check::new(now));
        let wait = if complete {
            check.short = 0;
            jittered(tuning.recheck)
        } else {
            check.short = check.short.saturating_add(1);
            let doublings = check.short.saturating_sub(1).min(16);
            jittered(
                tuning
                    .retry
                    .saturating_mul(1 << doublings)
                    .min(tuning.recheck),
            )
        };
        check.due = now.checked_add(wait).unwrap_or(now);
        check.holders = holders;
    }

    /// A check that ended early comes back after `retry` (at most
    /// [`NOT_CHECKED_RETRY`]), without doubling.
    fn reschedule_soon(&self, hash: Hash, mut holders: Vec<EndpointId>) {
        holders.sort();
        holders.dedup();
        let wait = self.config.durability.retry.min(NOT_CHECKED_RETRY);
        if let Ok(mut checks) = self.durability.checks.lock() {
            let now = Instant::now();
            let check = checks.entry(hash).or_insert_with(|| Check::new(now));
            check.due = now.checked_add(jittered(wait)).unwrap_or(now);
            check.holders = holders;
        }
    }

    /// Prove (or make) `need` other holders of `blob`: peers asked with
    /// `replicate` and proven, in this order: the closest to the blob of
    /// the post's network (those responsible), then `history_full` peers
    /// (durable nodes), then the rest, at most [`HANDOFF_TRIES`]. Returns
    /// the holders proven.
    async fn hand_off(&self, event: &Event, blob: &BlobRef, need: usize) -> usize {
        let Ok(hash) = crate::event::parse_hex::<32>(&blob.hash, "hash") else {
            return 0;
        };
        let mut peers = self.members(&event.net);
        for p in self.providers_for_hash(&blob.hash) {
            if !peers.contains(&p) {
                peers.push(p);
            }
        }
        let by_distance = proto::closest(&hash, &peers, peers.len());
        let durable = |p: &EndpointId| {
            self.peer_info(p)
                .is_some_and(|i| i.features.iter().any(|f| f == proto::FEATURE_HISTORY_FULL))
        };
        let closest = self.config.replication.min(HANDOFF_TRIES);
        let mut order: Vec<EndpointId> = by_distance.iter().take(closest).copied().collect();
        order.extend(by_distance.iter().skip(closest).filter(|p| durable(p)));
        for p in &by_distance {
            if !order.contains(p) {
                order.push(*p);
            }
        }
        order.truncate(HANDOFF_TRIES);
        let mut proven = 0usize;
        let gap = self.config.durability.miss_gap;
        for peer in order {
            if proven >= need {
                break;
            }
            if self.durability.skip_dial(&peer, gap) {
                continue;
            }
            if self.confirm(peer, event, blob).await == Answer::Holds {
                proven = proven.saturating_add(1);
            }
        }
        proven
    }

    /// Last-copy protection (ADR 0029 §4) for an eviction of the body of
    /// `id`, before anything is dropped: for each blob the drop would
    /// free that this node holds, a replica stays (unless this node is no
    /// longer among the 2k closest and k other holders are proven); any
    /// other copy needs other holders proven (two when k > 1), or it is
    /// stranded and the eviction is refused.
    pub(super) async fn plan_eviction(&self, id: &str, event: &Event) -> Result<EvictionPlan> {
        let store = Arc::clone(&self.store);
        let owned = id.to_string();
        let freed = tokio::task::spawn_blocking(move || store.blobs_freed_by(&owned)).await??;
        let mut plan = EvictionPlan::default();
        for blob in event.blobs.iter().filter(|b| b.url.is_none()) {
            if !plan.ks.contains_key(&blob.hash) {
                let k = self.blob_target_k(&blob.hash).await;
                plan.ks.insert(blob.hash.clone(), k);
            }
        }
        let k_of = |hex: &str| plan_k(&plan.ks, hex, self.config.replication);
        // Every replica blob of the post is recorded, freed or not (N8):
        // when the other events that list it go too, the record keeps it.
        let mut kept: Vec<(String, usize)> = Vec::new();
        for blob in event.blobs.iter().filter(|b| b.url.is_none()) {
            if freed.contains(&blob.hash) {
                continue;
            }
            if let Ok(hash) = blob.hash.parse::<Hash>()
                && self.has_replica_intent(&hash).await
                && !kept.iter().any(|(h, _)| *h == blob.hash)
            {
                kept.push((blob.hash.clone(), k_of(&blob.hash)));
            }
        }
        for hex in freed {
            let Some(blob) = event
                .blobs
                .iter()
                .find(|b| b.hash == hex && b.url.is_none())
            else {
                continue;
            };
            let Ok(hash) = hex.parse::<Hash>() else {
                continue;
            };
            // No copy here, nothing to lose.
            if !self.holds_blob(blob).await {
                continue;
            }
            let k = k_of(&hex);
            if self.has_replica_intent(&hash).await {
                if !self.within_twice_k(&hash, &event.net)
                    && self.hand_off(event, blob, k).await >= k
                {
                    bump(&self.durability.stats.handed_off);
                    plan.released.push(hex);
                } else {
                    kept.push((hex, k));
                }
                continue;
            }
            let need = k.clamp(1, 2);
            if self.hand_off(event, blob, need).await >= need {
                bump(&self.durability.stats.handed_off);
                plan.released.push(hex);
            } else {
                bump(&self.durability.stats.kept);
                warn!(hash = %hex, "kept: no other node proved it holds this blob, and this node may hold its last copy");
                plan.stranded.push(hex);
            }
        }
        plan.kept = kept;
        Ok(plan)
    }

    /// Blobs an eviction freed that its plan did not cover (another event
    /// that listed them was dropped meanwhile): handed off now with the
    /// event read before the drop, or kept (S7). Returns those kept.
    pub(super) async fn cover_late(
        &self,
        event: &Event,
        late: &[String],
        ks: &HashMap<String, usize>,
    ) -> Vec<(String, usize)> {
        let mut kept = Vec::new();
        for hex in late {
            let Some(blob) = event.blobs.iter().find(|b| &b.hash == hex) else {
                continue;
            };
            if !self.holds_blob(blob).await {
                continue;
            }
            let k = plan_k(ks, hex, self.config.replication);
            let need = k.clamp(1, 2);
            if self.hand_off(event, blob, need).await < need {
                kept.push((hex.clone(), k));
            }
        }
        kept
    }
}

/// The k a plan computed for blob `hex`, else `fallback`.
fn plan_k(ks: &HashMap<String, usize>, hex: &str, fallback: usize) -> usize {
    ks.get(hex).copied().unwrap_or(fallback)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn id(i: u8) -> EndpointId {
        SecretKey::from_bytes(&[i; 32]).public()
    }

    #[test]
    fn the_responsible_set_counts_this_node() {
        let peers: Vec<EndpointId> = (1..=9).map(id).collect();
        let hash = *blake3::hash(b"blob").as_bytes();
        for i in 1..=9 {
            let me = id(i);
            let others: Vec<EndpointId> = peers.iter().copied().filter(|p| *p != me).collect();
            let set = responsible_set(&hash, me, &others, 3);
            assert_eq!(set.len(), 3);
            assert_eq!(
                set,
                proto::closest(&hash, &peers, 3),
                "same set from every node"
            );
        }
        // Fewer nodes than k: all of them.
        assert_eq!(responsible_set(&hash, id(1), &[id(2)], 5).len(), 2);
    }

    #[test]
    fn jitter_stays_within_half_either_side() {
        for _ in 0..200 {
            let d = jittered(Duration::from_secs(10));
            assert!(
                d >= Duration::from_secs(5) && d <= Duration::from_secs(15),
                "{d:?}"
            );
            assert!(within(Duration::from_secs(3)) <= Duration::from_secs(3));
        }
        assert_eq!(within(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn a_peer_is_gone_after_two_misses_far_enough_apart() {
        let state = State::default();
        let peer = id(7);
        assert!(!state.skip_dial(&peer, Duration::from_secs(3600)));
        let gap = Duration::from_secs(3600);
        state.mark_unreachable(peer, gap);
        assert!(state.skip_dial(&peer, gap), "not dialed again at once");
        assert!(!state.gone(&peer, gap), "one miss");
        assert!(state.gone(&peer, Duration::ZERO));
        state.mark_unreachable(peer, gap);
        assert_eq!(state.unreachable.lock().unwrap()[&peer].failures, 2);
        state.peer_seen(peer);
        assert!(!state.skip_dial(&peer, gap) && !state.gone(&peer, Duration::ZERO));
    }

    #[test]
    fn declines_count_for_good_only_far_enough_apart() {
        let mut misses = Misses::new();
        assert!(!misses.for_good(Duration::from_secs(1)));
        misses.last = misses.first.checked_add(Duration::from_secs(2)).unwrap();
        assert!(misses.for_good(Duration::from_secs(1)));
        // A miss long after the last one starts a new sequence.
        let mut misses = Misses::new();
        std::thread::sleep(Duration::from_millis(20));
        misses.again(Duration::from_millis(1));
        assert!(
            !misses.for_good(Duration::from_millis(1)),
            "6 gaps later: anew"
        );
    }
}
