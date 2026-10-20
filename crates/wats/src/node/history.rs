//! Bounded history sync, newest first (ADR 0025).
//!
//! One history task per network, in the background: gossip, MCP and the
//! runner never wait for it.
//!
//! - **The initial fill** (watermark 0, no range): kept-forever kinds, the
//!   node's own posts and threads, digests and their signals, then
//!   everything else newest first, down to `sync.initial_max`. Where it
//!   stops is the cut.
//! - **Catch-up** (after a stop or an offline period): a pending range from
//!   now down to the watermark, walked newest first too, resumable from its
//!   cursor. The watermark moves only once no range is pending.
//! - **Older peers** (no neighbor speaks `sync_desc`): ascending windows of
//!   7 days, then 14, 28…, each read until it meets the history the
//!   previous one brought, until the budget is spent.
//!
//! A node serves its history honestly: a descending page stops at the top
//! of a hole (its cut, a pending or skipped range) with a `gap`, and an
//! ascending pull from below a hole is refused, so that no peer takes a
//! partial history for a complete one.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Result, bail};
use iroh::EndpointId;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::{MAX_CLOCK_SKEW_SECS, Node, Origin, now_secs};
use crate::proto::{self, Gap, Request, Response};
use crate::settings::InitialMax;
use crate::store::{PHASE_WINDOW, RangeState, SyncRange};

/// The kinds every node keeps whatever its budget (ADR 0025 §4, phase 0):
/// halts, key successions, retractions, announces and vouches.
pub const KEEP_KINDS: [&str; 7] = [
    crate::halt::KIND_HALT,
    crate::halt::KIND_APPROVE,
    crate::halt::KIND_UNHALT,
    "rotate",
    crate::retract::KIND,
    "announce",
    "vouch",
];

/// The fill's phases, in order (ADR 0025 §4): kept-forever kinds, the
/// node's own posts and their threads (phase 0), digests and their signals
/// (phase 1), then everything newest first ([`PHASE_WINDOW`], phase 2).
pub const PHASE_KEEP: &str = "keep";
pub const PHASE_OWN: &str = "own";
pub const PHASE_KNOWLEDGE: &str = "knowledge";

/// Sub-caps, in percent of the fill's budget: phase 0 (each of its two
/// steps), phase 1, and one key (with its rotations) in phase 2.
const KEEP_SHARE: u64 = 10;
const KNOWLEDGE_SHARE: u64 = 20;
const AUTHOR_SHARE: u64 = 2;

/// Events older than this when they arrive are history: they wake no
/// agent beyond `agent.catchup`, and `wait` leaves them out (ADR 0025 §4).
pub const HISTORY_AGE: Duration = Duration::from_secs(24 * 3600);

/// A page that takes longer moves the walk to the next peer.
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// On-demand fetches (ADR 0025 §6): at most this many peers, this long
/// each, and this many a minute from this node.
const GET_PEERS: usize = 3;
const GET_TIMEOUT: Duration = Duration::from_secs(3);
const GETS_PER_MIN: usize = 60;
/// Ids per `get` of the fill (threads, signals).
const GET_BATCH: usize = proto::MAX_GET_IDS;
/// Most events a `get` answers per root (`thread`) and in all
/// (`signals`), and most bytes in one answer, under the 16 MiB response.
const GET_THREAD_MAX: usize = 500;
const GET_SIGNALS_MAX: usize = 2000;
const GET_MAX_BYTES: usize = 15 * 1024 * 1024;
/// The first window asked from older peers; each next one doubles.
const OLDER_WINDOW_SECS: u64 = 7 * 24 * 3600;

/// How the history task behaves (tests shorten or slow it).
#[derive(Debug, Clone)]
pub struct Tuning {
    /// With no neighbor speaking `sync_desc` this long after the first
    /// neighbor came up, the node falls back to older peers' ascending
    /// pulls.
    pub older_peers_wait: Duration,
    /// Events asked per page.
    pub page: u32,
    /// A pause after each page (tests: to stop a walk half-way).
    pub page_pause: Duration,
    /// Pending ranges are tried again this often.
    pub retry: Duration,
    /// Test only: behave as 0.4.0 on `/wats/1` (no `sync_desc`, no `get`,
    /// no history task, an ascending pull from every new neighbor).
    pub legacy: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            older_peers_wait: Duration::from_secs(30),
            page: proto::MAX_SYNC_PAGE,
            page_pause: Duration::ZERO,
            retry: Duration::from_secs(60),
            legacy: false,
        }
    }
}

/// What the history task of one network is doing, for `status`.
#[derive(Debug, Default)]
struct Progress {
    running: bool,
    /// Pulling from older peers (no `sync_desc`).
    older_peers: bool,
    /// The peer that served the last page: asked first on demand.
    source: Option<EndpointId>,
    /// When the first neighbor came up.
    first_neighbor: Option<std::time::Instant>,
    /// Neighbors that came up, for the cheap ascending check once no range
    /// is pending.
    check: HashSet<EndpointId>,
}

/// The history tasks' shared state.
#[derive(Debug, Default)]
pub struct History {
    kicks: std::sync::Mutex<HashMap<String, Arc<Notify>>>,
    progress: std::sync::Mutex<HashMap<String, Progress>>,
    /// On-demand fetches in the last minute.
    gets: std::sync::Mutex<std::collections::VecDeque<std::time::Instant>>,
}

/// Which holes a request is served around (ADR 0025 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holes {
    /// Every one: a descending page stops at the first.
    All,
    /// The cut and the skipped ranges: a `partial` request takes what a
    /// pending range holds.
    Intentional,
    /// Those and the initial fill still running: an ascending pull from
    /// below them is refused. Catch-up ranges are not, as a 0.4.0 node
    /// served what it held during its own catch-up.
    Ascending,
}

/// A `sync` request as [`Node::serve_sync`] serves it.
#[derive(Debug)]
pub struct SyncAsk {
    pub after: (u64, String),
    pub before: Option<(u64, String)>,
    pub kinds: Vec<String>,
    pub authors: Vec<String>,
    pub partial: bool,
    pub limit: u32,
}

/// How a walk down a range ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// A peer had nothing more: the range is held.
    Complete,
    /// The budget is spent.
    Budget,
    /// Every peer stopped at a hole of its own: nobody reachable holds more.
    Gapped,
    /// No peer answered well: try again later.
    Failed,
}

/// What one walk asks for.
#[derive(Debug, Default)]
struct Walk {
    kinds: Vec<String>,
    authors: Vec<String>,
    /// Only events with `ts` at or above this (the range's `lo`).
    floor: u64,
    /// Stop once the store holds this many bytes.
    stop_at: Option<u64>,
    /// Phase 2: one key's share, in bytes of events.
    author_cap: Option<u64>,
}

/// State of one pass of the task.
#[derive(Debug, Default)]
struct Pass {
    /// Peers that answered a gap at the top of the range: they hold less
    /// than this node there.
    behind: HashSet<EndpointId>,
    /// Peers that served pages.
    served: HashSet<EndpointId>,
    /// Bytes stored per key chain in phase 2.
    author_bytes: HashMap<String, u64>,
    /// Key -> the first key of its rotation chain.
    chains: HashMap<String, String>,
    /// Neighbors without `sync_desc`.
    older: Vec<EndpointId>,
}

impl Pass {
    fn chain_of<'a>(&'a self, author: &'a str) -> &'a str {
        self.chains.get(author).map_or(author, String::as_str)
    }
}

/// Map every key to the first key of its rotation chain (as held here).
fn chains(rotations: &[(String, String)]) -> HashMap<String, String> {
    let previous: HashMap<&str, &str> = rotations
        .iter()
        .map(|(old, new)| (new.as_str(), old.as_str()))
        .collect();
    let mut out = HashMap::new();
    for key in previous.keys() {
        let mut root = *key;
        for _ in 0..256 {
            let Some(prev) = previous.get(root) else {
                break;
            };
            root = prev;
        }
        out.insert((*key).to_string(), root.to_string());
    }
    out
}

/// `a * percent / 100` without overflow.
fn percent(a: u64, percent: u64) -> u64 {
    a.saturating_mul(percent) / 100
}

/// `2026-09-14`.
pub fn date(ts: u64) -> String {
    let (y, m, d, ..) = crate::runs::civil(ts);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The fields of a wire event a page is checked on.
struct Head<'a> {
    ts: u64,
    id: &'a str,
    net: &'a str,
    kind: &'a str,
    author: &'a str,
}

fn head(value: &Value) -> Option<Head<'_>> {
    Some(Head {
        ts: value.get("ts")?.as_u64()?,
        id: value.get("id")?.as_str()?,
        net: value.get("net")?.as_str()?,
        kind: value.get("kind")?.as_str()?,
        author: value.get("author")?.as_str()?,
    })
}

/// The `history` line of `status` and the banner (ADR 0025 §8), from
/// [`Node::history_json`]'s value: "syncing, back to 2026-09-14, 812.0 MiB
/// / 2.5 GiB", "back to 2026-08-02, 2.3 GiB / 5.0 GiB", "full, back to
/// …", and the gaps.
pub fn line(history: &Value) -> String {
    let back = history["oldest_ts"]
        .as_u64()
        .map(|ts| format!("back to {}", date(ts)));
    let bytes = history["bytes"].as_u64().map(crate::node::format_bytes);
    let budget = history["budget"].as_u64().map(crate::node::format_bytes);
    let size = match (bytes, budget) {
        (Some(b), Some(m)) => Some(format!("{b} / {m}")),
        (Some(b), None) => Some(b),
        _ => None,
    };
    let mut parts: Vec<String> = Vec::new();
    match history["state"].as_str().unwrap_or("done") {
        "syncing" => parts.push("syncing".into()),
        "older_peers" => parts.push("syncing from older peers".into()),
        "full" => parts.push("full".into()),
        "none" => parts.push("not synced yet".into()),
        _ => {}
    }
    parts.extend(back);
    parts.extend(size);
    let mut text = parts.join(", ");
    for range in history["ranges"].as_array().into_iter().flatten() {
        let (Some(lo), Some(hi)) = (range["lo_ts"].as_u64(), range["hi_ts"].as_u64()) else {
            continue;
        };
        let why = match range["state"].as_str() {
            Some("pending") => "resuming",
            Some("skipped") => "skipped: budget",
            _ => continue,
        };
        text.push_str(&format!(" · gap {} – {} ({why})", date(lo), date(hi)));
    }
    text
}

/// The same, short, for `wats ps`: `back to 2026-08-02`, `syncing, back
/// to …`, `full`, with `+gaps` when there are.
pub fn short(history: &Value) -> String {
    let back = history["oldest_ts"].as_u64().map(date);
    let mut text = match (history["state"].as_str().unwrap_or("done"), back) {
        ("none", _) => "-".to_string(),
        ("full", _) => "full".to_string(),
        ("syncing" | "older_peers", Some(back)) => format!("syncing, back to {back}"),
        ("syncing" | "older_peers", None) => "syncing".to_string(),
        (_, Some(back)) => format!("back to {back}"),
        (_, None) => "-".to_string(),
    };
    if history["ranges"].as_array().is_some_and(|r| !r.is_empty()) {
        text.push_str(" +gaps");
    }
    text
}

/// The `history` of `status` read from a store, with no node running (the
/// banner, printed before the node starts).
pub fn stored_json(
    store: &crate::store::Store,
    net: &str,
    full: bool,
    cap: Option<u64>,
) -> Result<Value> {
    let ranges = store.sync_ranges(net)?;
    let watermark = store.sync_watermark(net)?;
    let pending = ranges.iter().any(|r| r.state == RangeState::Pending);
    let holes = ranges.iter().any(|r| r.hole_top().is_some());
    let state = if pending {
        "syncing"
    } else if watermark == 0 {
        "none"
    } else if full && !holes {
        "full"
    } else {
        "done"
    };
    Ok(view(store, net, state, &ranges, cap))
}

fn view(
    store: &crate::store::Store,
    net: &str,
    state: &str,
    ranges: &[SyncRange],
    budget: Option<u64>,
) -> Value {
    let cut = ranges
        .iter()
        .filter(|r| r.state == RangeState::Cut)
        .map(|r| r.cursor_ts)
        .max();
    let shown: Vec<Value> = ranges
        .iter()
        .filter(|r| {
            matches!(r.state, RangeState::Pending | RangeState::Skipped) && r.phase.is_empty()
        })
        .filter_map(|r| {
            Some(json!({
                "state": r.state.as_str(),
                "lo_ts": r.hole_lo(),
                "hi_ts": r.hole_top()?,
            }))
        })
        .collect();
    json!({
        "state": state,
        "oldest_ts": store.oldest_ts(net).ok().flatten(),
        "cut_ts": cut,
        "ranges": shown,
        "bytes": store.store_bytes(),
        "budget": budget,
    })
}

impl Node {
    /// The share of `storage.max` for the store, in bytes; `None`: no
    /// limit.
    pub fn events_share(&self) -> Option<u64> {
        self.config
            .storage_max
            .map(|max| percent(max, self.config.events_share.clamp(1, 100)))
    }

    /// The budget of an initial fill or a catch-up, in store bytes
    /// (`sync.initial_max`); `None`: no limit, a full node.
    pub fn history_budget(&self) -> Option<u64> {
        match self.config.initial_max {
            InitialMax::Auto => self.events_share().map(|s| s / 2).filter(|b| *b > 0),
            InitialMax::Bytes(0) => None,
            InitialMax::Bytes(n) => Some(n),
        }
    }

    /// The holes in this node's history of `net`: `(lo_ts, top_ts)`, whose
    /// complete history it does not hold (ADR 0025 §3).
    pub fn history_holes(&self, net: &str) -> Vec<(u64, u64)> {
        self.holes(net, Holes::All)
    }

    /// The holes of `net` that count for `which` request.
    fn holes(&self, net: &str, which: Holes) -> Vec<(u64, u64)> {
        self.store
            .sync_ranges(net)
            .unwrap_or_default()
            .iter()
            .filter(|r| match which {
                Holes::All => true,
                Holes::Intentional => r.state != RangeState::Pending,
                // A catch-up range is opened on every new neighbor: it is
                // recent, and today's nodes serve what they hold there too.
                Holes::Ascending => r.state != RangeState::Pending || !r.phase.is_empty(),
            })
            .filter_map(|r| Some((r.hole_lo(), r.hole_top()?)))
            .collect()
    }

    /// Whether this node holds the complete history of every network it
    /// joined (`history_full`): no budget, a fill done, and no hole.
    pub fn history_full(&self) -> bool {
        self.history_budget().is_none()
            && !self.config.history.legacy
            && self.joined_nets().iter().all(|net| {
                self.store.sync_watermark(net).unwrap_or(0) > 0
                    && self.history_holes(net).is_empty()
            })
    }

    /// What this node says about itself in `hello`.
    pub fn our_info(&self) -> proto::NodeInfo {
        let mut info = proto::NodeInfo::ours_greased();
        if self.config.history.legacy {
            info.features
                .retain(|f| f != proto::FEATURE_SYNC_DESC && f != proto::FEATURE_GET);
        } else if self.history_full() {
            info.features.push(proto::FEATURE_HISTORY_FULL.to_string());
        }
        info
    }

    /// The `history` of `status` for `net` (ADR 0025 §8).
    pub fn history_json(&self, net: &str) -> Value {
        let ranges = self.store.sync_ranges(net).unwrap_or_default();
        let (running, older) = self
            .history
            .progress
            .lock()
            .ok()
            .and_then(|p| p.get(net).map(|p| (p.running, p.older_peers)))
            .unwrap_or((false, false));
        let pending = ranges.iter().any(|r| r.state == RangeState::Pending);
        let watermark = self.store.sync_watermark(net).unwrap_or(0);
        let state = if older && (running || pending) {
            "older_peers"
        } else if pending || (running && watermark == 0) {
            "syncing"
        } else if watermark == 0 {
            "none"
        } else if self.history_budget().is_none() && self.history_holes(net).is_empty() {
            "full"
        } else {
            "done"
        };
        // While a fill or a catch-up runs, its budget; after, the events
        // share, which live traffic keeps filling.
        let budget = if state == "syncing" || state == "older_peers" {
            self.history_budget()
        } else {
            self.events_share()
        };
        view(&self.store, net, state, &ranges, budget)
    }

    /// The `history` of every network joined, by network.
    pub fn history_status(&self) -> Value {
        Value::Object(
            self.joined_nets()
                .into_iter()
                .map(|net| {
                    let value = self.history_json(&net);
                    (net, value)
                })
                .collect(),
        )
    }

    /// Start the history task of `net`, at node start.
    pub(super) fn spawn_history(self: &Arc<Self>, net: &str) {
        if self.config.history.legacy {
            return;
        }
        let kick = Arc::new(Notify::new());
        if let Ok(mut kicks) = self.history.kicks.lock() {
            kicks.insert(net.to_string(), Arc::clone(&kick));
        }
        let weak = Arc::downgrade(self);
        let name = format!("history {net}");
        let net = net.to_string();
        tokio::spawn(async move {
            crate::supervise::supervise(&name, move || {
                history_loop(weak.clone(), net.clone(), Arc::clone(&kick))
            })
            .await;
        });
    }

    /// A neighbor came up on `net` (or the network changed): walk what is
    /// pending, then check what `peers` hold (ADR 0025 §5).
    pub(super) fn kick_history(&self, net: &str, peers: &[EndpointId]) {
        if let Ok(mut progress) = self.history.progress.lock() {
            let p = progress.entry(net.to_string()).or_default();
            p.first_neighbor.get_or_insert_with(std::time::Instant::now);
            p.check.extend(peers.iter().copied());
        }
        let kick = self
            .history
            .kicks
            .lock()
            .ok()
            .and_then(|k| k.get(net).cloned());
        if let Some(kick) = kick {
            kick.notify_one();
        }
    }

    fn set_progress(&self, net: &str, f: impl FnOnce(&mut Progress)) {
        if let Ok(mut progress) = self.history.progress.lock() {
            f(progress.entry(net.to_string()).or_default());
        }
    }

    fn neighbors_of(&self, net: &str) -> Vec<EndpointId> {
        let mut peers: Vec<EndpointId> = self
            .neighbors
            .lock()
            .map(|n| {
                n.get(net)
                    .map(|s| s.iter().copied().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        peers.sort();
        peers
    }

    fn has_feature(&self, peer: &EndpointId, feature: &str) -> bool {
        self.peer_info(peer)
            .is_some_and(|i| i.features.iter().any(|f| f == feature))
    }

    /// Peers to walk history from: those with `sync_desc`, `history_full`
    /// ones first.
    fn history_peers(&self, net: &str) -> Vec<EndpointId> {
        let me = self.endpoint_id();
        let mut peers: Vec<EndpointId> = self.neighbors_of(net);
        peers.extend(self.known_peers());
        peers.sort();
        peers.dedup();
        peers.retain(|p| *p != me && self.has_feature(p, proto::FEATURE_SYNC_DESC));
        peers.sort_by_key(|p| !self.has_feature(p, proto::FEATURE_HISTORY_FULL));
        peers
    }

    /// One pass of the history task of `net`: open a range if needed, walk
    /// the pending ones newest first, then check the new neighbors.
    async fn history_pass(&self, net: &str) -> Result<()> {
        let neighbors = self.neighbors_of(net);
        if neighbors.is_empty() {
            return Ok(());
        }
        for peer in &neighbors {
            if self.peer_info(peer).is_none()
                && let Ok(Err(e)) =
                    tokio::time::timeout(Duration::from_secs(5), self.hello(*peer)).await
            {
                debug!(peer = %peer.fmt_short(), "hello failed: {e:#}");
            }
        }
        let peers = self.history_peers(net);
        let older = peers.is_empty();
        if older {
            // Give the neighbors a little time to show they speak
            // `sync_desc` (ADR 0025 §4, older peers only).
            let waited = self
                .history
                .progress
                .lock()
                .ok()
                .and_then(|p| p.get(net).and_then(|p| p.first_neighbor))
                .map_or(Duration::MAX, |t| t.elapsed());
            let wait = self.config.history.older_peers_wait;
            if waited < wait {
                let left = wait.saturating_sub(waited);
                let kick = self
                    .history
                    .kicks
                    .lock()
                    .ok()
                    .and_then(|k| k.get(net).cloned());
                tokio::spawn(async move {
                    tokio::time::sleep(left).await;
                    if let Some(kick) = kick {
                        kick.notify_one();
                    }
                });
                return Ok(());
            }
        }
        self.open_range(net)?;
        let mut pass = Pass::default();
        self.set_progress(net, |p| {
            p.running = true;
            p.older_peers = older;
        });
        let outcome = self.walk_pending(net, &neighbors, &peers, &mut pass).await;
        self.set_progress(net, |p| p.running = false);
        outcome?;
        if let Some(watermark) = self.store.settle_sync_ranges(net)? {
            info!(%net, watermark, "history: caught up");
        }
        // The cheap ascending check with the new neighbors (ADR 0025 §5),
        // once nothing is pending: the union of what peers hold.
        let pending = self
            .store
            .sync_ranges(net)?
            .iter()
            .any(|r| r.state == RangeState::Pending);
        if pending {
            return Ok(());
        }
        let check: Vec<EndpointId> = self
            .history
            .progress
            .lock()
            .ok()
            .and_then(|mut p| p.get_mut(net).map(|p| p.check.drain().collect()))
            .unwrap_or_default();
        for peer in check {
            if pass.served.contains(&peer) || pass.behind.contains(&peer) {
                continue;
            }
            match self.sync_from(peer, net).await {
                Ok(n) => debug!(%net, peer = %peer.fmt_short(), new = n, "checked"),
                Err(e) => debug!(%net, peer = %peer.fmt_short(), "check failed: {e:#}"),
            }
        }
        Ok(())
    }

    /// Open the range a pass walks (ADR 0025 §5): the initial fill when the
    /// node holds no history of `net`, otherwise a catch-up range from the
    /// watermark (or the newest pending range) up to now.
    fn open_range(&self, net: &str) -> Result<()> {
        let now = now_secs();
        let top = now.saturating_add(MAX_CLOCK_SKEW_SECS);
        let watermark = self.store.sync_watermark(net)?;
        let ranges = self.store.sync_ranges(net)?;
        let base_bytes = self.store.store_bytes();
        if watermark == 0 && ranges.is_empty() {
            info!(%net, budget = ?self.history_budget(), "history: initial fill, newest first");
            self.store.add_sync_range(&SyncRange {
                id: 0,
                net: net.to_string(),
                lo_ts: 0,
                hi_ts: top,
                cursor_ts: top,
                cursor_id: String::new(),
                state: RangeState::Pending,
                phase: PHASE_KEEP.into(),
                opened: now,
                base_bytes,
            })?;
            return Ok(());
        }
        let pending: Vec<&SyncRange> = ranges
            .iter()
            .filter(|r| r.state == RangeState::Pending)
            .collect();
        if watermark == 0 && pending.is_empty() {
            return Ok(());
        }
        let newest = pending.iter().map(|r| r.hi_ts).max();
        if newest.is_some_and(|hi| hi >= top) {
            return Ok(());
        }
        // A catch-up range not walked yet: it only grows.
        if let Some(fresh) = pending
            .iter()
            .find(|r| r.phase.is_empty() && Some(r.hi_ts) == newest && r.cursor_ts == r.hi_ts)
        {
            let mut range = (*fresh).clone();
            (range.hi_ts, range.cursor_ts, range.opened) = (top, top, now);
            return self.store.update_sync_range(&range);
        }
        let lo = match newest {
            Some(hi) => hi.saturating_sub(MAX_CLOCK_SKEW_SECS),
            None => watermark.saturating_sub(MAX_CLOCK_SKEW_SECS),
        };
        self.store.add_sync_range(&SyncRange {
            id: 0,
            net: net.to_string(),
            lo_ts: lo,
            hi_ts: top,
            cursor_ts: top,
            cursor_id: String::new(),
            state: RangeState::Pending,
            phase: String::new(),
            opened: now,
            base_bytes,
        })?;
        Ok(())
    }

    /// Walk the pending ranges of `net`, newest first.
    async fn walk_pending(
        &self,
        net: &str,
        neighbors: &[EndpointId],
        peers: &[EndpointId],
        pass: &mut Pass,
    ) -> Result<()> {
        let ranges: Vec<SyncRange> = self
            .store
            .sync_ranges(net)?
            .into_iter()
            .filter(|r| r.state == RangeState::Pending)
            .collect();
        // Older neighbors, for what no `sync_desc` peer holds.
        pass.older = neighbors
            .iter()
            .copied()
            .filter(|p| !self.has_feature(p, proto::FEATURE_SYNC_DESC))
            .collect();
        for mut range in ranges {
            let end = if peers.is_empty() {
                self.walk_older(&mut range, neighbors, pass, None).await?
            } else if range.phase.is_empty() {
                self.walk_catch_up(&mut range, peers, pass).await?
            } else {
                self.walk_fill(&mut range, peers, pass).await?
            };
            if end == End::Failed {
                debug!(%net, "history: no peer could serve the range; kept pending");
                break;
            }
        }
        Ok(())
    }

    /// A catch-up range, newest first down to its `lo`.
    async fn walk_catch_up(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        pass: &mut Pass,
    ) -> Result<End> {
        let walk = Walk {
            floor: range.lo_ts,
            stop_at: self
                .history_budget()
                .map(|b| range.base_bytes.saturating_add(b)),
            ..Walk::default()
        };
        let mut end = self.walk(range, peers, &walk, pass).await?;
        if end == End::Gapped && !pass.older.is_empty() {
            let older = pass.older.clone();
            return self
                .walk_older(range, &older, pass, Some(range.cursor_ts))
                .await;
        }
        if end == End::Gapped {
            end = End::Complete;
        }
        match end {
            End::Complete | End::Gapped => range.state = RangeState::Done,
            End::Budget => {
                warn!(
                    net = %range.net,
                    "history: catch-up larger than sync.initial_max; {} – {} skipped",
                    date(range.lo_ts),
                    date(range.cursor_ts)
                );
                range.state = RangeState::Skipped;
                range.hi_ts = range.cursor_ts;
            }
            End::Failed => {}
        }
        self.store.update_sync_range(range)?;
        Ok(end)
    }

    /// The initial fill, phase by phase (ADR 0025 §4).
    async fn walk_fill(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        pass: &mut Pass,
    ) -> Result<End> {
        let budget = self.history_budget();
        let net = range.net.clone();
        loop {
            let start = self.store.store_bytes();
            let cap = |share: u64| budget.map(|b| start.saturating_add(percent(b, share)));
            let (end, next) = match range.phase.as_str() {
                PHASE_KEEP => {
                    let walk = Walk {
                        kinds: KEEP_KINDS.iter().map(|k| k.to_string()).collect(),
                        stop_at: cap(KEEP_SHARE),
                        ..Walk::default()
                    };
                    (self.walk(range, peers, &walk, pass).await?, PHASE_OWN)
                }
                PHASE_OWN => {
                    let mut authors: Vec<String> = self.local_authors().into_iter().collect();
                    authors.sort();
                    authors.truncate(proto::MAX_SYNC_FILTER);
                    let walk = Walk {
                        authors: authors.clone(),
                        stop_at: cap(KEEP_SHARE),
                        ..Walk::default()
                    };
                    let mut end = self.walk(range, peers, &walk, pass).await?;
                    if end != End::Failed && end != End::Budget {
                        let roots = self.store.threads_of(&net, &authors, 10_000)?;
                        if !self
                            .get_batches(&net, &roots, "thread", peers, walk.stop_at)
                            .await
                        {
                            end = End::Budget;
                        }
                    }
                    (end, PHASE_KNOWLEDGE)
                }
                PHASE_KNOWLEDGE => {
                    let walk = Walk {
                        kinds: vec![crate::knowledge::DIGEST_KIND.to_string()],
                        stop_at: cap(KNOWLEDGE_SHARE),
                        ..Walk::default()
                    };
                    let mut end = self.walk(range, peers, &walk, pass).await?;
                    if end != End::Failed && end != End::Budget {
                        let digests =
                            self.store
                                .ids_of_kind(&net, crate::knowledge::DIGEST_KIND, 100_000)?;
                        if !self
                            .get_batches(&net, &digests, "signals", peers, walk.stop_at)
                            .await
                        {
                            end = End::Budget;
                        }
                    }
                    (end, PHASE_WINDOW)
                }
                _ => {
                    pass.chains = chains(&self.store.rotations()?);
                    let walk = Walk {
                        stop_at: budget.map(|b| range.base_bytes.saturating_add(b)),
                        author_cap: budget.map(|b| percent(b, AUTHOR_SHARE)),
                        ..Walk::default()
                    };
                    let end = self.walk(range, peers, &walk, pass).await?;
                    if end == End::Gapped && !pass.older.is_empty() {
                        let older = pass.older.clone();
                        let held = Some(range.cursor_ts);
                        return self.walk_older(range, &older, pass, held).await;
                    }
                    return self.end_fill(range, end);
                }
            };
            match end {
                End::Failed => return Ok(End::Failed),
                End::Budget => info!(
                    %net,
                    phase = %range.phase,
                    "history: the phase reached its share of the budget; moving on"
                ),
                End::Gapped => debug!(%net, phase = %range.phase, "history: no peer holds more"),
                End::Complete => {}
            }
            range.phase = next.to_string();
            (range.cursor_ts, range.cursor_id) = (range.hi_ts, String::new());
            self.store.update_sync_range(range)?;
        }
    }

    /// The end of the fill: done, with a cut where the budget stopped it
    /// (or where no peer held more).
    fn end_fill(&self, range: &mut SyncRange, end: End) -> Result<End> {
        let net = range.net.clone();
        // Gapped: no peer holds older history (their own cuts), so neither
        // does this node.
        let cut = match end {
            End::Failed => return Ok(end),
            End::Complete => false,
            End::Budget | End::Gapped => true,
        };
        if cut {
            info!(
                %net,
                cut = %date(range.cursor_ts),
                "history: initial fill done; older history is not held here (sync.initial_max)"
            );
            self.store.add_sync_range(&SyncRange {
                id: 0,
                net: net.clone(),
                lo_ts: 0,
                hi_ts: range.cursor_ts,
                cursor_ts: range.cursor_ts,
                cursor_id: range.cursor_id.clone(),
                state: RangeState::Cut,
                phase: String::new(),
                opened: now_secs(),
                base_bytes: 0,
            })?;
        } else {
            info!(%net, "history: initial fill done, the whole history");
        }
        range.state = RangeState::Done;
        self.store.update_sync_range(range)?;
        Ok(end)
    }

    /// Walk down `range` from its cursor, page by page and peer by peer,
    /// persisting the cursor after each page: first from peers that hold
    /// the range complete, then, when every peer stopped at a range of its
    /// own still pending, what they hold (`partial`): what the reachable
    /// swarm holds, as an ascending pull always took.
    async fn walk(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        pass: &mut Pass,
    ) -> Result<End> {
        match self.walk_round(range, peers, walk, pass, false).await? {
            End::Gapped => self.walk_round(range, peers, walk, pass, true).await,
            end => Ok(end),
        }
    }

    async fn walk_round(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        pass: &mut Pass,
        partial: bool,
    ) -> Result<End> {
        let (mut failed, mut gapped) = (false, false);
        for &peer in peers {
            if !partial && pass.behind.contains(&peer) {
                gapped = true;
                continue;
            }
            loop {
                if walk
                    .stop_at
                    .is_some_and(|stop| self.store.store_bytes() >= stop)
                {
                    return Ok(End::Budget);
                }
                let request = Request::Sync {
                    net: range.net.clone(),
                    after_ts: walk.floor,
                    after_id: String::new(),
                    before_ts: Some(range.cursor_ts),
                    before_id: Some(range.cursor_id.clone()),
                    kinds: walk.kinds.clone(),
                    authors: walk.authors.clone(),
                    partial,
                    limit: self.config.history.page,
                };
                let answer = tokio::time::timeout(
                    PAGE_TIMEOUT,
                    proto::call_solving(self.endpoint(), peer, &request),
                )
                .await;
                let (events, more, gap, seen) = match answer {
                    Ok(Ok(Response::SyncPage {
                        events,
                        more,
                        gap,
                        seen,
                    })) => (events, more, gap, seen),
                    other => {
                        debug!(peer = %peer.fmt_short(), "history page: {other:?}");
                        failed = true;
                        break;
                    }
                };
                if let Err(e) = self
                    .take_page(range, peer, &events, &seen, walk, pass, true)
                    .await
                {
                    debug!(peer = %peer.fmt_short(), "history page refused: {e:#}");
                    failed = true;
                    break;
                }
                pass.served.insert(peer);
                self.set_progress(&range.net, |p| p.source = Some(peer));
                if let Some(Gap { hi_ts }) = gap {
                    // Everything above the gap came: the next peer goes on
                    // below it.
                    let below = hi_ts.saturating_add(1);
                    if below < range.cursor_ts {
                        (range.cursor_ts, range.cursor_id) = (below, String::new());
                    }
                    if hi_ts >= range.hi_ts {
                        pass.behind.insert(peer);
                    }
                    self.store.update_sync_range(range)?;
                    gapped = true;
                    break;
                }
                self.store.update_sync_range(range)?;
                if !self.config.history.page_pause.is_zero() {
                    tokio::time::sleep(self.config.history.page_pause).await;
                }
                if !more || events.is_empty() {
                    if walk
                        .stop_at
                        .is_some_and(|stop| self.store.store_bytes() >= stop)
                    {
                        return Ok(End::Budget);
                    }
                    return Ok(End::Complete);
                }
            }
        }
        Ok(if failed || !gapped {
            End::Failed
        } else {
            End::Gapped
        })
    }

    /// Check a page from `peer` and store its events: strictly descending
    /// (or ascending) from the range's cursor, the right network, the
    /// filters respected (ADR 0025 §3). The cursor follows.
    #[allow(clippy::too_many_arguments)]
    async fn take_page(
        &self,
        range: &mut SyncRange,
        peer: EndpointId,
        events: &[Value],
        seen: &[u64],
        walk: &Walk,
        pass: &mut Pass,
        descending: bool,
    ) -> Result<()> {
        let mut cursor = (range.cursor_ts, range.cursor_id.clone());
        for value in events {
            let Some(h) = head(value) else {
                bail!("peer sent a malformed sync page");
            };
            let ordered = if descending {
                (h.ts, h.id) < (cursor.0, cursor.1.as_str()) && h.ts >= walk.floor
            } else {
                (h.ts, h.id) > (cursor.0, cursor.1.as_str())
            };
            if !ordered || h.net != range.net {
                bail!("peer sent an out-of-order sync page");
            }
            if (!walk.kinds.is_empty() && !walk.kinds.iter().any(|k| k == h.kind))
                || (!walk.authors.is_empty() && !walk.authors.iter().any(|a| a == h.author))
            {
                bail!("peer sent events outside the filter");
            }
            cursor = (h.ts, h.id.to_string());
        }
        if seen.len() == events.len() {
            let reports: Vec<(String, u64)> = events
                .iter()
                .zip(seen)
                .filter_map(|(v, s)| Some((head(v)?.author.to_string(), *s)))
                .collect();
            if let Err(e) = self.store.record_seen(&peer.to_string(), &reports) {
                debug!("seen reports not recorded: {e:#}");
            }
        }
        for value in events {
            let Some(h) = head(value) else { continue };
            let (author, kind, id) = (h.author.to_string(), h.kind.to_string(), h.id.to_string());
            (range.cursor_ts, range.cursor_id) = (h.ts, id.clone());
            // One key's share of phase 2 (ADR 0025 §4); kept-forever kinds
            // are never skipped.
            let size = serde_json::to_vec(value).map_or(0, |b| b.len() as u64);
            let chain = pass.chain_of(&author).to_string();
            if let Some(cap) = walk.author_cap
                && !KEEP_KINDS.contains(&kind.as_str())
                && pass.author_bytes.get(&chain).copied().unwrap_or(0) >= cap
            {
                continue;
            }
            match self.accept_value(value.clone(), Origin::Pulled).await {
                Ok(Some((_, true))) => {
                    if walk.author_cap.is_some() {
                        let used = pass.author_bytes.entry(chain).or_default();
                        *used = used.saturating_add(size);
                    }
                }
                Ok(_) => {}
                Err(e) => debug!(%id, "history: rejected event: {e:#}"),
            }
        }
        Ok(())
    }

    /// `get` the threads or signals of `ids`, in batches, until `stop_at`
    /// store bytes. Returns `false` when it stopped there.
    async fn get_batches(
        &self,
        net: &str,
        ids: &[String],
        include: &str,
        peers: &[EndpointId],
        stop_at: Option<u64>,
    ) -> bool {
        let peers: Vec<EndpointId> = peers
            .iter()
            .copied()
            .filter(|p| self.has_feature(p, proto::FEATURE_GET))
            .collect();
        for batch in ids.chunks(GET_BATCH) {
            if stop_at.is_some_and(|stop| self.store.store_bytes() >= stop) {
                return false;
            }
            for peer in &peers {
                match self
                    .get_from(*peer, net, batch, include, PAGE_TIMEOUT)
                    .await
                {
                    Ok(_) => break,
                    Err(e) => debug!(peer = %peer.fmt_short(), "history get: {e:#}"),
                }
            }
        }
        true
    }

    /// One `get` to `peer`; stores what comes. Returns how many events were
    /// new.
    async fn get_from(
        &self,
        peer: EndpointId,
        net: &str,
        ids: &[String],
        include: &str,
        timeout: Duration,
    ) -> Result<usize> {
        let request = Request::Get {
            net: net.to_string(),
            ids: ids.to_vec(),
            include: include.to_string(),
        };
        let answer = tokio::time::timeout(
            timeout,
            proto::call_solving(self.endpoint(), peer, &request),
        )
        .await;
        let (events, seen) = match answer {
            Ok(Ok(Response::SyncPage { events, seen, .. })) => (events, seen),
            Ok(Ok(other)) => bail!("unexpected get response: {other:?}"),
            Ok(Err(e)) => return Err(e),
            Err(_) => bail!("no answer within {timeout:?}"),
        };
        if seen.len() == events.len() {
            let reports: Vec<(String, u64)> = events
                .iter()
                .zip(&seen)
                .filter_map(|(v, s)| Some((head(v)?.author.to_string(), *s)))
                .collect();
            let _ = self.store.record_seen(&peer.to_string(), &reports);
        }
        let mut new: usize = 0;
        for value in events {
            if value.get("net").and_then(Value::as_str) != Some(net) {
                bail!("peer sent an event of another network");
            }
            match self.accept_value(value, Origin::Pulled).await {
                Ok(Some((_, true))) => new = new.saturating_add(1),
                Ok(_) => {}
                Err(e) => debug!("get: rejected event: {e:#}"),
            }
        }
        Ok(new)
    }

    /// Older peers (no `sync_desc`): ascending windows from now - 7 days,
    /// then 14, 28…, each read until it meets the history already held,
    /// until the budget is spent or the range's `lo` is reached. With no
    /// history `held` yet, the first window is read to its end whatever
    /// the budget. Also what is left of a range once every `sync_desc`
    /// peer stopped at a cut of its own.
    async fn walk_older(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        pass: &mut Pass,
        mut held: Option<u64>,
    ) -> Result<End> {
        let now = now_secs();
        let floor = range.lo_ts;
        let budget = self.history_budget();
        pass.chains = chains(&self.store.rotations()?);
        let author_cap = if range.phase.is_empty() {
            None
        } else {
            budget.map(|b| percent(b, AUTHOR_SHARE))
        };
        let stop_at = budget.map(|b| range.base_bytes.saturating_add(b));
        let mut span = OLDER_WINDOW_SECS;
        // `held`: the oldest `ts` held from the windows read so far.
        let mut first = held.is_none();
        let end = loop {
            let start = now.saturating_sub(span).max(floor);
            let walk = Walk {
                floor: start,
                stop_at: if first { None } else { stop_at },
                author_cap,
                ..Walk::default()
            };
            match self.older_window(range, peers, &walk, held, pass).await? {
                (End::Complete, oldest) => {
                    held = match (held, oldest) {
                        (Some(h), Some(o)) => Some(h.min(o)),
                        (h, o) => h.or(o),
                    };
                    // Held from `start` up.
                    if start < range.cursor_ts {
                        (range.cursor_ts, range.cursor_id) = (start, String::new());
                    }
                    self.store.update_sync_range(range)?;
                }
                (end, _) => break end,
            }
            if start <= floor {
                break End::Complete;
            }
            first = false;
            span = span.saturating_mul(2);
        };
        match (end, range.phase.is_empty()) {
            (End::Failed, _) => {}
            // The fill: done, with a cut at the bottom of the last window
            // read in full.
            (_, false) => {
                range.phase = PHASE_WINDOW.into();
                return self.end_fill(range, end);
            }
            (End::Budget, true) => {
                range.state = RangeState::Skipped;
                range.hi_ts = range.cursor_ts;
                self.store.update_sync_range(range)?;
            }
            (_, true) => {
                range.state = RangeState::Done;
                self.store.update_sync_range(range)?;
            }
        }
        Ok(end)
    }

    /// One ascending window from an older peer, from `walk.floor` up to the
    /// history already `held` (or the end). Returns how it ended and the
    /// oldest `ts` it brought.
    async fn older_window(
        &self,
        range: &SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        held: Option<u64>,
        pass: &mut Pass,
    ) -> Result<(End, Option<u64>)> {
        let mut oldest = None;
        // A cursor of its own: ascending, from the window's start.
        let mut window = range.clone();
        (window.cursor_ts, window.cursor_id) = (walk.floor.saturating_sub(1), "~".repeat(64));
        if walk.floor == 0 {
            window.cursor_id = String::new();
        }
        for &peer in peers {
            loop {
                if walk
                    .stop_at
                    .is_some_and(|stop| self.store.store_bytes() >= stop)
                {
                    return Ok((End::Budget, oldest));
                }
                let request = Request::sync_after(
                    &range.net,
                    window.cursor_ts,
                    &window.cursor_id,
                    self.config.history.page,
                );
                let answer = tokio::time::timeout(
                    PAGE_TIMEOUT,
                    proto::call_solving(self.endpoint(), peer, &request),
                )
                .await;
                let (mut events, more, seen) = match answer {
                    Ok(Ok(Response::SyncPage {
                        events, more, seen, ..
                    })) => (events, more, seen),
                    other => {
                        debug!(peer = %peer.fmt_short(), "older peer page: {other:?}");
                        break;
                    }
                };
                // Stop at the history the previous window brought.
                let reached = held.and_then(|h| {
                    events
                        .iter()
                        .position(|v| head(v).is_some_and(|x| x.ts >= h))
                });
                if let Some(at) = reached {
                    events.truncate(at);
                }
                let seen = if seen.len() >= events.len() {
                    seen.get(..events.len()).unwrap_or_default().to_vec()
                } else {
                    Vec::new()
                };
                let floor_walk = Walk {
                    floor: 0,
                    author_cap: walk.author_cap,
                    ..Walk::default()
                };
                if let Err(e) = self
                    .take_page(&mut window, peer, &events, &seen, &floor_walk, pass, false)
                    .await
                {
                    debug!(peer = %peer.fmt_short(), "older peer page refused: {e:#}");
                    break;
                }
                if oldest.is_none() {
                    oldest = events.first().and_then(|v| head(v).map(|h| h.ts));
                }
                pass.served.insert(peer);
                self.set_progress(&range.net, |p| p.source = Some(peer));
                if !self.config.history.page_pause.is_zero() {
                    tokio::time::sleep(self.config.history.page_pause).await;
                }
                if reached.is_some() || !more || events.is_empty() {
                    return Ok((End::Complete, oldest));
                }
            }
        }
        Ok((End::Failed, oldest))
    }

    /// Serve a `sync` request for `net`, honestly (ADR 0025 §3): an
    /// ascending pull from below a hole is refused; a descending page stops
    /// at the top of the first hole, with a `gap`.
    pub(super) fn serve_sync(&self, net: &str, ask: SyncAsk) -> Response {
        let SyncAsk {
            after,
            before,
            mut kinds,
            authors,
            partial,
            limit,
        } = ask;
        if kinds.len() > proto::MAX_SYNC_FILTER || authors.len() > proto::MAX_SYNC_FILTER {
            return Response::Refused {
                reason: format!("at most {} kinds and authors", proto::MAX_SYNC_FILTER),
            };
        }
        // Serving a halt: halt events only (ADR 0016 §3).
        if self.halt_serving.load(std::sync::atomic::Ordering::Relaxed) {
            let halt: Vec<String> = crate::halt::KINDS.iter().map(|k| k.to_string()).collect();
            kinds = if kinds.is_empty() {
                halt
            } else {
                kinds.into_iter().filter(|k| halt.contains(k)).collect()
            };
            if kinds.is_empty() {
                return Response::SyncPage {
                    events: Vec::new(),
                    more: false,
                    gap: None,
                    seen: Vec::new(),
                };
            }
        }
        let limit = limit.clamp(1, proto::MAX_SYNC_PAGE);
        // A `partial` requester takes what is held in a pending range.
        let holes = match (partial, before.is_some()) {
            (true, _) => self.holes(net, Holes::Intentional),
            (false, true) => self.holes(net, Holes::All),
            (false, false) => self.holes(net, Holes::Ascending),
        };
        let mut stop = None;
        match &before {
            None => {
                if let Some(top) = holes
                    .iter()
                    .map(|(_, top)| *top)
                    .filter(|top| *top >= after.0)
                    .max()
                {
                    return Response::Refused {
                        reason: format!("history before {} not held here", date(top)),
                    };
                }
            }
            Some((before_ts, _)) => {
                stop = holes
                    .iter()
                    .filter(|(lo, top)| *lo <= *before_ts && *top >= after.0)
                    .map(|(_, top)| (*top).min(*before_ts))
                    .max();
            }
        }
        let query = crate::store::PageQuery {
            net: net.to_string(),
            after,
            before,
            above_ts: stop,
            kinds,
            authors,
            limit: limit as usize,
        };
        match self.store.sync_page(&query) {
            Ok(rows) => {
                let more = rows.len() == limit as usize;
                let (events, seen) = rows.into_iter().unzip();
                Response::SyncPage {
                    events,
                    more,
                    gap: stop.filter(|_| !more).map(|hi_ts| Gap { hi_ts }),
                    seen,
                }
            }
            Err(e) => Response::Refused {
                reason: format!("{e:#}"),
            },
        }
    }

    /// Serve a `get` request (ADR 0025 §3).
    pub(super) fn serve_get(&self, net: &str, ids: &[String], include: &str) -> Response {
        if ids.len() > proto::MAX_GET_IDS {
            return Response::Refused {
                reason: format!("at most {} ids", proto::MAX_GET_IDS),
            };
        }
        let rows = match include {
            "event" => self.store.events_by_ids(net, ids),
            "thread" => {
                let mut rows = Vec::new();
                for id in ids {
                    match self.store.thread_rows(net, id, GET_THREAD_MAX) {
                        Ok(more) => rows.extend(more),
                        Err(e) => {
                            return Response::Refused {
                                reason: format!("{e:#}"),
                            };
                        }
                    }
                }
                Ok(rows)
            }
            "signals" => self.store.signal_events(net, ids, GET_SIGNALS_MAX),
            other => {
                return Response::Refused {
                    reason: format!("unknown include {other:?}: event, thread or signals"),
                };
            }
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                return Response::Refused {
                    reason: format!("{e:#}"),
                };
            }
        };
        let halt = self.halt_serving.load(std::sync::atomic::Ordering::Relaxed);
        let (mut events, mut seen) = (Vec::new(), Vec::new());
        let (mut ids, mut bytes) = (HashSet::new(), 0usize);
        for (value, at) in rows {
            let Some(h) = head(&value) else { continue };
            if (halt && !crate::halt::KINDS.contains(&h.kind)) || !ids.insert(h.id.to_string()) {
                continue;
            }
            bytes = bytes.saturating_add(serde_json::to_vec(&value).map_or(0, |b| b.len()));
            if bytes > GET_MAX_BYTES {
                break;
            }
            events.push(value);
            seen.push(at);
        }
        Response::SyncPage {
            events,
            more: false,
            gap: None,
            seen,
        }
    }

    /// Fetch events on demand (ADR 0025 §6): `get` from the peer the
    /// history came from, then `history_full` peers, then neighbors with
    /// `get`; at most 3 peers, 3 s each, skipping peers that failed lately.
    /// Returns whether any peer answered.
    pub async fn fetch_on_demand(&self, ids: &[String], include: &str) -> bool {
        if self.config.history.legacy || ids.is_empty() {
            return false;
        }
        {
            let Ok(mut gets) = self.history.gets.lock() else {
                return false;
            };
            let now = std::time::Instant::now();
            while gets
                .front()
                .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
            {
                gets.pop_front();
            }
            if gets.len() >= GETS_PER_MIN {
                debug!("on-demand fetch skipped: {GETS_PER_MIN} a minute already");
                return false;
            }
            gets.push_back(now);
        }
        let ids: Vec<String> = ids.iter().take(proto::MAX_GET_IDS).cloned().collect();
        let me = self.endpoint_id();
        for net in self.joined_nets() {
            let source = self
                .history
                .progress
                .lock()
                .ok()
                .and_then(|p| p.get(&net).and_then(|p| p.source));
            let mut others: Vec<EndpointId> = self.neighbors_of(&net);
            others.extend(self.known_peers());
            others.sort_by_key(|p| !self.has_feature(p, proto::FEATURE_HISTORY_FULL));
            let mut peers: Vec<EndpointId> = Vec::new();
            for peer in source.into_iter().chain(others) {
                if peer != me
                    && !peers.contains(&peer)
                    && self.has_feature(&peer, proto::FEATURE_GET)
                    && !self.provider_backing_off(&peer)
                {
                    peers.push(peer);
                }
            }
            let mut answered = false;
            for peer in peers.into_iter().take(GET_PEERS) {
                match self.get_from(peer, &net, &ids, include, GET_TIMEOUT).await {
                    Ok(_) => {
                        self.provider_ok(peer);
                        answered = true;
                        if ids
                            .iter()
                            .all(|id| self.store.get_raw(id).ok().flatten().is_some())
                        {
                            return true;
                        }
                    }
                    Err(e) => {
                        debug!(peer = %peer.fmt_short(), "on-demand get: {e:#}");
                        self.provider_failed(peer);
                    }
                }
            }
            if answered {
                return true;
            }
        }
        false
    }

    /// Whether a pending range of `net` remains.
    fn history_pending(&self, net: &str) -> bool {
        self.store
            .sync_ranges(net)
            .is_ok_and(|r| r.iter().any(|r| r.state == RangeState::Pending))
    }
}

/// The history task of one network: a pass on every kick (a neighbor up,
/// a network change), and every `retry` while a range is pending.
async fn history_loop(weak: Weak<Node>, net: String, kick: Arc<Notify>) {
    loop {
        let wait = {
            let Some(node) = weak.upgrade() else { return };
            if node.endpoint().is_closed() {
                return;
            }
            if let Err(e) = node.history_pass(&net).await {
                debug!(%net, "history pass: {e:#}");
            }
            if node.history_pending(&net) {
                node.config.history.retry
            } else {
                Duration::from_secs(3600)
            }
        };
        let _ = tokio::time::timeout(wait, kick.notified()).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn rotation_chains_lead_to_the_first_key() {
        let rotations = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("x".to_string(), "y".to_string()),
        ];
        let chains = chains(&rotations);
        assert_eq!(chains["c"], "a");
        assert_eq!(chains["b"], "a");
        assert_eq!(chains["y"], "x");
        assert!(!chains.contains_key("a"));
    }

    #[test]
    fn the_history_line_reads_like_the_adr() {
        let day = 86_400;
        let syncing = json!({
            "state": "syncing", "oldest_ts": 20_710 * day, "bytes": 812u64 << 20,
            "budget": 5u64 << 29, "ranges": [],
        });
        assert_eq!(
            line(&syncing),
            "syncing, back to 2026-09-14, 812.0 MiB / 2.5 GiB"
        );
        let done = json!({
            "state": "done", "oldest_ts": 20_667 * day, "bytes": 1u64 << 30,
            "budget": 5u64 << 30,
            "ranges": [{"state": "skipped", "lo_ts": 20_697 * day, "hi_ts": 20_716 * day}],
        });
        assert_eq!(
            line(&done),
            "back to 2026-08-02, 1.0 GiB / 5.0 GiB · gap 2026-09-01 – 2026-09-20 (skipped: budget)"
        );
        let full =
            json!({"state": "full", "oldest_ts": 0, "bytes": 1024, "budget": null, "ranges": []});
        assert_eq!(line(&full), "full, back to 1970-01-01, 1.0 KiB");
    }
}
