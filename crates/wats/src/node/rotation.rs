//! Rotation at the cap (ADR 0025 §7, PR B).
//!
//! When the store bytes reach [`ROTATE_AT_PERCENT`] of the events share,
//! the node frees space until they are back under [`ROTATE_TO_PERCENT`],
//! in the background, in transactions of at most [`BATCH`] rows:
//!
//! 0. **Keys over their share.** A key (with its rotation chain) holding
//!    more than [`KEY_SHARE_PERCENT`] of the events' bytes loses its oldest
//!    rows first, down to that share: a flood pays for itself before
//!    anyone else's posts are touched. The cut does not move: as with the
//!    fill's per-key share, these are skipped events.
//! 1. **Bodies, lowest value first** (reason `evicted`): `contested`, then
//!    `unverified`, `stale` and `confirmed`, then the post's score, then
//!    older first, knowledge entries last. Only bodies that save at least
//!    1 KiB, never a `react` (its body is its signal), and only posts
//!    older than a week ([`BODY_GRACE_SECS`]: a new post has not had the
//!    time to earn its signals).
//! 2. **The cut rises.** Whole rows go, oldest first, and the cut of their
//!    network rises to the newest one deleted, in the same transaction.
//!    Protected rows stay as islands below it. Never below a range still
//!    being fetched, nor in a network whose history task has not run yet.
//!
//! Never dropped, in any stage ([`crate::store::Keep`]): posts of this
//! node's profiles, the threads they posted in and the human's asks
//! (`ask_notify`), and the kept-forever kinds.
//!
//! Each batch is one transaction: a crash leaves the store as after the
//! last batch, and the next check (every 30 s) starts again. Blobs that no
//! kept post lists any more are recorded in that transaction and untagged
//! after it, also after a crash in between.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use super::{Node, now_secs};
use crate::signals::Grouping;
use crate::store::{EVICT_MIN_SAVING, Keep, Rotated, RotationLog, RowHead, Store};

/// Rotation starts when the store bytes reach this share of the events
/// share, and runs until they are back under the next one.
pub const ROTATE_AT_PERCENT: u64 = 95;
pub const ROTATE_TO_PERCENT: u64 = 85;
/// Most rows per transaction.
pub const BATCH: usize = 500;
/// Stage 1 leaves the bodies of posts younger than this alone.
pub const BODY_GRACE_SECS: u64 = 7 * 24 * 3600;
/// Stage 0: the share of the events' bytes one key, with its rotation
/// chain, may hold before rotation trims it first.
pub const KEY_SHARE_PERCENT: u64 = 2;
/// Most bodies one run ranks by value: the oldest ones, beyond.
const MAX_PLAN: usize = 200_000;
/// Rows read per step of a scan.
const SCAN: usize = 2_000;
/// Stage 2 stops a run once the rows it deleted weigh, by the store's
/// average, this many times what was to be freed: freed pages can lag
/// (the full-text index merges its deletions later), and the next check
/// runs again if the store is still full.
const OVERSHOOT: u64 = 2;

/// The rotation of a node: one run at a time.
#[derive(Debug, Default)]
pub struct Rotation {
    running: AtomicBool,
    /// The run in progress.
    current: std::sync::Mutex<Option<Value>>,
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

/// The value order of stage 1 (ADR 0025 §7): lowest first.
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
    /// bounds, and the last run.
    pub fn rotation_json(&self) -> Value {
        let current = self.rotation.current.lock().ok().and_then(|c| c.clone());
        let last = self.store.last_rotation().ok().flatten();
        json!({
            "state": if current.is_some() { "running" } else { "idle" },
            "stage": current.as_ref().map(|c| c["stage"].clone()),
            "at_percent": ROTATE_AT_PERCENT,
            "to_percent": ROTATE_TO_PERCENT,
            "last": last,
        })
    }

    fn rotation_stage(&self, stage: &str) {
        if let Ok(mut current) = self.rotation.current.lock() {
            *current = Some(json!({ "stage": stage }));
        }
    }

    /// Run a rotation if the store reached [`ROTATE_AT_PERCENT`] of its
    /// share and none runs: what it did. Untags first the blobs a crashed
    /// run left to untag.
    pub async fn rotate_if_full(self: &Arc<Self>) -> Result<Option<RotationLog>> {
        self.unpin_rotated().await;
        let Some((at, to)) = self.rotation_bounds() else {
            return Ok(None);
        };
        if self.store.store_bytes() < at {
            return Ok(None);
        }
        if self.rotation.running.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let _running = Running(&self.rotation);
        self.rotation_stage("starting");
        let run = self.rotate(to).await?;
        Ok(Some(run))
    }

    /// One rotation run, down to `to` store bytes.
    async fn rotate(self: &Arc<Self>, to: u64) -> Result<RotationLog> {
        let before = self.store.store_bytes();
        let now = now_secs();
        let mut run = RotationLog {
            before,
            ..RotationLog::default()
        };
        let keep = {
            let (node, local) = (Arc::clone(self), self.local_authors());
            tokio::task::spawn_blocking(move || node.store.rotation_keep(&local)).await??
        };
        info!(
            store = %crate::node::format_bytes(before),
            target = %crate::node::format_bytes(to),
            "rotation: the store reached {ROTATE_AT_PERCENT} % of its share; freeing space"
        );
        let keep = Arc::new(keep);
        let total = if self.store.store_bytes() > to {
            self.rotation_stage("keys over their share");
            let (trimmed, total) = self.trim_keys(&keep, to, now).await?;
            run.trimmed = trimmed;
            total
        } else {
            0
        };
        if self.store.store_bytes() > to {
            self.rotation_stage("bodies");
            run.bodies = self.evict_by_value(&keep, to, now).await?;
        }
        if self.store.store_bytes() > to {
            self.rotation_stage("raising the cut");
            run.rows = self.raise_cuts(&keep, to, total, now).await?;
        }
        run.at = now_secs();
        run.after = self.store.store_bytes();
        self.store.log_rotation(&run)?;
        if run.after > to {
            warn!(
                store = %crate::node::format_bytes(run.after),
                target = %crate::node::format_bytes(to),
                "rotation: still above {ROTATE_TO_PERCENT} % of the events share \
                 (what is left is kept forever or this node's own, or freed pages lag); \
                 the next check runs again"
            );
        }
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

    /// Why this node keeps blob `hash` although no post it holds in full
    /// lists it any more, if it does (ADR 0025, "Blobs"): rotation of events
    /// never loses a file by side effect. Blob retention follows blob
    /// responsibility ([`Node::blob_responsible`]), not whether the body is
    /// held. A blob it only cached (fetched on a read, not responsible) may
    /// go.
    ///
    /// The place for the next reasons (a pin, the last copy: ADR 0029),
    /// and where blob eviction by value will look.
    pub fn blob_retained(&self, hash: &str) -> Option<&'static str> {
        self.blob_responsible(hash).then_some("replica")
    }

    /// Untag the blobs rotation freed (`unpin_pending`): those no kept post
    /// lists any more, unless this node keeps them ([`Node::blob_retained`]:
    /// recorded in `blob_retained`, tag kept). Best effort: what fails
    /// stays pending for the next run.
    async fn unpin_rotated(&self) {
        let Ok(pending) = self.store.unpin_pending() else {
            return;
        };
        let mut done = Vec::new();
        for hash in pending {
            match self.store.blob_referenced(&hash) {
                // Listed again since: kept, nothing to untag.
                Ok(true) => {
                    done.push(hash);
                    continue;
                }
                Ok(false) => {}
                Err(_) => continue,
            }
            if let Some(reason) = self.blob_retained(&hash) {
                match self.store.retain_blob(&hash, reason, now_secs()) {
                    Ok(()) => done.push(hash),
                    Err(e) => warn!(%hash, "rotation: kept blob not recorded: {e:#}"),
                }
                continue;
            }
            let Ok(parsed) = hash.parse::<iroh_blobs::Hash>() else {
                done.push(hash);
                continue;
            };
            match super::unpin_blob(&self.blobs, parsed).await {
                Ok(()) => {
                    if let Ok(mut unreadable) = self.blob_keep.unreadable.lock() {
                        unreadable.remove(&parsed);
                    }
                    done.push(hash);
                }
                Err(e) => warn!(%hash, "rotation: blob not untagged yet: {e:#}"),
            }
        }
        if !done.is_empty()
            && let Err(e) = self.store.unpinned(&done)
        {
            warn!("rotation: untagged blobs not recorded: {e:#}");
        }
    }

    /// After a transaction: untag what it freed, and let inserts in. Fails
    /// once the node stopped: a restarted node may already use the store,
    /// and it untags what is left (`unpin_pending`).
    async fn after_batch(&self, rotated: &Rotated) -> Result<()> {
        self.still_running()?;
        if !rotated.freed_blobs.is_empty() {
            self.unpin_rotated().await;
        }
        tokio::time::sleep(self.config.history.rotation_pause).await;
        self.still_running()
    }

    /// Stage 0: trim the keys over [`KEY_SHARE_PERCENT`] of the events'
    /// bytes, largest first, oldest rows first, down to that share. Returns
    /// the rows deleted, and the bytes of every row's JSON before.
    async fn trim_keys(
        self: &Arc<Self>,
        keep: &Arc<Keep>,
        to: u64,
        now: u64,
    ) -> Result<(u64, u64)> {
        let node = Arc::clone(self);
        let keep_for = Arc::clone(keep);
        let (over, total) =
            tokio::task::spawn_blocking(move || node.keys_over_share(&keep_for)).await??;
        let mut deleted = 0u64;
        for (keys, mut bytes, limit) in over {
            let mut after = (0u64, String::new());
            while bytes > limit && self.store.store_bytes() > to {
                let rows = self
                    .store
                    .author_rows_after(&keys, (after.0, &after.1), SCAN)?;
                if rows.is_empty() {
                    break;
                }
                // Oldest first: the next page starts after the last row
                // looked at, taken or protected.
                let mut ids = Vec::new();
                for row in &rows {
                    if bytes <= limit || ids.len() >= BATCH {
                        break;
                    }
                    after = (row.ts, row.id.clone());
                    if keep.protects(row) {
                        continue;
                    }
                    bytes = bytes.saturating_sub(row.len);
                    ids.push(row.id.clone());
                }
                if ids.is_empty() {
                    continue;
                }
                let (node, keep_for) = (Arc::clone(self), Arc::clone(keep));
                let rotated = tokio::task::spawn_blocking(move || {
                    node.store.delete_rows(&ids, &keep_for, false, now)
                })
                .await??;
                deleted = deleted.saturating_add(rotated.rows as u64);
                self.after_batch(&rotated).await?;
            }
            info!(
                key = %keys.first().map(String::as_str).unwrap_or_default(),
                "rotation: a key over {KEY_SHARE_PERCENT} % of the events trimmed to its share"
            );
        }
        Ok((deleted, total))
    }

    /// The keys (rotation chains) over their share, largest first: their
    /// keys, their bytes and their share; and the bytes of every row.
    /// This node's own keys are never in it.
    #[allow(clippy::type_complexity)]
    fn keys_over_share(&self, keep: &Keep) -> Result<(Vec<(Vec<String>, u64, u64)>, u64)> {
        let chains = super::history::chains(&self.store.rotations()?);
        let mut bytes: HashMap<String, (u64, HashSet<String>)> = HashMap::new();
        let mut total = 0u64;
        let mut after = 0;
        loop {
            let rows = self.store.rows_after_seq(after, SCAN)?;
            let Some(last) = rows.last() else { break };
            after = last.seq;
            for row in &rows {
                total = total.saturating_add(row.len);
                let chain = chains.get(&row.author).unwrap_or(&row.author);
                let entry = bytes.entry(chain.clone()).or_default();
                entry.0 = entry.0.saturating_add(row.len);
                if !entry.1.contains(&row.author) {
                    entry.1.insert(row.author.clone());
                }
            }
        }
        let limit = percent(total, KEY_SHARE_PERCENT);
        let mut over: Vec<(Vec<String>, u64, u64)> = bytes
            .into_values()
            .filter(|(b, keys)| *b > limit && !keys.iter().any(|k| keep.authors.contains(k)))
            .map(|(b, keys)| {
                let mut keys: Vec<String> = keys.into_iter().collect();
                keys.sort();
                (keys, b, limit)
            })
            .collect();
        over.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok((over, total))
    }

    /// Stage 1: drop bodies, lowest value first, until the store is down
    /// to `to`. Returns the bodies dropped.
    async fn evict_by_value(self: &Arc<Self>, keep: &Arc<Keep>, to: u64, now: u64) -> Result<u64> {
        let (node, keep_for) = (Arc::clone(self), Arc::clone(keep));
        let plan = tokio::task::spawn_blocking(move || node.plan_bodies(&keep_for, now)).await??;
        let mut dropped = 0u64;
        for chunk in plan.chunks(BATCH) {
            if self.store.store_bytes() <= to {
                break;
            }
            let ids = chunk.to_vec();
            let (node, keep_for) = (Arc::clone(self), Arc::clone(keep));
            let rotated =
                tokio::task::spawn_blocking(move || node.store.evict_bodies(&ids, &keep_for, now))
                    .await??;
            dropped = dropped.saturating_add(rotated.rows as u64);
            self.after_batch(&rotated).await?;
        }
        Ok(dropped)
    }

    /// The bodies stage 1 may drop, lowest value first (ADR 0025 §7), as
    /// this node judges them: its profiles and reputation tables.
    pub fn plan_bodies(&self, keep: &Keep, now: u64) -> Result<Vec<String>> {
        let mut grouping = Grouping::new(self.local_authors());
        for net in self.joined_nets() {
            match self.reputation(&net) {
                Ok(table) => grouping = grouping.with_table(table),
                Err(e) => warn!(%net, "rotation: no reputation table, scores count 0: {e:#}"),
            }
        }
        plan(&self.store, &grouping, keep, now)
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

    /// Below what each network's rows may go in stage 2: under its ranges
    /// still being fetched; nothing in a network whose history task has not
    /// run yet (a cut would stop its fill).
    fn cut_floors(&self) -> Result<HashMap<String, u64>> {
        let mut floors = HashMap::new();
        for net in self.store.nets_held()? {
            let ranges = self.store.sync_ranges(&net)?;
            let floor = if self.joined(&net)
                && ranges.is_empty()
                && self.store.sync_watermark(&net)? == 0
            {
                0
            } else {
                ranges
                    .iter()
                    .filter(|r| r.state == crate::store::RangeState::Pending)
                    .map(|r| r.lo_ts)
                    .min()
                    .unwrap_or(u64::MAX)
            };
            floors.insert(net, floor);
        }
        Ok(floors)
    }

    /// Stage 2: delete the oldest unprotected rows, across networks, and
    /// raise each network's cut to the newest one deleted. Returns the rows
    /// deleted.
    async fn raise_cuts(
        self: &Arc<Self>,
        keep: &Arc<Keep>,
        to: u64,
        total: u64,
        now: u64,
    ) -> Result<u64> {
        let floors = self.cut_floors()?;
        let start = self.store.store_bytes();
        // How much a row's JSON weighs in store bytes, on average.
        let total = if total == 0 {
            let mut sum = 0u64;
            let mut after = 0;
            loop {
                let rows = self.store.rows_after_seq(after, SCAN)?;
                let Some(last) = rows.last() else { break };
                after = last.seq;
                sum = rows.iter().fold(sum, |s, r| s.saturating_add(r.len));
            }
            sum
        } else {
            total
        };
        let ratio = if total > 0 {
            start as f64 / total as f64
        } else {
            1.0
        };
        let bound = (start.saturating_sub(to) as f64 * OVERSHOOT as f64) as u64;
        // Per network: what is left to read, and the rows read not taken.
        let mut cursors: HashMap<String, Option<(u64, String)>> = floors
            .iter()
            .filter(|(_, floor)| **floor > 0)
            .map(|(net, _)| (net.clone(), Some((0, String::new()))))
            .collect();
        let mut buffers: HashMap<String, VecDeque<RowHead>> = HashMap::new();
        let mut deleted = 0u64;
        let mut weighed = 0u64;
        loop {
            let mut batch: Vec<RowHead> = Vec::new();
            while batch.len() < BATCH {
                // Refill the networks whose buffer is empty.
                for (net, cursor) in cursors.iter_mut() {
                    let buffer = buffers.entry(net.clone()).or_default();
                    while buffer.is_empty() {
                        let Some(after) = cursor.clone() else { break };
                        let rows = self.store.net_rows_after(net, (after.0, &after.1), SCAN)?;
                        let floor = floors.get(net).copied().unwrap_or(0);
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
                // The oldest of the networks' next rows.
                let next = buffers
                    .iter()
                    .filter_map(|(net, b)| b.front().map(|r| ((r.ts, r.id.clone()), net.clone())))
                    .min();
                let Some((_, net)) = next else { break };
                if let Some(row) = buffers.get_mut(&net).and_then(|b| b.pop_front()) {
                    batch.push(row);
                }
            }
            if batch.is_empty() {
                break;
            }
            weighed = batch.iter().fold(weighed, |w, r| {
                w.saturating_add((r.len as f64 * ratio) as u64)
            });
            self.before_last_copy(&batch);
            let ids: Vec<String> = batch.into_iter().map(|r| r.id).collect();
            let (node, keep_for) = (Arc::clone(self), Arc::clone(keep));
            let rotated = tokio::task::spawn_blocking(move || {
                node.store.delete_rows(&ids, &keep_for, true, now)
            })
            .await??;
            deleted = deleted.saturating_add(rotated.rows as u64);
            for (net, ts) in &rotated.cuts {
                info!(%net, cut = %super::history::date(*ts), "rotation: the cut rises");
            }
            self.after_batch(&rotated).await?;
            if self.store.store_bytes() <= to {
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

/// The bodies stage 1 may drop, lowest value first (ADR 0025 §7): full
/// events older than [`BODY_GRACE_SECS`] that save at least
/// [`EVICT_MIN_SAVING`], not `react`s, not protected; judged with
/// `grouping`.
pub fn plan(store: &Store, grouping: &Grouping, keep: &Keep, now: u64) -> Result<Vec<String>> {
    let grace = now.saturating_sub(BODY_GRACE_SECS);
    let mut candidates: Vec<RowHead> = Vec::new();
    let mut after = 0;
    loop {
        let rows = store.rows_after_seq(after, SCAN)?;
        let Some(last) = rows.last() else { break };
        after = last.seq;
        candidates.extend(rows.into_iter().filter(|r| {
            !r.opaque
                && r.len >= EVICT_MIN_SAVING
                && r.ts < grace
                && r.kind != crate::event::Kind::React.as_str()
                && !keep.protects(r)
        }));
        if candidates.len() > 2 * MAX_PLAN {
            candidates.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
            candidates.truncate(MAX_PLAN);
        }
    }
    if candidates.len() > MAX_PLAN {
        candidates.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        candidates.truncate(MAX_PLAN);
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut ranked: Vec<Value1> = Vec::with_capacity(candidates.len());
    for chunk in candidates.chunks(BATCH) {
        let ids: Vec<String> = chunk.iter().map(|r| r.id.clone()).collect();
        let stored = store.full_events(&ids)?;
        // Exactly what the body saves: the full event against its
        // tombstone.
        let worth: Vec<&crate::store::Stored> = stored
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
            .collect();
        let events: Vec<&crate::event::Event> = worth.iter().map(|s| &*s.event).collect();
        let signals = crate::signals::compute_at(store, grouping, &events, now)?;
        let mut roots: Vec<String> = events.iter().map(|e| e.thread_root().to_string()).collect();
        roots.sort();
        roots.dedup();
        let entries = crate::knowledge::compute_at(store, grouping, &roots, &signals, now)?.ids();
        for event in events {
            let s = signals.get(&event.id).cloned().unwrap_or_default();
            ranked.push(Value1 {
                entry: entries.contains(&event.id),
                status: s.status.rank(),
                score: s.score,
                ts: event.ts,
                id: event.id.clone(),
            });
        }
    }
    ranked.sort_by(by_value);
    Ok(ranked.into_iter().map(|v| v.id).collect())
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
        g.reply(30, &joined, Kind::Answer, json!({}));
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
        let expected: Vec<String> = [&contested, &older, &newer, &voted, &confirmed, &entry]
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(plan, expected);
        for left_out in [
            &own, &joined, &in_joined, &announce, &react, &short, &recent,
        ] {
            assert!(!plan.contains(&left_out.id), "{}", left_out.kind);
        }
        // And the store drops what the plan says, nothing else.
        let done = g.store.evict_bodies(&plan, &keep(&g, &[30]), g.ts).unwrap();
        assert_eq!(done.rows, expected.len());
        assert!(g.store.get(&own.id).unwrap().is_some());
        assert!(g.store.get(&in_joined.id).unwrap().is_some());
        assert!(g.store.get(&react.id).unwrap().is_some());
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
