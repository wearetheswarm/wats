//! Blob durability (ADR 0029): replica repair, more replicas for
//! knowledge, and last-copy protection.
//!
//! - **Replica repair.** Every node walks, at a low and jittered rate, the
//!   blobs it holds as a replica (or that its own posts list). For each
//!   one it asks the holders it last confirmed, then the peers closest to
//!   the hash, to hold it, with the `replicate` request every release
//!   answers: a peer that holds the blob confirms at once, one that does
//!   not fetches it from this node and records itself as a replica. It
//!   stops once the blob has its k replicas, itself included. A peer that
//!   is gone, full or refuses is replaced by the next closest. No wire
//!   change.
//! - **Knowledge replicas.** A blob listed by a knowledge entry (ADR 0019
//!   §5.2: a promoted digest, a thread's canonical answer), or by one of
//!   the sources an entry rests on, wants [`KNOWLEDGE_REPLICATION`] (5)
//!   replicas instead of `replication` (3), as this node computes entries.
//! - **Last copy.** Before a node deletes a blob for storage pressure
//!   (`evicted`) or on its human's `wats drop`, another node must confirm
//!   it holds the blob, or take it first ([`Node::hand_off_before_drop`]).
//!   Retractions, erasure labels and moderation labels delete regardless
//!   (ADR 0020): those are reasons why the content must not be kept.
//!
//! A node is a replica of a blob when the blob came to it by `replicate`:
//! the receiver records that intent at fetch time (a `wats/replica/` tag),
//! because the pusher chose it from its own view of the swarm, which no
//! other node can recompute. The XOR estimate (the k closest among the
//! peers this node knows and can reach, plus itself) may only add to it.
//! Where a missing replica goes is the closest peers that do not hold it;
//! two nodes with different views may push to different peers, which
//! costs at most an extra replica.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::EndpointId;
use iroh_blobs::Hash;
use iroh_blobs::api::proto::BlobStatus;
use serde::Serialize;
use tracing::{debug, info, warn};

use super::{Node, PUSH_TIMEOUT, REPAIR_CONNECT_LIMIT};
use crate::event::{BlobRef, Event};
use crate::proto::{self, Request, Response};
use crate::supervise::supervise;

/// Replicas a blob of knowledge wants (ADR 0029, decided by Vincent on
/// 2026-10-21), instead of `replication`.
pub const KNOWLEDGE_REPLICATION: usize = 5;

/// Peers asked for one blob in one check beyond the replicas it lacks:
/// room to replace that many gone, full or refusing peers.
const EXTRA_TRIES: usize = 3;
/// Requests one pass sends to one peer, all blobs together: far under a
/// receiver's `limits.replicate_per_min` (120, a burst of 30).
const PER_PEER_PER_PASS: usize = 16;
/// Blobs checked at once in a pass.
const CONCURRENT_CHECKS: usize = 4;
/// Peers asked to take a blob before it is deleted (last copy).
const HANDOFF_TRIES: usize = 6;
/// The longest a repair or a hand-off waits for one of the paced push
/// slots to a peer ([`super::PUSHES_PER_PEER`]); past it the peer counts
/// as busy for this check.
const SLOT_WAIT: Duration = Duration::from_secs(60);
/// A peer found unreachable is left out of the k closest for this long,
/// doubling with each further failure up to [`UNREACHABLE_MAX`], or until
/// it contacts this node again.
const UNREACHABLE_FIRST: Duration = Duration::from_secs(600);
const UNREACHABLE_MAX: Duration = Duration::from_secs(24 * 3600);

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
    /// Most blobs checked in one pass.
    pub blobs_per_pass: usize,
    /// The list of the blobs this node holds is read again this often (a
    /// scan of the store, as the blob collection does).
    pub inventory_every: Duration,
    /// Replicas of a blob of knowledge: [`KNOWLEDGE_REPLICATION`].
    pub knowledge_replication: usize,
    /// Test only: answer `replicate` as 0.4.2 does (a blob already held
    /// goes through the fetch slots and the storage checks).
    pub legacy_replicate: bool,
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
            legacy_replicate: false,
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
    /// `replicate` requests sent by repair and hand-offs.
    pub asked: u64,
    /// Answers that confirmed a replica.
    pub confirmed: u64,
    /// Replicas confirmed by a peer that had not confirmed this blob in
    /// the previous check: a replica restored (or one first seen).
    pub added: u64,
    /// Checks that left a blob short of its replicas.
    pub short: u64,
    /// Blobs handed to another node before a deletion.
    pub handed_off: u64,
    /// Deletions refused: this node may hold the last copy.
    pub kept: u64,
    /// `replicate` requests this node served (from any peer).
    pub served: u64,
}

#[derive(Debug, Default)]
struct Counters {
    passes: AtomicU64,
    checked: AtomicU64,
    asked: AtomicU64,
    confirmed: AtomicU64,
    added: AtomicU64,
    short: AtomicU64,
    handed_off: AtomicU64,
    kept: AtomicU64,
    served: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// When a blob is checked next, and what the last check found.
#[derive(Debug, Clone)]
struct Check {
    due: Instant,
    /// Checks in a row that left it short.
    short: u32,
    /// The peers that confirmed it at the last check.
    holders: Vec<EndpointId>,
}

/// The blobs kept full events list, one event each.
type Inventory = Arc<Vec<(Hash, String)>>;

/// Repair's state ([`Node::durability`]): in memory only, rebuilt after a
/// restart (every blob is then due at a random time within `recheck`).
#[derive(Debug, Default)]
pub struct State {
    checks: Mutex<HashMap<Hash, Check>>,
    /// The blobs kept full events list, one event each, and when read.
    inventory: Mutex<Option<(Instant, Inventory)>>,
    /// Peers found unreachable: (failures in a row, left out until).
    unreachable: Mutex<HashMap<EndpointId, (u32, Instant)>>,
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

    fn mark_unreachable(&self, peer: EndpointId) {
        if let Ok(mut unreachable) = self.unreachable.lock() {
            if unreachable.len() >= 10_000 {
                let now = Instant::now();
                unreachable.retain(|_, (_, until)| *until > now);
            }
            let failures = unreachable
                .get(&peer)
                .map_or(0, |(n, _)| *n)
                .saturating_add(1);
            let doublings = failures.saturating_sub(1).min(16);
            let wait = UNREACHABLE_FIRST
                .saturating_mul(1 << doublings)
                .min(UNREACHABLE_MAX);
            if let Some(until) = Instant::now().checked_add(wait) {
                unreachable.insert(peer, (failures, until));
            }
        }
    }

    fn is_unreachable(&self, peer: &EndpointId) -> bool {
        self.unreachable
            .lock()
            .ok()
            .and_then(|u| u.get(peer).map(|(_, until)| Instant::now() < *until))
            .unwrap_or(false)
    }

    fn stats(&self) -> Stats {
        let s = &self.stats;
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Stats {
            passes: get(&s.passes),
            checked: get(&s.checked),
            asked: get(&s.asked),
            confirmed: get(&s.confirmed),
            added: get(&s.added),
            short: get(&s.short),
            handed_off: get(&s.handed_off),
            kept: get(&s.kept),
            served: get(&s.served),
        }
    }
}

/// The tag that records a blob held as a replica, followed by its hash.
pub const REPLICA_TAG: &str = "wats/replica/";

fn replica_tag(hash: &Hash) -> String {
    format!("{REPLICA_TAG}{}", hash.to_hex())
}

/// The `k` nodes responsible for `hash`: the `k` closest to it by XOR
/// distance among `peers` and `me` (ADR 0005, ADR 0029).
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
    /// It holds the blob (had it, or fetched it from us).
    Holds,
    /// It answered without the blob (full, quota, body dropped there,
    /// refused, busy) or did not answer in time.
    Declined,
    /// No connection to it.
    Unreachable,
}

/// A deletion refused because this node may hold the last copy of some
/// of the post's blobs and no other node took them (ADR 0029).
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
             took them (none reachable, or all full)",
            self.id,
            self.blobs.len()
        )
    }
}

impl std::error::Error for KeptLastCopy {}

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
    /// among the peers it knows and can reach: an estimate that may only
    /// add to the replica record ([`Node::has_replica_intent`]), never
    /// remove it (ADR 0029 §1).
    pub fn is_responsible(&self, hash: &str, k: usize) -> bool {
        let Ok(hash) = crate::event::parse_hex::<32>(hash, "hash") else {
            return false;
        };
        let me = self.endpoint_id();
        responsible_set(&hash, me, &self.repair_peers(), k).contains(&me)
    }

    /// The replicas blob `hash` (hex) wants (ADR 0029): the knowledge
    /// factor when one of the kept events listing it (the first 8) is
    /// knowledge, else `replication`. What `blob_responsible` of rotation
    /// (ADR 0025 PR B) takes as its k.
    pub fn blob_target_k(&self, hash: &str) -> usize {
        let events: Vec<Arc<Event>> = match self.store.events_listing_blob(hash, 8) {
            Ok(rows) => rows.into_iter().map(|s| s.event).collect(),
            Err(e) => {
                debug!(%hash, "events listing a blob not read: {e:#}");
                Vec::new()
            }
        };
        self.replicas_wanted(!self.knowledge_listed(&events).is_empty())
    }

    /// The blobs of `event` this node is responsible for, each with the
    /// replicas it wants (knowledge counted, ADR 0029).
    pub fn responsible_blobs(&self, event: &Arc<Event>) -> Vec<String> {
        let knowledge = self.knowledge_listed(std::slice::from_ref(event));
        let k = self.replicas_wanted(knowledge.contains(&event.id));
        event
            .blobs
            .iter()
            .filter(|b| b.url.is_none() && self.is_responsible(&b.hash, k))
            .map(|b| b.hash.clone())
            .collect()
    }

    /// Known peers, without those found unreachable lately.
    fn repair_peers(&self) -> Vec<EndpointId> {
        self.known_peers()
            .into_iter()
            .filter(|p| !self.durability.is_unreachable(p))
            .collect()
    }

    /// The ids, among `events`, of those a knowledge entry lists (ADR
    /// 0019 §5.2, as this node computes entries): an entry itself (a
    /// promoted digest, a canonical answer), or a source an entry rests on
    /// (a digest's `refs`, a canonical answer's thread root). Empty, and
    /// logged, when the store fails.
    fn knowledge_listed(&self, events: &[Arc<Event>]) -> HashSet<String> {
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
        let knowledge =
            match crate::knowledge::compute(&self.store, &grouping, &threads, &HashMap::new()) {
                Ok(knowledge) => knowledge,
                Err(e) => {
                    warn!("knowledge entries not computed for repair: {e:#}");
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

    /// Record that this node holds blob `hash` as a replica: it came by
    /// `replicate` (a publisher's push or a repair). Intent at fetch time is
    /// the source of truth for "this node is a replica" (ADR 0029 §1): the
    /// pusher chose this node from its own view of the swarm, which this
    /// node cannot recompute. Durable: a tag in the blob store
    /// ([`REPLICA_TAG`]), which also keeps the blob from the collection.
    /// Deleting the blob deletes the record ([`super::unpin_blob`] drops
    /// every tag of a hash).
    pub(super) async fn record_replica_intent(&self, hash: Hash) {
        if let Err(e) = self.blobs.tags().set(replica_tag(&hash), hash).await {
            warn!(%hash, "replica not recorded: {e:#}");
        }
    }

    /// Whether this node recorded blob `hash` as a replica
    /// ([`Node::record_replica_intent`]).
    pub async fn has_replica_intent(&self, hash: &Hash) -> bool {
        matches!(self.blobs.tags().get(replica_tag(hash)).await, Ok(Some(_)))
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

    /// Ask `peer` to hold blob `hash` of `event`: `replicate`, through the
    /// paced push slots (ADR 0014, 0.4.2), giving up on a peer with no
    /// connection within [`REPAIR_CONNECT_LIMIT`].
    async fn ask_to_hold(&self, peer: EndpointId, event: &Event, hash: &str) -> Answer {
        let request = Request::Replicate {
            event: serde_json::to_value(event).unwrap_or_default(),
            hashes: vec![hash.to_string()],
        };
        let Ok(slot) = self.pushes.acquire(peer, SLOT_WAIT).await else {
            debug!(peer = %peer.fmt_short(), "repair: no push slot to this peer now");
            return Answer::Declined;
        };
        bump(&self.durability.stats.asked);
        let call =
            proto::call_solving_within(self.endpoint(), peer, &request, Some(REPAIR_CONNECT_LIMIT));
        let result = tokio::time::timeout(PUSH_TIMEOUT, call).await;
        drop(slot);
        match result {
            Ok(Ok(Response::Replicated { hashes })) if hashes.iter().any(|h| h == hash) => {
                bump(&self.durability.stats.confirmed);
                Answer::Holds
            }
            Ok(Ok(other)) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: declined: {other:?}");
                Answer::Declined
            }
            Ok(Err(e)) if e.downcast_ref::<proto::ConnectFailed>().is_some() => {
                debug!(peer = %peer.fmt_short(), "repair: unreachable: {e:#}");
                self.durability.mark_unreachable(peer);
                Answer::Unreachable
            }
            Ok(Err(e)) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: failed: {e:#}");
                Answer::Declined
            }
            Err(_) => {
                debug!(peer = %peer.fmt_short(), %hash, "repair: timed out");
                Answer::Declined
            }
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

    /// The blobs kept full events list, read again when older than
    /// `inventory_every`.
    fn repair_inventory(&self) -> Result<Inventory> {
        let every = self.config.durability.inventory_every;
        if let Ok(cached) = self.durability.inventory.lock()
            && let Some((at, list)) = cached.as_ref()
            && at.elapsed() < every
        {
            return Ok(Arc::clone(list));
        }
        let list: Vec<(Hash, String)> = self
            .store
            .blob_listings()?
            .into_iter()
            .filter_map(|(hash, id)| Some((hash.parse::<Hash>().ok()?, id)))
            .collect();
        let list = Arc::new(list);
        if let Ok(mut cached) = self.durability.inventory.lock() {
            *cached = Some((Instant::now(), Arc::clone(&list)));
        }
        Ok(list)
    }

    /// One repair pass: at most `blobs_per_pass` blobs due, at most
    /// [`PER_PEER_PER_PASS`] requests to any one peer. Returns the blobs
    /// checked.
    pub async fn repair_pass(&self) -> usize {
        bump(&self.durability.stats.passes);
        let inventory = match self.repair_inventory() {
            Ok(inventory) => inventory,
            Err(e) => {
                warn!("replica repair: blobs not listed: {e:#}");
                return 0;
            }
        };
        let tuning = &self.config.durability;
        let now = Instant::now();
        let due: Vec<(Hash, String)> = {
            let Ok(mut checks) = self.durability.checks.lock() else {
                return 0;
            };
            let listed: HashSet<Hash> = inventory.iter().map(|(h, _)| *h).collect();
            checks.retain(|hash, _| listed.contains(hash));
            for (hash, _) in inventory.iter() {
                checks.entry(*hash).or_insert_with(|| Check {
                    // First seen (a replica just taken, or every blob after
                    // a restart): due at a random time within `recheck`.
                    due: now.checked_add(within(tuning.recheck)).unwrap_or(now),
                    short: 0,
                    holders: Vec::new(),
                });
            }
            let mut due: Vec<(Instant, Hash, String)> = inventory
                .iter()
                .filter_map(|(hash, id)| {
                    let check = checks.get(hash)?;
                    (check.due <= now).then(|| (check.due, *hash, id.clone()))
                })
                .collect();
            due.sort_by_key(|(at, _, _)| *at);
            due.truncate(tuning.blobs_per_pass);
            due.into_iter().map(|(_, hash, id)| (hash, id)).collect()
        };
        if due.is_empty() {
            return 0;
        }
        // The events listing them, and which of them knowledge lists.
        let mut work: Vec<(Hash, Arc<Event>)> = Vec::new();
        for (hash, id) in due {
            match self.store.get(&id) {
                Ok(Some(stored)) => work.push((hash, stored.event)),
                Ok(None) => self.reschedule(hash, true, Vec::new()),
                Err(e) => {
                    warn!(%id, "replica repair: event not read: {e:#}");
                    self.reschedule(hash, false, Vec::new());
                }
            }
        }
        let events: Vec<Arc<Event>> = work.iter().map(|(_, e)| Arc::clone(e)).collect();
        let knowledge = self.knowledge_listed(&events);
        let peers = self.repair_peers();
        let budget: Mutex<HashMap<EndpointId, usize>> = Mutex::default();
        let gate = tokio::sync::Semaphore::new(CONCURRENT_CHECKS);
        let checks = work.iter().map(|(hash, event)| {
            let k = self.replicas_wanted(knowledge.contains(&event.id));
            let (peers, budget, gate) = (&peers, &budget, &gate);
            async move {
                let Ok(_permit) = gate.acquire().await else {
                    return;
                };
                self.check_blob(*hash, event, k, peers, budget).await;
            }
        });
        n0_future::join_all(checks).await;
        work.len()
    }

    /// Check one blob (see the module doc), and schedule its next check.
    async fn check_blob(
        &self,
        hash: Hash,
        event: &Arc<Event>,
        k: usize,
        peers: &[EndpointId],
        budget: &Mutex<HashMap<EndpointId, usize>>,
    ) {
        let hex = hash.to_hex().to_string();
        let Some(blob) = event
            .blobs
            .iter()
            .find(|b| b.hash == hex && b.url.is_none())
        else {
            self.reschedule(hash, true, Vec::new());
            return;
        };
        // Not held here (refused when full, never fetched): nothing to
        // repair from.
        if !self.holds_blob(blob).await {
            self.reschedule(hash, true, Vec::new());
            return;
        }
        let me = self.endpoint_id();
        // A replica by intent (it came by `replicate`), or by this node's
        // XOR estimate, which may only add (ADR 0029 §1).
        let replica = self.has_replica_intent(&hash).await
            || responsible_set(hash.as_bytes(), me, peers, k).contains(&me);
        if !replica && !self.is_local_author(&event.author) {
            // Someone else's to watch (a copy fetched on read).
            self.reschedule(hash, true, Vec::new());
            return;
        }
        bump(&self.durability.stats.checked);
        let need = k.saturating_sub(usize::from(replica));
        let previous: Vec<EndpointId> = self
            .durability
            .checks
            .lock()
            .ok()
            .and_then(|c| c.get(&hash).map(|c| c.holders.clone()))
            .unwrap_or_default();
        // The holders confirmed last time first, then the closest peers:
        // where a replica is pushed when one is missing.
        let mut order: Vec<EndpointId> = previous
            .iter()
            .filter(|p| peers.contains(p))
            .copied()
            .collect();
        for peer in proto::closest(hash.as_bytes(), peers, peers.len()) {
            if !order.contains(&peer) {
                order.push(peer);
            }
        }
        let mut holders = Vec::new();
        let mut asked = 0usize;
        for peer in order {
            if holders.len() >= need || asked >= need.saturating_add(EXTRA_TRIES) {
                break;
            }
            let allowed = budget.lock().is_ok_and(|mut b| {
                let used = b.entry(peer).or_insert(0);
                *used = used.saturating_add(1);
                *used <= PER_PEER_PER_PASS
            });
            if !allowed {
                continue;
            }
            asked = asked.saturating_add(1);
            if self.ask_to_hold(peer, event, &hex).await == Answer::Holds {
                holders.push(peer);
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
        let complete = holders.len() >= need;
        if complete {
            if !previous.is_empty() && !added.is_empty() {
                info!(
                    %hash,
                    replicas = holders.len().saturating_add(usize::from(replica)),
                    k,
                    "replica repaired"
                );
            } else {
                debug!(%hash, holders = holders.len(), k, "replicas confirmed");
            }
        } else {
            bump(&self.durability.stats.short);
            info!(
                %hash,
                replicas = holders.len().saturating_add(usize::from(replica)),
                k,
                asked,
                "blob short of replicas: no more peer took it this time"
            );
        }
        self.reschedule(hash, complete, holders);
    }

    /// When blob `hash` is checked next: after `recheck` when `complete`,
    /// else after `retry`, doubling per check left short.
    fn reschedule(&self, hash: Hash, complete: bool, holders: Vec<EndpointId>) {
        let tuning = &self.config.durability;
        let Ok(mut checks) = self.durability.checks.lock() else {
            return;
        };
        let now = Instant::now();
        let check = checks.entry(hash).or_insert_with(|| Check {
            due: now,
            short: 0,
            holders: Vec::new(),
        });
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

    /// Last-copy protection (ADR 0029), before the body of event `id` is
    /// dropped for a reason other than a retraction or a label: for each
    /// blob the drop would free that this node holds, another node must
    /// confirm it holds it, or take it now. Peers are asked in this order:
    /// the closest to the blob (those responsible for it), then
    /// `history_full` peers (durable nodes), then the rest, at most
    /// [`HANDOFF_TRIES`]. Returns the blobs no other node took: deleting
    /// them may lose their last copy. Replicas (a replica record) are
    /// handed off like any other copy.
    pub async fn hand_off_before_drop(&self, id: &str) -> Result<Vec<String>> {
        self.hand_off_before_drop_except(id, &[]).await
    }

    /// [`Node::hand_off_before_drop`], leaving out the blobs in `kept`
    /// (hex), which the drop will not delete.
    pub(super) async fn hand_off_before_drop_except(
        &self,
        id: &str,
        kept: &[String],
    ) -> Result<Vec<String>> {
        let mut freed = self.store.blobs_freed_by(id)?;
        freed.retain(|h| !kept.contains(h));
        if freed.is_empty() {
            return Ok(Vec::new());
        }
        let Some(stored) = self.store.get(id)? else {
            return Ok(Vec::new());
        };
        let event = stored.event;
        let mut stranded = Vec::new();
        for hex in freed {
            let Some(blob) = event
                .blobs
                .iter()
                .find(|b| b.hash == hex && b.url.is_none())
            else {
                continue;
            };
            // No copy here, nothing to lose.
            if !self.holds_blob(blob).await {
                continue;
            }
            if self.hand_off(&event, blob).await {
                bump(&self.durability.stats.handed_off);
            } else {
                bump(&self.durability.stats.kept);
                stranded.push(hex);
            }
        }
        Ok(stranded)
    }

    /// Find, or make, another holder of `blob` (see
    /// [`Node::hand_off_before_drop`]).
    async fn hand_off(&self, event: &Event, blob: &BlobRef) -> bool {
        let Ok(hash) = crate::event::parse_hex::<32>(&blob.hash, "hash") else {
            return false;
        };
        let mut peers = self.repair_peers();
        for p in self.providers_for_hash(&blob.hash) {
            if !peers.contains(&p) && !self.durability.is_unreachable(&p) {
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
        for peer in order {
            if self.ask_to_hold(peer, event, &blob.hash).await == Answer::Holds {
                info!(hash = %blob.hash, peer = %peer.fmt_short(), "another node holds the blob: it may be deleted here");
                return true;
            }
        }
        warn!(
            hash = %blob.hash,
            "kept: no other node took this blob, and this node may hold its last copy"
        );
        false
    }
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
    fn unreachable_peers_back_off_until_seen() {
        let state = State::default();
        let peer = id(7);
        assert!(!state.is_unreachable(&peer));
        state.mark_unreachable(peer);
        assert!(state.is_unreachable(&peer));
        state.mark_unreachable(peer);
        let (failures, _) = state.unreachable.lock().unwrap()[&peer];
        assert_eq!(failures, 2);
        state.peer_seen(peer);
        assert!(!state.is_unreachable(&peer));
    }
}
