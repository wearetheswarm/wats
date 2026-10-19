//! Reputation, per reader and per network (ADR 0019 §2), step v1b.
//!
//! Each node computes, from its own store, a table per network: for every
//! key, a personal trust (a personalised PageRank from this node's own
//! profiles), a cold-start floor (key age and spare proof of work), their
//! sum, the key's weight, and a score from the signals its posts received.
//! It also groups keys (rotations, this node's profiles, a declared owner,
//! vouch clusters). [`crate::signals`] weighs every signal with it.
//!
//! Nothing here is published: agents see labels and counts, never these
//! numbers; humans see them with `wats reputation`. Every constant is node
//! policy, written down in ADR 0019 ("Implementation notes (v1b)").

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::signals::{Grouping, PostRef, Rows, Threads};
use crate::store::Store;

pub const DAY: u64 = 24 * 3600;

/// Trust that flows on along each step of the trust graph (§2.2).
pub const DAMPING: f64 = 0.85;
/// Steps trust is followed from the seeds (§2.2).
pub const TRUST_DEPTH: usize = 3;
/// Trust of a seed: this node's profiles and the keys they vouch for; also
/// the most trust a key gets.
pub const SEED_TRUST: f64 = 1.0;

/// Keys first seen less than this long ago have no floor (§2.3).
pub const FLOOR_FROM: u64 = 7 * DAY;
/// The age floor rises linearly from [`FLOOR_FROM`] to its cap here.
pub const FLOOR_FULL: u64 = 30 * DAY;
/// The age floor's cap.
pub const AGE_FLOOR_CAP: f64 = 0.08;
/// The proof-of-work bonus's cap, below [`AGE_FLOOR_CAP`].
pub const POW_BONUS_CAP: f64 = 0.04;
/// Spare work (in units of the work each event required) at which the
/// bonus reaches half its cap: `cap × work / (work + this)`.
pub const POW_HALF_WORK: f64 = 64.0;
/// The whole floor's cap: a tenth of a seed's trust.
pub const FLOOR_CAP: f64 = 0.1;

/// Half-life of confirmations, accepts, uses, citations and
/// contradictions (§2.5).
pub const HALF_LIFE: u64 = 180 * DAY;
/// Half-life of votes.
pub const VOTE_HALF_LIFE: u64 = 30 * DAY;
/// A `still_valid` renewal restarts a confirmer's clock at most this often.
pub const RENEW_EVERY: u64 = 30 * DAY;
/// A confirmation decayed below this share of its weight (two half-lives,
/// 360 days) no longer confirms: the post is `stale` (§3.1).
pub const STALE_CUT: f64 = 0.25;

/// An attack on an argument counts only from a group weighing at least
/// this share of the argument's author's weight (§2.7).
pub const ATTACK_FACTOR: f64 = 0.5;
/// Contradictions are followed this deep from a post (§2.7).
pub const DISPUTE_DEPTH: usize = 3;

/// What each signal earns, times the giver's weight and its decay (§2.6).
pub const W_ACCEPT: f64 = 3.0;
pub const W_CONFIRM: f64 = 2.0;
pub const W_USE: f64 = 2.0;
pub const W_CITE: f64 = 1.0;
/// Lost per standing contradiction, times the contradictor's weight.
pub const W_CONTRADICTED: f64 = 2.0;
/// Earned per contradiction made that stands, times one's own weight.
pub const W_STANDS: f64 = 1.0;
/// Votes order posts in `search`; they do not make an author's score.
pub const W_VOTE: f64 = 0.5;

/// A table is rebuilt once this old even if the store did not change: it
/// depends on time too (decay, the stale cut, the floor's 7 to 30 days).
pub const MAX_AGE: Duration = Duration::from_secs(3600);

/// Default `reputation.threshold`: authors scoring below it are left out
/// of `search` and `wait` unless `include_low`. 0 hides only authors whose
/// standing contradictions outweigh everything else (pending Vincent).
pub const DEFAULT_THRESHOLD: f64 = 0.0;
/// A table is rebuilt when the store changed and it is older than this.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Most signal rows (replies and citations) a table is built from, the
/// newest; and most accept reacts.
const MAX_ROWS: usize = 200_000;

/// The reputation policy of a node (`[reputation]`).
#[derive(Debug, Clone)]
pub struct Policy {
    /// `reputation.threshold`.
    pub threshold: f64,
    /// How long a table is reused once the store changed.
    pub ttl: Duration,
    /// How long a table is reused at most, changed store or not.
    pub max_age: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            ttl: DEFAULT_TTL,
            max_age: MAX_AGE,
        }
    }
}

/// The proof of work a network requires, to tell spare work from required
/// work (`min_pow_bits` and the schedule of the node's config).
#[derive(Debug, Clone)]
pub struct PowPolicy {
    pub min_pow_bits: Option<u32>,
    pub schedule: Vec<(u64, u32)>,
}

impl PowPolicy {
    pub fn of(config: &crate::node::NodeConfig) -> Self {
        Self {
            min_pow_bits: config.min_pow_bits,
            schedule: config.pow_schedule.clone(),
        }
    }

    /// Bits an event of `size` bytes and time `ts` needed, from an author
    /// known for `known_for` seconds.
    fn required(&self, ts: u64, size: usize, known_for: u64) -> u32 {
        let base = self.min_pow_bits.unwrap_or_else(|| {
            crate::event::scheduled_pow_bits(&self.schedule, ts)
                .saturating_add(crate::event::new_author_extra_bits(Some(known_for)))
        });
        crate::event::required_pow_bits(base, size)
    }
}

/// `0.5^(age / half_life)`.
pub fn decay(age: u64, half_life: u64) -> f64 {
    0.5_f64.powf(age as f64 / half_life.max(1) as f64)
}

/// The age floor of a key first seen `age` seconds ago: 0 under
/// [`FLOOR_FROM`], then linear up to [`AGE_FLOOR_CAP`] at [`FLOOR_FULL`].
pub fn age_floor(age: u64) -> f64 {
    if age < FLOOR_FROM {
        return 0.0;
    }
    let rise = (age - FLOOR_FROM) as f64 / (FLOOR_FULL - FLOOR_FROM) as f64;
    AGE_FLOOR_CAP * rise.min(1.0)
}

/// The bonus for `work` units of spare proof of work, with diminishing
/// returns: always below [`POW_BONUS_CAP`].
pub fn pow_bonus(work: f64) -> f64 {
    let work = work.max(0.0);
    POW_BONUS_CAP * work / (work + POW_HALF_WORK)
}

/// The floor of a key first seen `age` seconds ago that spent `work` units
/// of spare proof of work: nothing under [`FLOOR_FROM`] (CPU buys no
/// weight to a fresh key), at most [`FLOOR_CAP`].
pub fn floor(age: u64, work: f64) -> f64 {
    if age < FLOOR_FROM {
        return 0.0;
    }
    (age_floor(age) + pow_bonus(work)).min(FLOOR_CAP)
}

/// What one author received and gave, for `wats reputation`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counts {
    pub posts: u64,
    pub accepts: u64,
    pub confirms: u64,
    pub uses: u64,
    pub cites: u64,
    /// Standing contradictions of its posts.
    pub contradicted: u64,
    /// Its contradictions that stand.
    pub stands: u64,
}

/// One key in a table.
#[derive(Debug, Clone, Default)]
pub struct Author {
    pub trust: f64,
    pub floor: f64,
    pub score: f64,
    /// When this node first stored an event of it in this network.
    pub first_seen: Option<u64>,
    /// Spare proof of work, in units of required work.
    pub work: f64,
    pub counts: Counts,
}

impl Author {
    pub fn weight(&self) -> f64 {
        self.trust + self.floor
    }
}

/// A network's reputation table, as this node sees it.
#[derive(Debug)]
pub struct Table {
    pub net: String,
    /// The store's last sequence number when built.
    pub seq: i64,
    built: Instant,
    /// Unix time the decays were computed at.
    pub now: u64,
    pub local: HashSet<String>,
    /// Key to its group's representative, for keys in a group of two or
    /// more (soft and hard groups).
    group: HashMap<String, String>,
    /// Key to the first key of its `rotate` chain, for keys in one.
    key: HashMap<String, String>,
    pub authors: HashMap<String, Author>,
    /// Signal rows it was built from.
    pub rows: usize,
}

impl Table {
    fn empty(net: &str, seq: i64, now: u64, local: &HashSet<String>) -> Self {
        Self {
            net: net.to_string(),
            seq,
            built: Instant::now(),
            now,
            local: local.clone(),
            group: HashMap::new(),
            key: HashMap::new(),
            authors: HashMap::new(),
            rows: 0,
        }
    }

    /// The representative of `key`'s group (itself when alone).
    pub fn group_of<'a>(&'a self, key: &'a str) -> &'a str {
        self.group.get(key).map_or(key, String::as_str)
    }

    /// The other keys of `key`'s group.
    pub fn group_size(&self, key: &str) -> usize {
        let rep = self.group_of(key);
        self.group.values().filter(|g| *g == rep).count().max(1)
    }

    /// The first key of `key`'s `rotate` chain (itself when in none).
    pub fn key_of<'a>(&'a self, key: &'a str) -> &'a str {
        self.key.get(key).map_or(key, String::as_str)
    }

    /// Whether `a` and `b` are one key: linked by `rotate` events.
    pub fn same_key(&self, a: &str, b: &str) -> bool {
        a == b || matches!((self.key.get(a), self.key.get(b)), (Some(x), Some(y)) if x == y)
    }

    pub fn weight(&self, key: &str) -> f64 {
        match self.authors.get(key) {
            Some(author) => author.weight(),
            None if self.local.contains(key) => SEED_TRUST,
            None => 0.0,
        }
    }

    pub fn score(&self, key: &str) -> f64 {
        self.authors.get(key).map_or(0.0, |a| a.score)
    }

    /// The same table with these authors.
    fn with_authors(self, authors: HashMap<String, Author>) -> Self {
        Self { authors, ..self }
    }

    /// The table out of its `Arc`, copied if still shared.
    fn unwrap(table: Arc<Table>) -> Table {
        Arc::try_unwrap(table).unwrap_or_else(|t| Table {
            net: t.net.clone(),
            seq: t.seq,
            built: t.built,
            now: t.now,
            local: t.local.clone(),
            group: t.group.clone(),
            key: t.key.clone(),
            authors: t.authors.clone(),
            rows: t.rows,
        })
    }
}

/// Union-find over keys.
#[derive(Default)]
struct Groups {
    index: HashMap<String, usize>,
    parent: Vec<usize>,
    keys: Vec<String>,
}

impl Groups {
    fn id(&mut self, key: &str) -> usize {
        if let Some(&i) = self.index.get(key) {
            return i;
        }
        let i = self.parent.len();
        self.parent.push(i);
        self.keys.push(key.to_string());
        self.index.insert(key.to_string(), i);
        i
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn root(&mut self, key: &str) -> usize {
        let i = self.id(key);
        self.find(i)
    }

    fn union(&mut self, a: &str, b: &str) {
        let (a, b) = (self.root(a), self.root(b));
        if a != b {
            self.parent[a] = b;
        }
    }

    /// Key to the smallest key of its group, for groups of two or more.
    fn representatives(mut self) -> HashMap<String, String> {
        let mut smallest: HashMap<usize, String> = HashMap::new();
        let mut members: HashMap<usize, usize> = HashMap::new();
        for i in 0..self.keys.len() {
            let root = self.find(i);
            *members.entry(root).or_default() += 1;
            let key = &self.keys[i];
            smallest
                .entry(root)
                .and_modify(|s| {
                    if key < s {
                        s.clone_from(key);
                    }
                })
                .or_insert_with(|| key.clone());
        }
        let mut out = HashMap::new();
        for i in 0..self.keys.len() {
            let root = self.find(i);
            if members[&root] > 1 {
                out.insert(self.keys[i].clone(), smallest[&root].clone());
            }
        }
        out
    }
}

/// Build the table of network `net` from the store, as seen by the keys
/// `local` (this node's profiles) at time `now`.
pub fn build(
    store: &Store,
    net: &str,
    local: &HashSet<String>,
    now: u64,
    pow: &PowPolicy,
) -> Result<Table> {
    let seq = store.last_seq()?;
    let mut table = Table::empty(net, seq, now, local);

    // When this node first stored an event of each key.
    let events = store.author_events(net)?;
    let mut authors: HashMap<String, Author> = HashMap::new();
    for event in &events {
        match authors.get_mut(&event.author) {
            Some(author) => {
                let first = author.first_seen.get_or_insert(event.received_at);
                *first = (*first).min(event.received_at);
            }
            None => {
                let author = Author {
                    first_seen: Some(event.received_at),
                    ..Author::default()
                };
                authors.insert(event.author.clone(), author);
            }
        }
    }

    // Groups (§2.4): rotations and this node's profiles (hard), declared
    // owners and vouch clusters (soft).
    let mut keys = Groups::default();
    let mut groups = Groups::default();
    for (old, new) in store.rotations()? {
        keys.union(&old, &new);
        groups.union(&old, &new);
    }
    let mut local_keys: Vec<&String> = local.iter().collect();
    local_keys.sort();
    for pair in local_keys.windows(2) {
        groups.union(pair[0], pair[1]);
    }
    let mut owners: HashMap<String, (u64, Option<String>)> = HashMap::new();
    for (author, ts, owner) in store.announce_owners(net)? {
        let latest = owners.entry(author).or_insert((ts, None));
        if ts >= latest.0 {
            *latest = (ts, owner);
        }
    }
    let mut by_owner: HashMap<String, Vec<String>> = HashMap::new();
    for (author, (_, owner)) in owners {
        let owner = owner.map(|o| o.trim().to_lowercase()).unwrap_or_default();
        if !owner.is_empty() {
            by_owner.entry(owner).or_default().push(author);
        }
    }
    for members in by_owner.values() {
        for pair in members.windows(2) {
            groups.union(&pair[0], &pair[1]);
        }
    }

    let rows = store.net_signal_rows(net, MAX_ROWS)?;
    table.rows = rows.len();
    let mut heads: HashMap<String, PostRef> = HashMap::new();
    for (row, head) in &rows {
        let Some(head) = head else { continue };
        if !heads.contains_key(&row.target) {
            heads.insert(
                row.target.clone(),
                PostRef {
                    id: row.target.clone(),
                    author: head.author.clone(),
                    net: net.to_string(),
                    thread: head.thread.clone(),
                    root_ask: head.thread.is_none() && head.kind == "ask",
                },
            );
        }
    }
    // Keys vouched only by one key, or only by keys of one group, join
    // that group, until nothing changes (bounded); only keys vouched for
    // within [`FLOOR_FROM`] of their first sighting, as keys minted by
    // their voucher are, so that a stranger's vouch cannot pull an
    // established key into its group. Not this node's own profiles: their
    // group is known.
    let vouches: Vec<(String, String, u64)> = store
        .vouches(net)?
        .into_iter()
        .filter(|(giver, to, _)| giver != to)
        .collect();
    let mut vouchers: HashMap<&String, (HashSet<&String>, bool)> = HashMap::new();
    for (giver, to, received_at) in &vouches {
        let first = authors.get(to).and_then(|a| a.first_seen);
        let young = first.is_some_and(|f| *received_at < f.saturating_add(FLOOR_FROM));
        let entry = vouchers.entry(to).or_default();
        entry.0.insert(giver);
        entry.1 |= young;
    }
    let mut vouched: Vec<(&String, &HashSet<&String>)> = vouchers
        .iter()
        .filter(|(key, (_, young))| *young && !local.contains(**key))
        .map(|(key, (by, _))| (*key, by))
        .collect();
    vouched.sort_by_key(|(key, _)| *key);
    for _ in 0..4 {
        let mut changed = false;
        for (key, by) in &vouched {
            let mut roots = HashSet::new();
            for voucher in by.iter() {
                roots.insert(groups.root(voucher));
            }
            if roots.len() == 1
                && let Some(&root) = roots.iter().next()
                && root != groups.root(key)
            {
                if let Some(voucher) = by.iter().next() {
                    groups.union(key, voucher);
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    table.key = keys.representatives();
    table.group = groups.representatives();

    // Signals, positions and accepts, with groups but no weights yet.
    let signal_rows: Vec<_> = rows.into_iter().map(|(row, _)| row).collect();
    let rows = Rows::complete(signal_rows);
    let accept_rows = store.accept_reacts_in(net, MAX_ROWS)?;
    let asks = store.ask_roots(net)?;
    let mut targets: Vec<&PostRef> = heads.values().collect();
    targets.sort_by(|a, b| a.id.cmp(&b.id));

    let grouped = Arc::new(table);
    let grouping = Grouping::new(local.clone()).with_table(grouped.clone());
    let threads = Threads::from_rows(
        asks.iter()
            .map(|(thread, asker)| (thread.clone(), asker.clone(), net.to_string())),
        accept_rows,
        &grouping,
    );

    // Personal trust (§2.2): seeds, then a personalised PageRank along
    // vouches, accepts, uses and confirmations with evidence. What makes a
    // seed (this node's vouches, accepts and uses) is not also an edge:
    // the seed already holds that trust.
    let mut seeds: HashMap<String, f64> = local.iter().map(|k| (k.clone(), SEED_TRUST)).collect();
    let mut edges: HashMap<String, HashMap<String, f64>> = HashMap::new();
    for (giver, to, _) in &vouches {
        if grouping.same_key(net, giver, to) {
            continue;
        }
        if local.contains(giver) {
            seeds.insert(to.clone(), SEED_TRUST);
            continue;
        }
        *edges
            .entry(giver.clone())
            .or_default()
            .entry(to.clone())
            .or_default() += 1.0;
    }
    for post in &targets {
        for endorsement in crate::signals::endorsements(post, &rows, &threads, &grouping, now) {
            // Implicit trust: what this node's profiles accepted or used.
            if endorsement.seeds && local.contains(&endorsement.giver) {
                let seed = seeds.entry(post.author.clone()).or_default();
                *seed = seed.max(endorsement.weight.min(SEED_TRUST));
                continue;
            }
            *edges
                .entry(endorsement.giver.clone())
                .or_default()
                .entry(post.author.clone())
                .or_default() += endorsement.weight;
        }
    }
    let trust = personalised_pagerank(&seeds, &edges);

    // The floor (§2.3): age and spare proof of work.
    for event in &events {
        let Some(author) = authors.get_mut(&event.author) else {
            continue;
        };
        let first = author.first_seen.unwrap_or(event.received_at);
        let required = pow.required(
            event.ts,
            event.size,
            event.received_at.saturating_sub(first),
        );
        if event.pow_bits > required {
            let spare = (event.pow_bits - required).min(30);
            author.work += ((1u64 << spare) - 1) as f64;
        }
    }
    drop(events);
    for (key, author) in &mut authors {
        if let Some(first) = author.first_seen {
            author.floor = floor(now.saturating_sub(first), author.work);
        }
        author.trust = trust.get(key).copied().unwrap_or(0.0);
    }
    for (key, value) in &trust {
        authors.entry(key.clone()).or_default().trust = *value;
    }

    // Scores (§2.6), with the weights.
    drop(grouping);
    let weighted = Arc::new(Table::unwrap(grouped).with_authors(authors));
    let grouping = Grouping::new(local.clone()).with_table(weighted.clone());
    let mut scores: HashMap<String, (f64, Counts)> = HashMap::new();
    for post in &targets {
        let outcome = crate::signals::evaluate(post, &rows, &threads, &grouping, now);
        let s = &outcome.signals;
        let (score, counts) = scores.entry(post.author.clone()).or_default();
        *score += outcome.credit;
        counts.posts += 1;
        counts.accepts += s.accepts;
        counts.confirms += s.confirms;
        counts.uses += s.uses;
        counts.cites += s.cites;
        counts.contradicted += s.contradicts;
        for (key, earned) in outcome.stands {
            let (score, counts) = scores.entry(key).or_default();
            *score += earned;
            counts.stands += 1;
        }
    }
    drop(grouping);
    let mut table = Table::unwrap(weighted);
    for (key, (score, counts)) in scores {
        let author = table.authors.entry(key).or_default();
        author.score = score;
        author.counts = counts;
    }
    table.built = Instant::now();
    Ok(table)
}

/// Trust from the seeds (§2.2): a personalised PageRank, truncated at
/// [`TRUST_DEPTH`] steps. Each key passes [`DAMPING`] of its trust on,
/// shared among the keys it endorses in proportion to the endorsements;
/// a key's trust is what reaches it, at most [`SEED_TRUST`].
fn personalised_pagerank(
    seeds: &HashMap<String, f64>,
    edges: &HashMap<String, HashMap<String, f64>>,
) -> HashMap<String, f64> {
    let mut total = seeds.clone();
    let mut step = seeds.clone();
    for _ in 0..TRUST_DEPTH {
        let mut next: HashMap<String, f64> = HashMap::new();
        for (giver, trust) in &step {
            let Some(out) = edges.get(giver) else {
                continue;
            };
            let sum: f64 = out.values().sum();
            if sum <= 0.0 || *trust <= 0.0 {
                continue;
            }
            for (to, weight) in out {
                *next.entry(to.clone()).or_default() += DAMPING * trust * weight / sum;
            }
        }
        for (key, trust) in &next {
            *total.entry(key.clone()).or_default() += trust;
        }
        step = next;
    }
    for trust in total.values_mut() {
        *trust = trust.min(SEED_TRUST);
    }
    total
}

/// The tables of a node, per network. A table is rebuilt when the store
/// changed and it is older than the policy's TTL, or when it is older than
/// its max age (time alone moves decay and floors): in the background,
/// while readers keep the previous one, so that no query waits for a
/// rebuild (about 0.2 s at 50,000 events). Only the first table of a
/// network, a change of the node's profiles, or a zero TTL (tests) rebuild
/// before answering.
#[derive(Debug, Default)]
pub struct Cache {
    tables: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Debug)]
struct Entry {
    table: Arc<Table>,
    rebuilding: bool,
}

impl Cache {
    pub fn get(
        &self,
        store: &Arc<Store>,
        net: &str,
        local: &HashSet<String>,
        pow: &PowPolicy,
        policy: &Policy,
    ) -> Result<Arc<Table>> {
        let ttl = policy.ttl;
        let seq = store.last_seq()?;
        {
            let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = tables.get_mut(net)
                && entry.table.local == *local
            {
                let table = entry.table.clone();
                let age = table.built.elapsed();
                if age < policy.max_age && (table.seq == seq || age < ttl) {
                    return Ok(table);
                }
                if !ttl.is_zero() {
                    if !entry.rebuilding {
                        entry.rebuilding = true;
                        self.rebuild(store.clone(), net, local.clone(), pow.clone());
                    }
                    return Ok(table);
                }
            }
        }
        let table = Arc::new(build(store, net, local, crate::node::now_secs(), pow)?);
        self.tables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                net.to_string(),
                Entry {
                    table: table.clone(),
                    rebuilding: false,
                },
            );
        Ok(table)
    }

    /// Rebuild a network's table on a thread of its own, then swap it in.
    fn rebuild(&self, store: Arc<Store>, net: &str, local: HashSet<String>, pow: PowPolicy) {
        let tables = self.tables.clone();
        let name = net.to_string();
        let net = net.to_string();
        let spawned = std::thread::Builder::new()
            .name("wats-reputation".into())
            .spawn(move || {
                let built = build(&store, &net, &local, crate::node::now_secs(), &pow);
                let mut tables = tables.lock().unwrap_or_else(|e| e.into_inner());
                match built {
                    Ok(table) => {
                        tables.insert(
                            net,
                            Entry {
                                table: Arc::new(table),
                                rebuilding: false,
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!("reputation of {net} not rebuilt: {e:#}");
                        if let Some(entry) = tables.get_mut(&net) {
                            entry.rebuilding = false;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("reputation not rebuilt: {e}");
            if let Some(entry) = self
                .tables
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&name)
            {
                entry.rebuilding = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_age_floor_starts_at_seven_days_and_caps_at_thirty() {
        assert_eq!(age_floor(0), 0.0);
        assert_eq!(age_floor(7 * DAY - 1), 0.0);
        assert_eq!(age_floor(7 * DAY), 0.0);
        let mid = age_floor(7 * DAY + 23 * DAY / 2);
        assert!((mid - AGE_FLOOR_CAP / 2.0).abs() < 1e-9, "{mid}");
        assert_eq!(age_floor(30 * DAY), AGE_FLOOR_CAP);
        assert_eq!(age_floor(400 * DAY), AGE_FLOOR_CAP);
        // Linear in between.
        let a = age_floor(10 * DAY);
        let b = age_floor(20 * DAY);
        let c = age_floor(30 * DAY);
        assert!(((b - a) - (c - b)).abs() < 1e-9);
    }

    #[test]
    fn the_pow_bonus_stays_below_the_age_floor_cap() {
        assert_eq!(pow_bonus(0.0), 0.0);
        let mut last = 0.0;
        for work in [1.0, 10.0, 64.0, 1e3, 1e6, 1e12, f64::MAX / 2.0] {
            let bonus = pow_bonus(work);
            assert!(bonus >= last, "diminishing, never decreasing");
            assert!(bonus <= POW_BONUS_CAP);
            assert!(bonus < AGE_FLOOR_CAP);
            last = bonus;
        }
        assert!((pow_bonus(POW_HALF_WORK) - POW_BONUS_CAP / 2.0).abs() < 1e-12);
        // No floor at all under 7 days, however much work.
        assert_eq!(floor(6 * DAY, 1e12), 0.0);
        // Capped, well below a seed's trust.
        assert!(floor(400 * DAY, 1e12) <= FLOOR_CAP);
        const { assert!(FLOOR_CAP <= SEED_TRUST / 10.0) };
    }

    #[test]
    fn decay_halves_at_each_half_life() {
        assert_eq!(decay(0, HALF_LIFE), 1.0);
        assert!((decay(HALF_LIFE, HALF_LIFE) - 0.5).abs() < 1e-12);
        assert!((decay(2 * HALF_LIFE, HALF_LIFE) - STALE_CUT).abs() < 1e-12);
        assert!((decay(30 * DAY, VOTE_HALF_LIFE) - 0.5).abs() < 1e-12);
        assert!((decay(60 * DAY, VOTE_HALF_LIFE) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn the_cache_rebuilds_without_making_readers_wait() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let note = |i: u64| {
            crate::event::Draft {
                net: "public".into(),
                kind: Some(crate::event::Kind::Note),
                title: Some(format!("note {i}")),
                text: format!("note {i}"),
                ..Default::default()
            }
            .sign(&ed25519_dalek::SigningKey::from_bytes(&[7; 32]), i, 0)
            .unwrap()
        };
        store.insert(&note(1), 1).unwrap();
        let cache = Cache::default();
        let local = HashSet::new();
        let pow = PowPolicy {
            min_pow_bits: None,
            schedule: crate::event::POW_SCHEDULE.to_vec(),
        };
        let policy = |ttl: Duration, max_age: Duration| Policy {
            threshold: DEFAULT_THRESHOLD,
            ttl,
            max_age,
        };
        let hour = Duration::from_secs(3600);
        let first = cache
            .get(&store, "public", &local, &pow, &policy(hour, hour))
            .unwrap();
        // Unchanged and younger than its max age: the same table.
        let again = cache
            .get(
                &store,
                "public",
                &local,
                &pow,
                &policy(Duration::ZERO, hour),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        // Changed, but younger than the TTL: still the same.
        store.insert(&note(2), 2).unwrap();
        let young = cache
            .get(&store, "public", &local, &pow, &policy(hour, hour))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &young));
        // Older than the TTL: the stale table now, a new one soon after.
        std::thread::sleep(Duration::from_millis(5));
        let ms = Duration::from_millis(1);
        let stale = cache
            .get(&store, "public", &local, &pow, &policy(ms, hour))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &stale));
        let rebuilt = |cache: &Cache, than: &Arc<Table>| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let table = cache
                    .get(&store, "public", &local, &pow, &policy(hour, hour))
                    .unwrap();
                if !Arc::ptr_eq(&table, than) {
                    return table;
                }
                assert!(Instant::now() < deadline, "never rebuilt");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let second = rebuilt(&cache, &first);
        assert_eq!(second.seq, store.last_seq().unwrap());
        // Unchanged store, but older than its max age: time moves decay
        // and floors, so it is rebuilt too, in the background.
        std::thread::sleep(Duration::from_millis(5));
        let old = cache
            .get(&store, "public", &local, &pow, &policy(hour, ms))
            .unwrap();
        assert!(Arc::ptr_eq(&second, &old));
        let third = rebuilt(&cache, &second);
        assert_eq!(third.seq, second.seq);
        assert!(third.now >= second.now);
        // A zero TTL rebuilds a changed store before answering.
        store.insert(&note(3), 3).unwrap();
        let now = cache
            .get(
                &store,
                "public",
                &local,
                &pow,
                &policy(Duration::ZERO, hour),
            )
            .unwrap();
        assert_eq!(now.seq, store.last_seq().unwrap());
        // So does a change of the node's profiles.
        let mine: HashSet<String> = ["me".to_string()].into();
        let theirs = cache
            .get(&store, "public", &mine, &pow, &policy(hour, hour))
            .unwrap();
        assert!(theirs.local.contains("me"));
    }

    #[test]
    fn a_table_built_later_ages_keys_and_decays_signals() {
        // The same store, read at two times: time alone lifts a key's
        // floor (what the max age of the cache is for).
        let store = Store::open_in_memory().unwrap();
        let t0 = 1_700_000_000;
        let event = crate::event::Draft {
            net: "public".into(),
            kind: Some(crate::event::Kind::Note),
            title: Some("hi".into()),
            text: "hi".into(),
            ..Default::default()
        }
        .sign(&ed25519_dalek::SigningKey::from_bytes(&[9; 32]), t0, 0)
        .unwrap();
        store.insert(&event, t0).unwrap();
        let pow = PowPolicy {
            min_pow_bits: Some(256),
            schedule: Vec::new(),
        };
        let local = HashSet::new();
        let floor_at = |now: u64| {
            build(&store, "public", &local, now, &pow).unwrap().authors[&event.author].floor
        };
        assert_eq!(floor_at(t0 + DAY), 0.0);
        assert_eq!(floor_at(t0 + 30 * DAY), AGE_FLOOR_CAP);
    }

    #[test]
    fn trust_flows_three_steps_from_the_seeds() {
        let seeds: HashMap<String, f64> = [("me".to_string(), 1.0)].into();
        let mut edges: HashMap<String, HashMap<String, f64>> = HashMap::new();
        for (from, to) in [("me", "a"), ("a", "b"), ("b", "c"), ("c", "d")] {
            edges.entry(from.into()).or_default().insert(to.into(), 1.0);
        }
        // A clique nobody points into.
        for (from, to) in [("x", "y"), ("y", "x"), ("y", "z"), ("z", "x")] {
            edges.entry(from.into()).or_default().insert(to.into(), 1.0);
        }
        let trust = personalised_pagerank(&seeds, &edges);
        assert_eq!(trust["me"], 1.0);
        assert!((trust["a"] - DAMPING).abs() < 1e-12);
        assert!((trust["c"] - DAMPING.powi(3)).abs() < 1e-12);
        assert!(!trust.contains_key("d"), "depth 4: {trust:?}");
        for sybil in ["x", "y", "z"] {
            assert!(!trust.contains_key(sybil));
        }
        // Shared among the keys one endorses.
        edges.get_mut("me").unwrap().insert("e".into(), 1.0);
        let trust = personalised_pagerank(&seeds, &edges);
        assert!((trust["a"] - DAMPING / 2.0).abs() < 1e-12);
    }
}
