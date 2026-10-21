//! Rotation at the cap (ADR 0025 §7, PR B).
//!
//! When the store bytes reach [`ROTATE_AT_PERCENT`] of the events share,
//! the node frees space until they are back under [`ROTATE_TO_PERCENT`],
//! in the background, in transactions of at most [`BATCH`] rows:
//!
//! 0. **Keys over their share**, per network. A key (with its rotation
//!    chain) holding more than the larger of [`KEY_SHARE_PERCENT`] of the
//!    network's bytes and [`KEY_SHARE_AVERAGE`] times a key's average there
//!    loses rows down to it: those older than a week, lowest value first.
//!    The cut does not move: as with the fill's per-key share, these are
//!    skipped events.
//! 1. **Bodies, lowest value first** (reason `evicted`): `contested`, then
//!    `unverified`, `stale` and `confirmed`, then the post's score, then
//!    older first, knowledge entries last. Only bodies that save at least
//!    1 KiB, never a `react` (its body is its signal), and only posts
//!    older than a week ([`BODY_GRACE_SECS`]).
//! 2. **The cut rises.** Whole rows go, oldest first, and the rotation cut
//!    of their network rises to the newest one deleted, in the same
//!    transaction. Protected rows stay as islands below it. Never below a
//!    range still being fetched, nor in a network whose history task has
//!    not run yet.
//!
//! Never dropped, in any stage ([`crate::store::Keep`]): posts of this
//! node's profiles, their ancestors and the direct replies to them, the
//! human's asks (`ask_notify`), the kept-forever kinds and erasure labels.
//!
//! Each batch is one transaction: a crash leaves the store as after the
//! last batch, and the next check (every 30 s) starts again. Blobs that no
//! kept post lists any more are recorded in that transaction; after it,
//! each is kept if this node holds it for a reason (a replica pushed here,
//! its own, or responsible by its estimate), untagged otherwise. All the
//! store work runs on blocking threads. A run that frees almost nothing
//! backs off, from 30 s up to an hour.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use super::{Node, now_secs};
use crate::event::Event;
use crate::signals::Grouping;
use crate::store::{DropReason, EVICT_MIN_SAVING, Keep, Rotated, RotationLog, RowHead, Store};

/// Rotation starts when the store bytes reach this share of the events
/// share, and runs until they are back under the next one.
pub const ROTATE_AT_PERCENT: u64 = 95;
pub const ROTATE_TO_PERCENT: u64 = 85;
/// Most rows per transaction.
pub const BATCH: usize = 500;
/// Bodies per transaction of stage 1, which measures the store after each.
const BODIES_BATCH: usize = 100;
/// Stages 0 and 1 leave posts younger than this alone: a new post has not
/// had the time to earn its signals.
pub const BODY_GRACE_SECS: u64 = 7 * 24 * 3600;
/// Stage 0: a key, with its rotation chain, may hold this share of a
/// network's bytes...
pub const KEY_SHARE_PERCENT: u64 = 2;
/// ...or this many times the average per key there, whichever is larger,
/// before rotation trims it first.
pub const KEY_SHARE_AVERAGE: u64 = 3;
/// Most rows one run ranks by value, per stage or key: the oldest ones,
/// beyond.
const MAX_PLAN: usize = 200_000;
/// Rows read per step of a scan by `seq`, and per page by time.
const SEQ_STEP: i64 = 5_000;
const SCAN: usize = 2_000;
/// Stage 2 stops a run once the rows it deleted weigh, by the store's
/// average, this many times what was to be freed: freed pages can lag
/// (the full-text index merges its deletions later).
const OVERSHOOT: u64 = 2;
/// A run that frees less than this share of what it had to free is
/// unproductive: the next one waits, from the first delay, doubling up to
/// the last, unless the store grew by [`REGROW_PERCENT`] of its share.
const PRODUCTIVE_PERCENT: u64 = 1;
const BACKOFF_FIRST: Duration = Duration::from_secs(30);
/// A post kept as a possible last copy is not tried again for this long,
/// plus up to a quarter more (spread by its id).
const LAST_COPY_RETRY: Duration = Duration::from_secs(3600);
/// Most evictions through the node (each may ask peers for proofs) in one
/// run; the store's own batches have no such cost.
const MAX_NODE_EVICTIONS: usize = 200;
const BACKOFF_MAX: Duration = Duration::from_secs(3600);
const REGROW_PERCENT: u64 = 1;

/// The rotation of a node: one run at a time, and the backoff after runs
/// that freed nothing.
#[derive(Debug, Default)]
pub struct Rotation {
    running: AtomicBool,
    /// The run in progress.
    current: std::sync::Mutex<Option<Value>>,
    backoff: std::sync::Mutex<Backoff>,
    /// Posts whose eviction was refused (this node may hold the last copy
    /// of a file), not tried again before then (ADR 0029).
    kept_last: std::sync::Mutex<HashMap<String, Instant>>,
    /// Evictions through the node in the run going on.
    node_evictions: std::sync::atomic::AtomicUsize,
}

/// What [`Node::evict_through_node`] did with a post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eviction {
    /// The body went.
    Dropped,
    /// It stays: protected, a stronger reason, a possible last copy, an
    /// error (logged).
    Kept,
    /// Not tried: the run's evictions through the node are used up.
    Capped,
}

#[derive(Debug, Default)]
struct Backoff {
    /// Unproductive runs in a row.
    step: u32,
    /// No run before this, unless the store grows.
    until: Option<Instant>,
    /// The store bytes after the last unproductive run.
    bytes: u64,
}

/// Clears `running` when a run ends, whatever the way.
struct Running<'a>(&'a Rotation);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Ok(mut current) = self.0.current.lock() {
            *current = None;
        }
        self.0.running.store(false, Ordering::Release);
    }
}

/// The value order of stages 0 and 1 (ADR 0025 §7): lowest first.
#[derive(Debug, Clone, PartialEq)]
struct Value1 {
    /// A knowledge entry: last.
    entry: bool,
    /// [`crate::signals::Status::rank`]: contested 0, unverified 1, stale
    /// 2, confirmed 3.
    status: u8,
    score: f64,
    ts: u64,
    id: String,
}

fn by_value(a: &Value1, b: &Value1) -> std::cmp::Ordering {
    a.entry
        .cmp(&b.entry)
        .then(a.status.cmp(&b.status))
        .then(a.score.total_cmp(&b.score))
        .then(a.ts.cmp(&b.ts))
        .then(a.id.cmp(&b.id))
}

/// `part` percent of `of`.
fn percent(of: u64, part: u64) -> u64 {
    of.saturating_mul(part) / 100
}

/// Run `f` on a blocking thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

/// The `rotation` line of `wats status`, from [`Node::rotation_json`].
pub fn summary(rotation: &Value) -> String {
    let mut text = match rotation["state"].as_str() {
        Some("running") => format!(
            "running ({})",
            rotation["stage"].as_str().unwrap_or("starting")
        ),
        _ => "idle".to_string(),
    };
    text.push_str(&format!(
        " · from {} % down to {} % of the events share",
        rotation["at_percent"], rotation["to_percent"]
    ));
    if let Some(secs) = rotation["backoff_secs"].as_u64() {
        text.push_str(&format!(
            " · backing off: the last run freed almost nothing, next try in {secs} s \
             or once the store grows"
        ));
    }
    let last = &rotation["last"];
    if let Some(at) = last["at"].as_u64() {
        let n = |k: &str| last[k].as_u64().unwrap_or(0);
        text.push_str(&format!(
            " · last {}: {} bodies dropped, {} rows of keys over their share and {} old rows deleted, {} → {}",
            crate::runs::format_time(at),
            n("bodies"),
            n("trimmed"),
            n("rows"),
            crate::node::format_bytes(n("before")),
            crate::node::format_bytes(n("after")),
        ));
    }
    text
}

/// A network's bytes in all, and per rotation chain with its keys.
type NetBytes = (u64, HashMap<String, (u64, HashSet<String>)>);

/// A key over its share in a network (stage 0).
#[derive(Debug, Clone)]
struct Over {
    net: String,
    /// The keys of its rotation chain.
    keys: Vec<String>,
    bytes: u64,
    limit: u64,
}

/// The keys over their share, per network, largest excess first: a key
/// may hold the larger of [`KEY_SHARE_PERCENT`] of the network's bytes and
/// [`KEY_SHARE_AVERAGE`] times the network's average per key. This node's
/// own keys are never in it.
fn keys_over_share(store: &Store, keep: &Keep) -> Result<Vec<Over>> {
    let chains = super::history::chains(&store.rotations()?);
    // Per network: bytes per chain (with its keys), and in all.
    let mut nets: HashMap<String, NetBytes> = HashMap::new();
    let max = store.max_seq()?;
    let mut from = 0;
    while from < max {
        let to = from.saturating_add(SEQ_STEP);
        for row in store.rows_in_seq(from, to)? {
            let chain = chains.get(&row.author).unwrap_or(&row.author).clone();
            let (total, keys) = nets.entry(row.net.clone()).or_default();
            *total = total.saturating_add(row.len);
            let entry = keys.entry(chain).or_default();
            entry.0 = entry.0.saturating_add(row.len);
            entry.1.insert(row.author);
        }
        from = to;
    }
    let mut over = Vec::new();
    for (net, (total, keys)) in nets {
        let active = keys.len().max(1) as u64;
        let limit = percent(total, KEY_SHARE_PERCENT).max(
            total
                .saturating_mul(KEY_SHARE_AVERAGE)
                .checked_div(active)
                .unwrap_or(0),
        );
        for (bytes, members) in keys.into_values() {
            if bytes > limit && !members.iter().any(|k| keep.authors.contains(k)) {
                let mut members: Vec<String> = members.into_iter().collect();
                members.sort();
                over.push(Over {
                    net: net.clone(),
                    keys: members,
                    bytes,
                    limit,
                });
            }
        }
    }
    over.sort_by(|a, b| {
        (b.bytes.saturating_sub(b.limit))
            .cmp(&a.bytes.saturating_sub(a.limit))
            .then(a.keys.cmp(&b.keys))
    });
    Ok(over)
}

/// The value of each of these full events (stages 0 and 1).
fn values(store: &Store, grouping: &Grouping, events: &[&Event], now: u64) -> Result<Vec<Value1>> {
    let signals = crate::signals::compute_at(store, grouping, events, now)?;
    let mut roots: Vec<String> = events.iter().map(|e| e.thread_root().to_string()).collect();
    roots.sort();
    roots.dedup();
    let entries = crate::knowledge::compute_at(store, grouping, &roots, &signals, now)?.ids();
    Ok(events
        .iter()
        .map(|event| {
            let s = signals.get(&event.id).cloned().unwrap_or_default();
            Value1 {
                entry: entries.contains(&event.id),
                status: s.status.rank(),
                score: s.score,
                ts: event.ts,
                id: event.id.clone(),
            }
        })
        .collect())
}

/// Stage 0's order within a key: rows older than the grace and not
/// strictly protected, tombstones and opaque rows first (oldest first), then full
/// events lowest value first. Returns `(id, bytes)`.
fn trim_order(
    store: &Store,
    grouping: &Grouping,
    keep: &Keep,
    over: &Over,
    now: u64,
) -> Result<Vec<(String, u64)>> {
    let grace = now.saturating_sub(BODY_GRACE_SECS);
    let mut rows: Vec<RowHead> = Vec::new();
    let mut after = (0u64, String::new());
    loop {
        let page = store.author_rows_after(&over.net, &over.keys, (after.0, &after.1), SCAN)?;
        let Some(last) = page.last() else { break };
        after = (last.ts, last.id.clone());
        let done = page.len() < SCAN;
        for row in page {
            if row.ts >= grace {
                break;
            }
            if !keep.protects_strictly(&row) {
                rows.push(row);
            }
        }
        if done || after.0 >= grace || rows.len() >= MAX_PLAN {
            break;
        }
    }
    let len: HashMap<String, u64> = rows.iter().map(|r| (r.id.clone(), r.len)).collect();
    let (opaque, full): (Vec<RowHead>, Vec<RowHead>) = rows.into_iter().partition(|r| r.opaque);
    let mut out: Vec<(String, u64)> = opaque.into_iter().map(|r| (r.id, r.len)).collect();
    let mut ranked = Vec::new();
    for chunk in full.chunks(BATCH) {
        let seqs: Vec<i64> = chunk.iter().map(|r| r.seq).collect();
        let stored = store.full_events_by_seq(&seqs)?;
        let events: Vec<&Event> = stored.iter().map(|s| &*s.event).collect();
        ranked.extend(values(store, grouping, &events, now)?);
    }
    ranked.sort_by(by_value);
    out.extend(ranked.into_iter().map(|v| {
        let bytes = len.get(&v.id).copied().unwrap_or(0);
        (v.id, bytes)
    }));
    Ok(out)
}

/// The bodies stage 1 may drop, lowest value first (ADR 0025 §7): full
/// events older than [`BODY_GRACE_SECS`] whose body saves at least
/// [`EVICT_MIN_SAVING`], not `react`s, not protected; judged with
/// `grouping`. The candidates are chosen in SQL by the size of their title,
/// text and tags, and at most the [`MAX_PLAN`] oldest are ranked: the
/// candidates take 16 bytes each while scanning, about 130 once ranked.
pub fn plan(store: &Store, grouping: &Grouping, keep: &Keep, now: u64) -> Result<Vec<String>> {
    let grace = now.saturating_sub(BODY_GRACE_SECS);
    // The body saves at most what its title, text and tags hold, plus its
    // data and blobs; this bounds the search, the exact check comes later.
    let min_body = EVICT_MIN_SAVING / 2;
    let mut candidates: Vec<(u64, i64)> = Vec::new();
    let max = store.max_seq()?;
    let mut from = 0;
    while from < max {
        let to = from.saturating_add(SEQ_STEP);
        candidates.extend(
            store
                .body_candidates(from, to, grace, min_body)?
                .into_iter()
                .filter(|r| !keep.protects(r))
                .map(|r| (r.ts, r.seq)),
        );
        if candidates.len() > 2 * MAX_PLAN {
            candidates.sort_unstable();
            candidates.truncate(MAX_PLAN);
        }
        from = to;
    }
    candidates.sort_unstable();
    candidates.truncate(MAX_PLAN);
    let mut ranked: Vec<Value1> = Vec::new();
    for chunk in candidates.chunks(BATCH) {
        let seqs: Vec<i64> = chunk.iter().map(|(_, seq)| *seq).collect();
        let stored = store.full_events_by_seq(&seqs)?;
        // Exactly what the body saves: the full event against its
        // tombstone.
        let events: Vec<&Event> = stored
            .iter()
            .filter(|s| {
                let full = s.event.to_json();
                serde_json::to_value(&*s.event)
                    .ok()
                    .and_then(|v| crate::wire::tombstone(&v).ok())
                    .and_then(|t| serde_json::to_string(&t.wire).ok())
                    .is_some_and(|t| {
                        full.len() as u64 >= (t.len() as u64).saturating_add(EVICT_MIN_SAVING)
                    })
            })
            .map(|s| &*s.event)
            .collect();
        ranked.extend(values(store, grouping, &events, now)?);
    }
    ranked.sort_by(by_value);
    Ok(ranked.into_iter().map(|v| v.id).collect())
}

/// Stage 2's walk over the networks, oldest rows first: per network, where
/// it reads from (its cut), what it may not go below (its ranges still
/// being fetched), and the rows read not taken yet.
struct Cutter {
    floors: HashMap<String, u64>,
    cursors: HashMap<String, Option<(u64, String)>>,
    buffers: HashMap<String, VecDeque<RowHead>>,
}

impl Cutter {
    /// For the networks this store holds rows of: below what each one's
    /// rows may go (under its ranges still being fetched; nothing in a
    /// network whose history task has not run yet, as a cut would stop
    /// its fill), read from its cut up.
    fn new(store: &Store, joined: &HashSet<String>) -> Result<Self> {
        let mut floors = HashMap::new();
        let mut cursors = HashMap::new();
        for net in store.nets_held()? {
            let ranges = store.sync_ranges(&net)?;
            let floor =
                if joined.contains(&net) && ranges.is_empty() && store.sync_watermark(&net)? == 0 {
                    0
                } else {
                    ranges
                        .iter()
                        .filter(|r| r.state == crate::store::RangeState::Pending)
                        .map(|r| r.lo_ts)
                        .min()
                        .unwrap_or(u64::MAX)
                };
            if floor > 0 {
                let from = store.cut_ts(&net)?.unwrap_or(0);
                cursors.insert(net.clone(), Some((from, String::new())));
            }
            floors.insert(net, floor);
        }
        Ok(Self {
            floors,
            cursors,
            buffers: HashMap::new(),
        })
    }

    /// The next rows to delete, at most [`BATCH`], oldest first across the
    /// networks.
    fn next_batch(&mut self, store: &Store, keep: &Keep) -> Result<Vec<RowHead>> {
        let mut batch: Vec<RowHead> = Vec::new();
        while batch.len() < BATCH {
            for (net, cursor) in self.cursors.iter_mut() {
                let buffer = self.buffers.entry(net.clone()).or_default();
                while buffer.is_empty() {
                    let Some(after) = cursor.clone() else { break };
                    let rows = store.net_rows_after(net, (after.0, &after.1), SCAN)?;
                    let floor = self.floors.get(net).copied().unwrap_or(0);
                    match rows.last() {
                        None => *cursor = None,
                        Some(last) => *cursor = Some((last.ts, last.id.clone())),
                    }
                    for row in rows {
                        if row.ts >= floor {
                            *cursor = None;
                            break;
                        }
                        if !keep.protects(&row) {
                            buffer.push_back(row);
                        }
                    }
                }
            }
            let next = self
                .buffers
                .iter()
                .filter_map(|(net, b)| b.front().map(|r| ((r.ts, r.id.clone()), net.clone())))
                .min();
            let Some((_, net)) = next else { break };
            if let Some(row) = self.buffers.get_mut(&net).and_then(|b| b.pop_front()) {
                batch.push(row);
            }
        }
        Ok(batch)
    }
}

impl Node {
    /// The store bytes at which rotation starts, and those it frees down
    /// to; `None` without an events share (no `storage.max`).
    pub fn rotation_bounds(&self) -> Option<(u64, u64)> {
        let share = self.events_share()?;
        Some((
            percent(share, ROTATE_AT_PERCENT),
            percent(share, ROTATE_TO_PERCENT),
        ))
    }

    /// The rotation of `status` (ADR 0025 §7): whether one runs, its
    /// bounds, the last run, and whether runs are backing off.
    pub fn rotation_json(&self) -> Value {
        let current = self.rotation.current.lock().ok().and_then(|c| c.clone());
        let last = self.store.last_rotation().ok().flatten();
        let backoff = self.rotation.backoff.lock().ok().and_then(|b| {
            b.until
                .map(|until| until.saturating_duration_since(Instant::now()).as_secs())
        });
        json!({
            "state": if current.is_some() { "running" } else { "idle" },
            "stage": current.as_ref().map(|c| c["stage"].clone()),
            "at_percent": ROTATE_AT_PERCENT,
            "to_percent": ROTATE_TO_PERCENT,
            "last": last,
            "backoff_secs": backoff,
        })
    }

    fn rotation_stage(&self, stage: &str) {
        if let Ok(mut current) = self.rotation.current.lock() {
            *current = Some(json!({ "stage": stage }));
        }
    }

    /// Whether the last runs freed nothing and the next one must wait:
    /// until the backoff ends, or the store grew by [`REGROW_PERCENT`] of
    /// its share since.
    fn rotation_backed_off(&self, bytes: u64, share: u64) -> bool {
        self.rotation.backoff.lock().is_ok_and(|b| {
            b.until.is_some_and(|until| Instant::now() < until)
                && bytes < b.bytes.saturating_add(percent(share, REGROW_PERCENT))
        })
    }

    /// After a run: back off when it freed less than
    /// [`PRODUCTIVE_PERCENT`] of what it had to, with one warning per step.
    fn rotation_outcome(&self, run: &RotationLog, to: u64) {
        let target = run.before.saturating_sub(to);
        let freed = run.before.saturating_sub(run.after);
        let Ok(mut backoff) = self.rotation.backoff.lock() else {
            return;
        };
        if target == 0 || freed.saturating_mul(100) >= target.saturating_mul(PRODUCTIVE_PERCENT) {
            *backoff = Backoff::default();
            return;
        }
        backoff.step = backoff.step.saturating_add(1);
        let delay = BACKOFF_FIRST
            .saturating_mul(2u32.saturating_pow(backoff.step.saturating_sub(1).min(16)))
            .min(BACKOFF_MAX);
        backoff.until = Instant::now().checked_add(delay);
        backoff.bytes = run.after;
        warn!(
            store = %crate::node::format_bytes(run.after),
            target = %crate::node::format_bytes(to),
            step = backoff.step,
            next_in_secs = delay.as_secs(),
            "rotation freed almost nothing: what is left is kept (this node's own, \
             kept-forever kinds, ranges still being fetched); trying again later, \
             or once the store grows"
        );
    }

    /// Run a rotation if the store reached [`ROTATE_AT_PERCENT`] of its
    /// share, none runs and runs are not backing off: what it did. Settles
    /// first the blobs a crashed run left to untag.
    pub async fn rotate_if_full(self: &Arc<Self>) -> Result<Option<RotationLog>> {
        self.unpin_rotated().await;
        let (Some((at, to)), Some(share)) = (self.rotation_bounds(), self.events_share()) else {
            return Ok(None);
        };
        let bytes = self.stored_bytes().await;
        if bytes < at {
            if let Ok(mut backoff) = self.rotation.backoff.lock() {
                *backoff = Backoff::default();
            }
            return Ok(None);
        }
        if self.rotation_backed_off(bytes, share) {
            return Ok(None);
        }
        if self.rotation.running.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let _running = Running(&self.rotation);
        self.rotation_stage("starting");
        self.rotation
            .node_evictions
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let run = match self.rotate(to).await {
            Ok(run) => run,
            // A failed run backs off as one that freed nothing.
            Err(e) => {
                let after = self.stored_bytes().await;
                self.rotation_outcome(
                    &RotationLog {
                        before: bytes,
                        after: bytes.max(after),
                        ..RotationLog::default()
                    },
                    to,
                );
                return Err(e.context("rotation failed"));
            }
        };
        self.rotation_outcome(&run, to);
        Ok(Some(run))
    }

    /// The store bytes, read on a blocking thread.
    async fn stored_bytes(&self) -> u64 {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.store_bytes())
            .await
            .unwrap_or(0)
    }

    /// How this node judges posts, for the value order: its profiles and
    /// the reputation tables of the networks it joined.
    fn rotation_grouping(&self) -> Grouping {
        let mut grouping = Grouping::new(self.local_authors());
        for net in self.joined_nets() {
            match self.reputation(&net) {
                Ok(table) => grouping = grouping.with_table(table),
                Err(e) => warn!(%net, "rotation: no reputation table, scores count 0: {e:#}"),
            }
        }
        grouping
    }

    /// One rotation run, down to `to` store bytes.
    async fn rotate(self: &Arc<Self>, to: u64) -> Result<RotationLog> {
        let before = self.stored_bytes().await;
        let now = now_secs();
        let mut run = RotationLog {
            before,
            ..RotationLog::default()
        };
        let (keep, grouping) = {
            let (node, local) = (Arc::clone(self), self.local_authors());
            blocking(move || {
                let keep = node.store.rotation_keep(&local)?;
                Ok((Arc::new(keep), Arc::new(node.rotation_grouping())))
            })
            .await?
        };
        info!(
            store = %crate::node::format_bytes(before),
            target = %crate::node::format_bytes(to),
            "rotation: the store reached {ROTATE_AT_PERCENT} % of its share; freeing space"
        );
        // Rows are deleted only once the node has run a while: a profile
        // used before 0.4.3 through an MCP session only, and not
        // configured, is known again once a session uses it (bodies, which
        // come back on read, may go meanwhile).
        let deletions = now
            >= self
                .started_at()
                .saturating_add(self.config.history.rotation_start_delay.as_secs());
        if !deletions {
            info!("rotation: no row deleted in the first minutes after start, bodies only");
        }
        if deletions && self.stored_bytes().await > to {
            self.rotation_stage("keys over their share");
            run.trimmed = self.trim_keys(&keep, &grouping, to, now).await?;
        }
        if self.stored_bytes().await > to {
            self.rotation_stage("bodies");
            run.bodies = self.evict_by_value(&keep, &grouping, to, now).await?;
        }
        if deletions && self.stored_bytes().await > to {
            self.rotation_stage("raising the cut");
            run.rows = self.raise_cuts(&keep, to, now).await?;
        }
        run.at = now_secs();
        run.after = self.stored_bytes().await;
        let (store, logged) = (Arc::clone(&self.store), run.clone());
        blocking(move || store.log_rotation(&logged)).await?;
        info!(
            bodies = run.bodies,
            trimmed = run.trimmed,
            rows = run.rows,
            before = %crate::node::format_bytes(run.before),
            after = %crate::node::format_bytes(run.after),
            "rotation done"
        );
        Ok(run)
    }

    /// Rotate out the post `id` that lists blobs, through the node's
    /// eviction (ADR 0029): it keeps a blob it holds as a replica, frees one
    /// only once other holders are proven, and refuses the eviction when it
    /// may hold the last copy. Before, this node's estimate of being among
    /// the k responsible for a blob it holds may add to the replica record,
    /// never remove from it. Returns whether the body went; a refusal, a
    /// stronger drop reason or a protection (checked again, strictly only
    /// with `strictly`) leaves it.
    async fn evict_through_node(&self, id: &str, keep: &Arc<Keep>, strictly: bool) -> Eviction {
        let kept_until = self
            .rotation
            .kept_last
            .lock()
            .ok()
            .and_then(|k| k.get(id).copied());
        if kept_until.is_some_and(|until| Instant::now() < until) {
            return Eviction::Kept;
        }
        let tried = self
            .rotation
            .node_evictions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tried >= MAX_NODE_EVICTIONS {
            return Eviction::Capped;
        }
        match self.try_evict_through_node(id, keep, strictly).await {
            Ok(true) => Eviction::Dropped,
            Ok(false) => Eviction::Kept,
            Err(e)
                if e.downcast_ref::<super::durability::KeptLastCopy>()
                    .is_some() =>
            {
                debug!(%id, "rotation: {e:#}");
                // Spread by the id, so that the retries do not come together.
                let spread = id
                    .bytes()
                    .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
                    .checked_rem((LAST_COPY_RETRY.as_secs() / 4).max(1))
                    .unwrap_or(0);
                let until = Instant::now()
                    .checked_add(LAST_COPY_RETRY.saturating_add(Duration::from_secs(spread)));
                if let (Ok(mut kept), Some(until)) = (self.rotation.kept_last.lock(), until) {
                    kept.retain(|_, t| Instant::now() < *t);
                    kept.insert(id.to_string(), until);
                }
                Eviction::Kept
            }
            Err(e) => {
                warn!(%id, "rotation: a post not evicted, skipped: {e:#}");
                Eviction::Kept
            }
        }
    }

    /// [`Node::evict_through_node`]'s work: `Ok(false)` when the post stays
    /// (protected, a stronger reason, gone), an error for a refusal.
    async fn try_evict_through_node(
        &self,
        id: &str,
        keep: &Arc<Keep>,
        strictly: bool,
    ) -> Result<bool> {
        let (store, keep_for, owned) = (Arc::clone(&self.store), Arc::clone(keep), id.to_string());
        let (rotatable, event) = blocking(move || {
            let rotatable = store.rotatable(&owned, &keep_for, strictly)?;
            Ok((rotatable, store.get(&owned)?.map(|s| s.event)))
        })
        .await?;
        let (true, Some(event)) = (rotatable, event) else {
            return Ok(false);
        };
        for blob in event.blobs.iter().filter(|b| b.url.is_none()) {
            let Ok(hash) = blob.hash.parse::<iroh_blobs::Hash>() else {
                continue;
            };
            if !self.has_replica_intent(&hash).await && self.holds_blob(blob).await {
                let k = self.blob_target_k(&blob.hash).await;
                if self.is_responsible(&blob.hash, k) {
                    self.record_replica_intent(hash, blob.size).await;
                }
            }
        }
        Ok(self
            .drop_body(id, DropReason::Evicted, None, None)
            .await?
            .body_dropped)
    }

    /// Before stage 0 or 2 deletes the rows `ids` (oldest first): evict,
    /// through the node, those that list blobs ([`Node::evict_through_node`]).
    /// A row whose eviction is refused stays full, and `delete_rows` leaves
    /// it; one whose eviction kept replicas stays as a tombstone. Once the
    /// run's evictions through the node are used up, returns how many of
    /// `ids` may still go (those before the first row not tried), so that
    /// the cut does not rise past rows never tried.
    async fn evict_listing_blobs(
        &self,
        ids: &[String],
        keep: &Arc<Keep>,
        strictly: bool,
    ) -> Result<Option<usize>> {
        let (store, owned) = (Arc::clone(&self.store), ids.to_vec());
        let listing = blocking(move || store.lists_blobs(&owned)).await?;
        for (i, id) in ids.iter().enumerate() {
            if listing.contains(id)
                && self.evict_through_node(id, keep, strictly).await == Eviction::Capped
            {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// Why this node keeps a blob left in `unpin_pending` (rotation's
    /// transactions free none any more: the rows that list blobs go through
    /// the node's eviction first): a replica, this node responsible for it,
    /// or no proof that another node holds it. Never freed here: nothing
    /// proves it is not the last copy.
    async fn residual_reason(&self, hash: &str) -> &'static str {
        if let Ok(parsed) = hash.parse::<iroh_blobs::Hash>()
            && self.has_replica_intent(&parsed).await
        {
            return "replica";
        }
        if self.is_responsible(hash, self.blob_target_k(hash).await) {
            return "responsible";
        }
        crate::store::RETAIN_UNDECIDED
    }

    /// Settle the blobs left in `unpin_pending` (by a run of an earlier
    /// build, or a crash between a transaction and its settling): listed
    /// again by a kept post, nothing to do; otherwise kept, logged in
    /// `blob_retained` with [`Node::residual_reason`] in the transaction that
    /// settles it. The store work runs on blocking threads.
    async fn unpin_rotated(&self) {
        let store = Arc::clone(&self.store);
        let pending = match blocking(move || store.unpin_pending()).await {
            Ok(pending) => pending,
            Err(e) => {
                warn!("rotation: blobs to settle not read: {e:#}");
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        let (mut kept, mut done) = (Vec::new(), Vec::new());
        for (hash, listed, reason) in pending {
            if listed {
                done.push(hash);
            } else if let Some(reason) = reason {
                kept.push((hash, reason));
            } else {
                let reason = self.residual_reason(&hash).await.to_string();
                kept.push((hash, reason));
            }
        }
        let store = Arc::clone(&self.store);
        let now = now_secs();
        if let Err(e) = blocking(move || store.settle_unpin(&kept, &done, now)).await {
            warn!("rotation: kept blobs not recorded: {e:#}");
        }
    }

    /// After a transaction: untag what it freed, and let inserts in. Fails
    /// once the node stopped: a restarted node may already use the store,
    /// and it settles what is left (`unpin_pending`).
    async fn after_batch(&self, rotated: &Rotated) -> Result<()> {
        self.still_running()?;
        if !rotated.freed_blobs.is_empty() {
            self.unpin_rotated().await;
        }
        tokio::time::sleep(self.config.history.rotation_pause).await;
        self.still_running()
    }

    /// Stage 0: trim the keys over their share, per network, largest excess
    /// first, down to it: rows older than a week, lowest value first.
    /// Returns the rows deleted.
    async fn trim_keys(
        self: &Arc<Self>,
        keep: &Arc<Keep>,
        grouping: &Arc<Grouping>,
        to: u64,
        now: u64,
    ) -> Result<u64> {
        let (store, keep_for) = (Arc::clone(&self.store), Arc::clone(keep));
        let over = blocking(move || keys_over_share(&store, &keep_for)).await?;
        let mut deleted = 0u64;
        for key in over {
            if self.stored_bytes().await <= to {
                break;
            }
            let (store, keep_for, grouping_for, key_for) = (
                Arc::clone(&self.store),
                Arc::clone(keep),
                Arc::clone(grouping),
                key.clone(),
            );
            let order =
                blocking(move || trim_order(&store, &grouping_for, &keep_for, &key_for, now))
                    .await?;
            let mut bytes = key.bytes;
            let mut trimmed = 0u64;
            let mut rows = order.into_iter().peekable();
            while bytes > key.limit && rows.peek().is_some() && self.stored_bytes().await > to {
                let mut ids = Vec::new();
                while ids.len() < BATCH && bytes > key.limit {
                    let Some((id, len)) = rows.next() else { break };
                    bytes = bytes.saturating_sub(len);
                    ids.push(id);
                }
                let capped = self.evict_listing_blobs(&ids, keep, true).await?;
                if let Some(n) = capped {
                    ids.truncate(n);
                }
                let (store, keep_for) = (Arc::clone(&self.store), Arc::clone(keep));
                let rotated = blocking(move || {
                    store.delete_rows(&ids, &keep_for, crate::store::Deletion::Trim, now)
                })
                .await?;
                trimmed = trimmed.saturating_add(rotated.rows as u64);
                self.after_batch(&rotated).await?;
                if capped.is_some() {
                    return Ok(deleted.saturating_add(trimmed));
                }
            }
            if trimmed > 0 {
                info!(
                    net = %key.net,
                    key = %key.keys.first().map(String::as_str).unwrap_or_default(),
                    rows = trimmed,
                    "rotation: a key over its share trimmed"
                );
            }
            deleted = deleted.saturating_add(trimmed);
        }
        Ok(deleted)
    }

    /// Stage 1: drop bodies, lowest value first, until the store is down
    /// to `to`. Returns the bodies dropped.
    async fn evict_by_value(
        self: &Arc<Self>,
        keep: &Arc<Keep>,
        grouping: &Arc<Grouping>,
        to: u64,
        now: u64,
    ) -> Result<u64> {
        let (store, keep_for, grouping_for) = (
            Arc::clone(&self.store),
            Arc::clone(keep),
            Arc::clone(grouping),
        );
        let plan = blocking(move || plan(&store, &grouping_for, &keep_for, now)).await?;
        let mut dropped = 0u64;
        let mut capped = false;
        // Small transactions: the store is measured again after each, so
        // that stage 1 stops near its target.
        for chunk in plan.chunks(BODIES_BATCH) {
            if self.stored_bytes().await <= to {
                break;
            }
            let ids = chunk.to_vec();
            // The cheap batch first: the store drops the bodies that list no
            // blob, and leaves the others.
            let (store, keep_for, batch) = (Arc::clone(&self.store), Arc::clone(keep), ids.clone());
            let rotated = blocking(move || store.evict_bodies(&batch, &keep_for, now)).await?;
            dropped = dropped.saturating_add(rotated.rows as u64);
            self.after_batch(&rotated).await?;
            if capped {
                continue;
            }
            // Those that list blobs go through the node's eviction, under
            // ADR 0029's last-copy rule, at most so many a run.
            let (store, owned) = (Arc::clone(&self.store), ids.clone());
            let listing = blocking(move || store.lists_blobs(&owned)).await?;
            for id in ids.iter().filter(|id| listing.contains(*id)) {
                if self.stored_bytes().await <= to {
                    break;
                }
                match self.evict_through_node(id, keep, false).await {
                    Eviction::Dropped => dropped = dropped.saturating_add(1),
                    Eviction::Kept => {}
                    Eviction::Capped => {
                        capped = true;
                        break;
                    }
                }
            }
        }
        Ok(dropped)
    }

    /// Whether this node's copy of `net`'s history may be the swarm's last:
    /// no trusted neighbor of the network (a configured peer or bootstrap
    /// node) advertises `history_full` (ADR 0025 §3), so none vouches for
    /// holding what stage 2 is about to delete.
    pub fn may_hold_last_copy(&self, net: &str) -> bool {
        !self.neighbors_of(net).iter().any(|peer| {
            self.trusted(peer) && self.has_feature(peer, crate::proto::FEATURE_HISTORY_FULL)
        })
    }

    /// The hook before stage 2 deletes `rows` (headers included): where a
    /// later release summarizes what is worth it before the last copy goes
    /// (a digest, ADR 0025 implementation notes, "Last copy"). Vincent's
    /// rule: only what is interesting is captured; chit-chat never used is
    /// simply lost. Today it only counts and logs the rows that may be the
    /// last copy.
    fn before_last_copy(&self, rows: &[RowHead]) {
        let mut last: HashMap<&str, usize> = HashMap::new();
        for row in rows {
            if self.may_hold_last_copy(&row.net) {
                let count = last.entry(row.net.as_str()).or_default();
                *count = count.saturating_add(1);
            }
        }
        for (net, count) in last {
            debug!(
                %net,
                rows = count,
                "rotation: deleting rows no history_full neighbor holds (maybe the last copy)"
            );
        }
    }

    /// Stage 2: delete the oldest unprotected rows, across networks, and
    /// raise each network's rotation cut to the newest one deleted. Returns
    /// the rows deleted. Bounded by [`OVERSHOOT`], with the store's bytes
    /// per row measured now, after stages 0 and 1.
    async fn raise_cuts(self: &Arc<Self>, keep: &Arc<Keep>, to: u64, now: u64) -> Result<u64> {
        let joined: HashSet<String> = self.joined_nets().into_iter().collect();
        let store = Arc::clone(&self.store);
        let (mut cutter, total) = blocking(move || {
            let cutter = Cutter::new(&store, &joined)?;
            Ok((cutter, store.json_bytes()?))
        })
        .await?;
        let start = self.stored_bytes().await;
        let ratio = if total > 0 {
            start as f64 / total as f64
        } else {
            1.0
        };
        let bound = (start.saturating_sub(to) as f64 * OVERSHOOT as f64) as u64;
        let mut deleted = 0u64;
        let mut weighed = 0u64;
        loop {
            let (store, keep_for) = (Arc::clone(&self.store), Arc::clone(keep));
            let (back, batch) = blocking(move || {
                let batch = cutter.next_batch(&store, &keep_for)?;
                Ok((cutter, batch))
            })
            .await?;
            cutter = back;
            if batch.is_empty() {
                break;
            }
            weighed = batch.iter().fold(weighed, |w, r| {
                w.saturating_add((r.len as f64 * ratio) as u64)
            });
            self.before_last_copy(&batch);
            let mut ids: Vec<String> = batch.into_iter().map(|r| r.id).collect();
            let capped = self.evict_listing_blobs(&ids, keep, false).await?;
            if let Some(n) = capped {
                ids.truncate(n);
            }
            let (store, keep_for) = (Arc::clone(&self.store), Arc::clone(keep));
            let rotated = blocking(move || {
                store.delete_rows(&ids, &keep_for, crate::store::Deletion::Cut, now)
            })
            .await?;
            deleted = deleted.saturating_add(rotated.rows as u64);
            for (net, ts) in &rotated.cuts {
                info!(%net, cut = %super::history::date(*ts), "rotation: the cut rises");
            }
            self.after_batch(&rotated).await?;
            if capped.is_some() {
                info!("rotation: enough evictions through the node for one run; the next goes on");
                break;
            }
            if self.stored_bytes().await <= to {
                break;
            }
            if weighed >= bound.max(1) {
                info!("rotation: freed pages lag behind the rows deleted; the next check goes on");
                break;
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn value_order_is_status_then_score_then_age_entries_last() {
        let v = |entry, status, score, ts, id: &str| Value1 {
            entry,
            status,
            score,
            ts,
            id: id.into(),
        };
        let mut values = [
            v(true, 0, 0.0, 1, "entry"),
            v(false, 3, 0.0, 1, "confirmed"),
            v(false, 0, 5.0, 9, "contested, scored"),
            v(false, 0, 0.0, 9, "contested, newer"),
            v(false, 0, 0.0, 1, "contested, older"),
            v(false, 1, 0.0, 1, "unverified"),
            v(false, 2, 0.0, 1, "stale"),
        ];
        values.sort_by(by_value);
        let order: Vec<&str> = values.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            order,
            [
                "contested, older",
                "contested, newer",
                "contested, scored",
                "unverified",
                "stale",
                "confirmed",
                "entry"
            ]
        );
    }

    #[test]
    fn the_summary_reads_well() {
        let idle = json!({"state": "idle", "at_percent": 95, "to_percent": 85, "last": null});
        assert_eq!(
            summary(&idle),
            "idle · from 95 % down to 85 % of the events share"
        );
        let ran = json!({"state": "running", "stage": "bodies", "at_percent": 95,
            "to_percent": 85, "last": {"at": 0, "bodies": 3, "trimmed": 0, "rows": 7,
            "before": 2048, "after": 1024}});
        let text = summary(&ran);
        assert!(text.starts_with("running (bodies)"), "{text}");
        assert!(text.contains("3 bodies dropped"), "{text}");
        assert!(text.contains("7 old rows deleted"), "{text}");
    }

    use crate::event::{Draft, Event, Kind};
    use crate::reputation::DAY;
    use crate::signals::Status;
    use crate::signals::tests::{Graph, id_of};
    use crate::store::DropReason;

    /// A post of `seed` whose body is worth dropping, a reply to `to`.
    fn long(g: &mut Graph, seed: u8, kind: Kind, title: &str, to: Option<&Event>) -> Event {
        g.add(
            seed,
            Draft {
                net: "public".into(),
                thread: to.map(|t| t.thread_root().to_string()),
                parent: to.map(|t| t.id.clone()),
                kind: Some(kind),
                title: Some(title.into()),
                text: format!("{title}: {}", "a long body worth dropping. ".repeat(60)),
                ..Draft::default()
            },
        )
    }

    fn grouping(g: &Graph, local: &[u8]) -> Grouping {
        let table = g.table(local);
        Grouping::new(table.local.clone()).with_table(table)
    }

    fn keep(g: &Graph, local: &[u8]) -> Keep {
        let local = local.iter().map(|s| id_of(*s)).collect();
        g.store.rotation_keep(&local).unwrap()
    }

    /// Stage 1's value order on a fixed graph (ADR 0025 §7): contested,
    /// then unverified (older first, then by score), then confirmed, and
    /// knowledge entries last; the never-drop list, reacts, short bodies
    /// and posts younger than a week are left out.
    #[test]
    fn value_order_on_a_fixed_graph() {
        let mut g = Graph::new();
        // 30 is this node.
        let ask = g.root(1, Kind::Ask, "how?");
        let contested = long(&mut g, 2, Kind::Answer, "contested", Some(&ask));
        g.contradict(3, &contested, "fails on rustc 1.91");
        let older = long(&mut g, 4, Kind::Note, "unverified, older", None);
        let newer = long(&mut g, 5, Kind::Note, "unverified, newer", None);
        let voted = long(&mut g, 6, Kind::Note, "unverified, voted", None);
        for seed in [7, 8] {
            g.reply(seed, &voted, Kind::React, json!({"value": 1}));
        }
        let ask2 = g.root(10, Kind::Ask, "why?");
        let confirmed = long(&mut g, 9, Kind::Answer, "confirmed", Some(&ask2));
        g.reply(10, &confirmed, Kind::React, json!({"accept": true}));
        let ask3 = g.root(12, Kind::Ask, "where?");
        let entry = long(&mut g, 11, Kind::Answer, "an entry", Some(&ask3));
        g.reply(12, &entry, Kind::React, json!({"accept": true}));
        g.confirm(13, &entry);
        // Never in the plan.
        let own = long(&mut g, 30, Kind::Note, "mine", None);
        let joined = long(&mut g, 14, Kind::Ask, "a thread I joined", None);
        let my_answer = g.reply(30, &joined, Kind::Answer, json!({}));
        let to_mine = long(&mut g, 20, Kind::Answer, "a reply to me", Some(&my_answer));
        // A sibling of my answer: anyone's.
        let in_joined = long(&mut g, 15, Kind::Answer, "in it", Some(&joined));
        let announce = long(&mut g, 16, Kind::Announce, "kept forever", None);
        let react = g.add(
            17,
            Draft {
                net: "public".into(),
                thread: Some(ask.id.clone()),
                parent: Some(ask.id.clone()),
                kind: Some(Kind::React),
                text: "a long reasoned vote. ".repeat(100),
                data: json!({"value": 1}).as_object().unwrap().clone(),
                ..Draft::default()
            },
        );
        let short = g.root(18, Kind::Note, "short");
        g.later(8 * DAY);
        let recent = long(&mut g, 19, Kind::Note, "too recent", None);

        let s = g.signals_with(&[30], &voted);
        assert_eq!(s.status, Status::Unverified);
        assert!(s.score > 0.0, "{s:?}");
        assert_eq!(g.signals_with(&[30], &contested).status, Status::Contested);
        assert_eq!(g.signals_with(&[30], &confirmed).status, Status::Confirmed);

        let plan = plan(&g.store, &grouping(&g, &[30]), &keep(&g, &[30]), g.ts).unwrap();
        let expected: Vec<String> = [
            &contested, &older, &newer, &in_joined, &voted, &confirmed, &entry,
        ]
        .iter()
        .map(|e| e.id.clone())
        .collect();
        assert_eq!(plan, expected);
        for left_out in [&own, &joined, &to_mine, &announce, &react, &short, &recent] {
            assert!(!plan.contains(&left_out.id), "{}", left_out.kind);
        }
        // And the store drops what the plan says, nothing else.
        let done = g.store.evict_bodies(&plan, &keep(&g, &[30]), g.ts).unwrap();
        assert_eq!(done.rows, expected.len());
        assert!(g.store.get(&own.id).unwrap().is_some());
        assert!(g.store.get(&to_mine.id).unwrap().is_some());
        assert!(g.store.get(&joined.id).unwrap().is_some());
        assert!(g.store.get(&react.id).unwrap().is_some());
    }

    /// A stage-1 eviction of the posts that give signals changes no
    /// status nor score: a reasoned reply that contests, citations, and
    /// the post itself (ADR 0025 PR B review). A retraction, unlike an
    /// eviction, withdraws the signal.
    #[test]
    fn evicting_signal_sources_changes_no_status_nor_score() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = long(&mut g, 2, Kind::Answer, "an answer", Some(&ask));
        g.reply(1, &answer, Kind::React, json!({"accept": true}));
        // A reasoned reply with a -1 contests it.
        let reasoned = long(&mut g, 3, Kind::Answer, "it fails on musl", Some(&answer));
        g.reply(3, &answer, Kind::React, json!({"value": -1}));
        // Later posts cite it.
        let ask2 = g.root(4, Kind::Ask, "and on arm?");
        let citing = g.add(
            5,
            Draft {
                net: "public".into(),
                thread: Some(ask2.id.clone()),
                parent: Some(ask2.id.clone()),
                kind: Some(Kind::Answer),
                title: Some("as said there".into()),
                text: format!("see there: {}", "a long body worth dropping. ".repeat(60)),
                refs: vec![answer.id.clone()],
                ..Draft::default()
            },
        );
        let before = g.signals(&answer);
        assert_eq!(before.status, Status::Contested, "{before:?}");
        assert!(before.cites > 0, "{before:?}");
        let table = g.table(&[]);
        let author_before = table.score(&id_of(2));
        for post in [&reasoned, &citing, &answer] {
            g.store
                .drop_body(&post.id, DropReason::Evicted, None, None, g.ts)
                .unwrap();
            assert!(g.store.get(&post.id).unwrap().is_none());
        }
        let after = g.signals(&answer);
        assert_eq!(after.status, before.status);
        assert_eq!(after.contradicts, before.contradicts);
        assert_eq!(after.cites, before.cites);
        assert!(
            (after.score - before.score).abs() < 1e-9,
            "{before:?} {after:?}"
        );
        let author_after = g.table(&[]).score(&id_of(2));
        assert!((author_after - author_before).abs() < 1e-9);
    }

    /// A reasoned reply and a -1 from the same key contest a post; once
    /// the author retracts the reply, they no longer do (a bare -1 is only
    /// a vote).
    #[test]
    fn a_retracted_reasoned_reply_no_longer_contests() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = long(&mut g, 2, Kind::Answer, "an answer", Some(&ask));
        let reasoned = long(&mut g, 3, Kind::Answer, "it fails on musl", Some(&answer));
        g.reply(3, &answer, Kind::React, json!({"value": -1}));
        assert_eq!(g.signals(&answer).status, Status::Contested);
        g.store
            .drop_body(&reasoned.id, DropReason::Retraction, Some("r"), None, g.ts)
            .unwrap();
        let after = g.signals(&answer);
        assert_ne!(after.status, Status::Contested, "{after:?}");
        assert_eq!(after.contradicts, 0);
    }

    /// An evicted post keeps its signals and its author's score: reputation
    /// reads the heads of tombstones (ADR 0025 §7), an evicted ask root
    /// included.
    #[test]
    fn an_evicted_post_keeps_its_authors_score() {
        let mut g = Graph::new();
        // 1 is this node.
        let ask = long(&mut g, 1, Kind::Ask, "how?", None);
        let good = long(&mut g, 2, Kind::Answer, "good", Some(&ask));
        g.reply(1, &good, Kind::React, json!({"accept": true}));
        g.confirm(3, &good);
        let before = g.table(&[1]);
        let score = before.score(&id_of(2));
        assert!(score > 0.0);
        for post in [&good, &ask] {
            g.store
                .drop_body(&post.id, DropReason::Evicted, None, None, g.ts)
                .unwrap();
        }
        assert!(g.store.get(&good.id).unwrap().is_none(), "a tombstone now");
        let after = g.table(&[1]);
        assert!(
            (after.score(&id_of(2)) - score).abs() < 1e-9,
            "{} then {}",
            score,
            after.score(&id_of(2))
        );
        assert_eq!(
            after.authors[&id_of(2)].counts.accepts,
            before.authors[&id_of(2)].counts.accepts
        );
    }
}
