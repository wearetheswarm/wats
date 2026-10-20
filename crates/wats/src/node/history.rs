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
//! - **Older peers** (neighbors without `sync_desc`), always pulled when
//!   any is up: ascending windows of 7 days, then 14, 28…, each read until
//!   it meets the history the previous one brought, until the budget is
//!   spent.
//!
//! A range is complete only when a peer this node trusts (one of its
//! configured peers or bootstrap nodes) has nothing more below the cursor,
//! or when two distinct peers say so at the same cursor. A `partial`
//! answer, a `gap` or one untrusted peer's word leaves it pending; after
//! [`Tuning::max_attempts`] passes it is closed as what it is: a cut (the
//! fill) or a skipped range (a catch-up), never as held.
//!
//! A node serves its history honestly: a descending page stops at the top
//! of a hole (its cut, a pending or skipped range) with a `gap`, and an
//! ascending pull from below a hole is refused, so that no peer takes a
//! partial history for a complete one.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
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
/// Most events a `get` answers per root (`thread`) and in all, and most
/// bytes in one answer, under the 16 MiB response.
const GET_THREAD_MAX: usize = 500;
const GET_ROWS_MAX: usize = 2000;
const GET_MAX_BYTES: usize = 15 * 1024 * 1024;
/// The first window asked from older peers; each next one doubles.
const OLDER_WINDOW_SECS: u64 = 7 * 24 * 3600;

/// How the history task behaves (tests shorten or slow it).
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Passes with neighbors up a range may stay unconfirmed (gaps,
    /// partial answers, one untrusted peer's word) before it is closed: a
    /// cut for the fill, a skipped range for a catch-up. A pending range
    /// is a hole that refuses 0.4.0 pulls, so it never stays forever.
    pub max_attempts: u32,
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
            max_attempts: 5,
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
    /// Neighbors that came up, for the cheap ascending check once no range
    /// is pending.
    check: HashSet<EndpointId>,
    /// The node came back on the network (its first neighbor, a network
    /// change): the next pass opens a catch-up range.
    catch_up: bool,
    /// Unconfirmed passes per pending range id.
    attempts: HashMap<i64, u32>,
    /// Bytes stored per key chain by the fill, across its passes (ADR
    /// 0025 §4: one key's share).
    author_bytes: HashMap<String, u64>,
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
    /// Every one: what `status` and `history_full` count.
    All,
    /// A descending page stops at the first of the cut, the skipped
    /// ranges and a fill still running, and an ascending pull from below
    /// one is refused. Not a pending catch-up range: recent history, which
    /// a node online all along cannot vouch for either (gossip loses
    /// messages), and which 0.4.0 nodes served during their own catch-up.
    Serving,
    /// The cut and the skipped ranges: a `partial` request takes what a
    /// pending range holds.
    Intentional,
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
    /// Confirmed: a trusted peer, or two distinct peers at the same cursor,
    /// had nothing more. The range is held.
    Complete,
    /// The budget is spent.
    Budget,
    /// Peers answered, but nothing confirms the range is held: gaps,
    /// `partial` answers, one untrusted peer's word.
    Open,
    /// No peer answered: try again later.
    Failed,
}

/// Peers that said they hold nothing below a cursor (ADR 0025 §9): a
/// trusted one settles it, otherwise two distinct ones at the same cursor.
#[derive(Debug, Default)]
struct Claims {
    at: Option<(u64, String)>,
    peers: HashSet<EndpointId>,
}

impl Claims {
    /// `peer` has nothing below `cursor`: whether the range is complete.
    fn claim(&mut self, peer: EndpointId, trusted: bool, cursor: (u64, &str)) -> bool {
        if trusted {
            return true;
        }
        if self.at.as_ref().map(|(ts, id)| (*ts, id.as_str())) != Some(cursor) {
            self.at = Some((cursor.0, cursor.1.to_string()));
            self.peers.clear();
        }
        self.peers.insert(peer);
        self.peers.len() >= 2
    }
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
    /// Bytes stored per key chain by the fill (kept across passes).
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

/// What a walk from the `sync_desc` peers and one from the older peers
/// say together: the budget stops both; either one confirmed is enough.
fn combine(desc: End, older: End) -> End {
    match (desc, older) {
        (End::Budget, _) | (_, End::Budget) => End::Budget,
        (End::Complete, _) | (_, End::Complete) => End::Complete,
        (End::Open, _) | (_, End::Open) => End::Open,
        _ => End::Failed,
    }
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
    /// (`sync.initial_max`, at most the events share, so that a fill never
    /// pushes into ADR 0012's ceiling); `None`: no limit, a full node.
    pub fn history_budget(&self) -> Option<u64> {
        let share = self.events_share();
        match self.config.initial_max {
            InitialMax::Auto => share.map(|s| s / 2).filter(|b| *b > 0),
            InitialMax::Bytes(0) => None,
            InitialMax::Bytes(n) => Some(share.map_or(n, |s| n.min(s))),
        }
    }

    /// `sync.initial_max` and the events share it is clamped to, when it
    /// is larger.
    pub fn initial_max_clamped(&self) -> Option<(u64, u64)> {
        match (self.config.initial_max, self.events_share()) {
            (InitialMax::Bytes(n), Some(share)) if n > share => Some((n, share)),
            _ => None,
        }
    }

    /// The holes in this node's history of `net`: `(lo_ts, top_ts)`, whose
    /// complete history it does not hold (ADR 0025 §3).
    pub fn history_holes(&self, net: &str) -> Vec<(u64, u64)> {
        self.holes(net, Holes::All)
    }

    /// The holes of `net` that count for `which` request.
    fn holes(&self, net: &str, which: Holes) -> Vec<(u64, u64)> {
        let ranges = self.store.sync_ranges(net).unwrap_or_default();
        let mut holes: Vec<(u64, u64)> = ranges
            .iter()
            .filter(|r| match which {
                Holes::All => true,
                Holes::Intentional => r.state != RangeState::Pending,
                Holes::Serving => r.state != RangeState::Pending || !r.phase.is_empty(),
            })
            .filter_map(|r| Some((r.hole_lo(), r.hole_top()?)))
            .collect();
        // No history yet (before the first pass): nothing below now is
        // held, as with a fill still to run.
        if ranges.is_empty()
            && which != Holes::Intentional
            && self.store.sync_watermark(net).unwrap_or(0) == 0
        {
            holes.push((0, now_secs().saturating_add(MAX_CLOCK_SKEW_SECS)));
        }
        holes
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
        // Back from a stop: the first pass with a neighbor catches up.
        self.note_offline(net);
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
    /// pending, then check what `peers` hold (ADR 0025 §5). `catch_up`:
    /// the node was offline on `net` until now (its first neighbor, a
    /// network change), so a catch-up range opens; a neighbor joining
    /// those already up only gets the cheap check, as gossip was live.
    pub(super) fn kick_history(&self, net: &str, peers: &[EndpointId], catch_up: bool) {
        if let Ok(mut progress) = self.history.progress.lock() {
            let p = progress.entry(net.to_string()).or_default();
            p.check.extend(peers.iter().copied());
            p.catch_up |= catch_up;
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

    /// The node was offline on `net` until now (started, or lost every
    /// neighbor): the next pass with a neighbor opens a catch-up range.
    /// Noted at once, before any `hello`, so that no pass in between
    /// takes the neighbor without it.
    pub(super) fn note_offline(&self, net: &str) {
        self.set_progress(net, |p| p.catch_up = true);
    }

    fn set_progress<T>(&self, net: &str, f: impl FnOnce(&mut Progress) -> T) -> Option<T> {
        let mut progress = self.history.progress.lock().ok()?;
        Some(f(progress.entry(net.to_string()).or_default()))
    }

    /// The current gossip neighbors of `net`: members of it, up now.
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

    /// Whether this node takes `peer`'s word that it holds nothing more
    /// (ADR 0025 §9): one of its configured peers or bootstrap nodes.
    /// Anyone else's `history_full` is a hint for the order, no more.
    fn trusted(&self, peer: &EndpointId) -> bool {
        self.seed_ids.contains(peer)
    }

    /// The neighbors of `net` to walk history from, in the order to ask
    /// them: trusted `history_full` peers, trusted peers, then the others
    /// in a random order (an endpoint id can be ground), those that claim
    /// `history_full` first. Split into those with `sync_desc` and the
    /// older ones.
    fn history_peers(&self, net: &str) -> (Vec<EndpointId>, Vec<EndpointId>) {
        let me = self.endpoint_id();
        let shuffle = std::collections::hash_map::RandomState::new();
        let mut peers = self.neighbors_of(net);
        peers.retain(|p| *p != me);
        peers.sort_by_cached_key(|p| {
            (
                !self.trusted(p),
                !self.has_feature(p, proto::FEATURE_HISTORY_FULL),
                shuffle.hash_one(p),
            )
        });
        peers
            .into_iter()
            .partition(|p| self.has_feature(p, proto::FEATURE_SYNC_DESC))
    }

    /// One pass of the history task of `net`: open a range if needed, walk
    /// the pending ones newest first, then check the new neighbors.
    async fn history_pass(&self, net: &str) -> Result<()> {
        let neighbors = self.neighbors_of(net);
        if neighbors.is_empty() {
            return Ok(());
        }
        // What each neighbor speaks: a hello still in flight, a few seconds
        // at most. One that does not answer is asked as an older peer.
        for peer in &neighbors {
            if self.peer_info(peer).is_none()
                && let Ok(Err(e)) =
                    tokio::time::timeout(Duration::from_secs(5), self.hello(*peer)).await
            {
                debug!(peer = %peer.fmt_short(), "hello failed: {e:#}");
            }
        }
        let (peers, older) = self.history_peers(net);
        // The watermark before this pass: the check below never starts
        // from one this pass moved.
        let watermark = self.store.sync_watermark(net)?;
        let catch_up = self
            .set_progress(net, |p| std::mem::take(&mut p.catch_up))
            .unwrap_or(true);
        self.open_range(net, catch_up)?;
        let mut pass = Pass {
            older,
            author_bytes: self
                .set_progress(net, |p| std::mem::take(&mut p.author_bytes))
                .unwrap_or_default(),
            ..Pass::default()
        };
        self.set_progress(net, |p| {
            p.running = true;
            p.older_peers = peers.is_empty();
        });
        let outcome = self.walk_pending(net, &peers, &mut pass).await;
        let fill_running = self
            .store
            .sync_ranges(net)?
            .iter()
            .any(|r| r.state == RangeState::Pending && !r.phase.is_empty());
        self.set_progress(net, |p| {
            p.running = false;
            if fill_running {
                p.author_bytes = std::mem::take(&mut pass.author_bytes);
            }
        });
        outcome?;
        if let Some(watermark) = self.store.settle_sync_ranges(net)? {
            info!(%net, watermark, "history: caught up");
        }
        // The cheap ascending check with the new neighbors (ADR 0025 §5),
        // once nothing is pending: the union of what peers hold.
        if self.history_pending(net) {
            return Ok(());
        }
        let check: Vec<EndpointId> = self
            .set_progress(net, |p| p.check.drain().collect())
            .unwrap_or_default();
        // From the watermark before this pass; after the fill, from the top
        // of its cut, or not at all when the whole history was confirmed
        // (an ascending pull from 0 would bring the oldest first).
        let from = if watermark > 0 {
            Some(watermark)
        } else {
            self.store
                .sync_ranges(net)?
                .iter()
                .filter(|r| r.state == RangeState::Cut)
                .filter_map(SyncRange::hole_top)
                .max()
        };
        let Some(from) = from else {
            return Ok(());
        };
        let stop_at = self
            .history_budget()
            .map(|b| self.store.store_bytes().saturating_add(b));
        for peer in check {
            if pass.served.contains(&peer) || pass.behind.contains(&peer) {
                continue;
            }
            match self.sync_from_with(peer, net, from, stop_at).await {
                Ok(n) => debug!(%net, peer = %peer.fmt_short(), new = n, "checked"),
                Err(e) => debug!(%net, peer = %peer.fmt_short(), "check failed: {e:#}"),
            }
        }
        Ok(())
    }

    /// Open the range a pass walks (ADR 0025 §5): the initial fill when the
    /// node holds no history of `net`, otherwise, after an offline period
    /// (`catch_up`), a catch-up range from the watermark (or the newest
    /// pending range) up to now.
    fn open_range(&self, net: &str, catch_up: bool) -> Result<()> {
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
        // Nothing missed: online all along, or a watermark ahead of now.
        if !catch_up || (watermark == 0 && pending.is_empty()) || watermark > now {
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

    /// Walk the pending ranges of `net`, newest first: from `peers` (with
    /// `sync_desc`), then from the older neighbors of the pass.
    async fn walk_pending(&self, net: &str, peers: &[EndpointId], pass: &mut Pass) -> Result<()> {
        let ranges: Vec<SyncRange> = self
            .store
            .sync_ranges(net)?
            .into_iter()
            .filter(|r| r.state == RangeState::Pending)
            .collect();
        for mut range in ranges {
            let end = if range.phase.is_empty() {
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

    /// A catch-up range, newest first down to its `lo`, then what the older
    /// neighbors hold from its `lo` up (ADR 0025 §5).
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
        let mut end = if peers.is_empty() {
            End::Failed
        } else {
            self.walk(range, peers, &walk, pass).await?
        };
        if end != End::Budget && !pass.older.is_empty() {
            let older = pass.older.clone();
            end = combine(end, self.walk_older(range, &older, pass, None).await?);
        }
        self.finish(range, end)
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
        // One key's share, for the whole fill but its own posts.
        let author_cap = budget.map(|b| percent(b, AUTHOR_SHARE));
        pass.chains = chains(&self.store.rotations()?);
        // Older peers filter nothing: the window only.
        if peers.is_empty() && range.phase != PHASE_WINDOW {
            range.phase = PHASE_WINDOW.into();
            (range.cursor_ts, range.cursor_id) = (range.hi_ts, String::new());
            self.store.update_sync_range(range)?;
        }
        loop {
            let start = self.store.store_bytes();
            let cap = |share: u64| budget.map(|b| start.saturating_add(percent(b, share)));
            let (end, next) = match range.phase.as_str() {
                PHASE_KEEP => {
                    let walk = Walk {
                        kinds: KEEP_KINDS.iter().map(|k| k.to_string()).collect(),
                        stop_at: cap(KEEP_SHARE),
                        author_cap,
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
                        author_cap,
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
                    let walk = Walk {
                        stop_at: budget.map(|b| range.base_bytes.saturating_add(b)),
                        author_cap,
                        ..Walk::default()
                    };
                    let mut end = if peers.is_empty() {
                        End::Failed
                    } else {
                        self.walk(range, peers, &walk, pass).await?
                    };
                    if end != End::Budget && !pass.older.is_empty() {
                        let older = pass.older.clone();
                        let held = (range.cursor_ts < range.hi_ts).then_some(range.cursor_ts);
                        end = combine(end, self.walk_older(range, &older, pass, held).await?);
                    }
                    return self.finish(range, end);
                }
            };
            match end {
                End::Failed => return self.finish(range, End::Failed),
                End::Budget => info!(
                    %net,
                    phase = %range.phase,
                    "history: the phase reached its share of the budget; moving on"
                ),
                End::Open => {
                    debug!(%net, phase = %range.phase, "history: phase not confirmed; moving on")
                }
                End::Complete => {}
            }
            range.phase = next.to_string();
            (range.cursor_ts, range.cursor_id) = (range.hi_ts, String::new());
            self.store.update_sync_range(range)?;
        }
    }

    /// How many unconfirmed passes `range` has had, this one included.
    fn bump_attempts(&self, net: &str, range: i64) -> u32 {
        self.set_progress(net, |p| {
            let n = p.attempts.entry(range).or_default();
            *n = n.saturating_add(1);
            *n
        })
        .unwrap_or(u32::MAX)
    }

    /// The end of a walk over `range`: complete, closed (a cut for the
    /// fill, a skipped range for a catch-up: the budget, or no peer
    /// confirmed it after [`Tuning::max_attempts`] passes), or still
    /// pending.
    fn finish(&self, range: &mut SyncRange, end: End) -> Result<End> {
        let net = range.net.clone();
        let fill = !range.phase.is_empty();
        let close = match end {
            End::Complete => false,
            End::Budget => true,
            End::Open | End::Failed => {
                let attempts = self.bump_attempts(&net, range.id);
                if attempts < self.config.history.max_attempts {
                    self.store.update_sync_range(range)?;
                    return Ok(end);
                }
                warn!(
                    %net,
                    "history: no peer confirmed {} – {} after {attempts} passes; not counted as held",
                    date(range.lo_ts),
                    date(range.cursor_ts)
                );
                true
            }
        };
        self.set_progress(&net, |p| {
            p.attempts.remove(&range.id);
            if fill {
                p.author_bytes.clear();
            }
        });
        if fill {
            // Before its window, nothing below the fill's top is held but
            // the kinds of its first phases.
            let top = if range.phase == PHASE_WINDOW {
                range.cursor_ts
            } else {
                range.hi_ts
            };
            if close {
                info!(
                    %net,
                    cut = %date(top),
                    "history: initial fill done; older history is not held here"
                );
                self.store.add_sync_range(&SyncRange {
                    id: 0,
                    net: net.clone(),
                    lo_ts: 0,
                    hi_ts: top,
                    cursor_ts: top,
                    cursor_id: String::new(),
                    state: RangeState::Cut,
                    phase: String::new(),
                    opened: now_secs(),
                    base_bytes: 0,
                })?;
            } else {
                info!(%net, "history: initial fill done, the whole history");
            }
            range.phase = PHASE_WINDOW.into();
            range.state = RangeState::Done;
        } else if close {
            warn!(
                net = %range.net,
                "history: catch-up not held: {} – {} skipped",
                date(range.lo_ts),
                date(range.cursor_ts)
            );
            range.state = RangeState::Skipped;
            range.hi_ts = range.cursor_ts;
        } else {
            range.state = RangeState::Done;
        }
        self.store.update_sync_range(range)?;
        Ok(end)
    }

    /// Walk down `range` from its cursor, page by page and peer by peer,
    /// persisting the cursor after each page: first from peers that hold
    /// the range complete, then, when that confirms nothing, what they hold
    /// in ranges of their own still pending (`partial`). A `partial`
    /// answer never completes the range.
    async fn walk(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        pass: &mut Pass,
    ) -> Result<End> {
        match self.walk_round(range, peers, walk, pass, false).await? {
            End::Open => match self.walk_round(range, peers, walk, pass, true).await? {
                End::Budget => Ok(End::Budget),
                _ => Ok(End::Open),
            },
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
        let mut answered = false;
        let mut claims = Claims::default();
        for &peer in peers {
            if !partial && pass.behind.contains(&peer) {
                answered = true;
                continue;
            }
            loop {
                self.still_running()?;
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
                        // One peer failing (or not a member) vetoes nothing:
                        // the next one is asked.
                        debug!(peer = %peer.fmt_short(), "history page: {other:?}");
                        break;
                    }
                };
                if let Err(e) = self
                    .take_page(range, peer, &events, &seen, walk, pass, true)
                    .await
                {
                    debug!(peer = %peer.fmt_short(), "history page refused: {e:#}");
                    break;
                }
                answered = true;
                pass.served.insert(peer);
                self.set_progress(&range.net, |p| p.source = Some(peer));
                if let Some(Gap { hi_ts }) = gap {
                    // The peer holds no more below here. The cursor stays at
                    // what came: a lying gap skips no history, the next peer
                    // goes on from there.
                    if hi_ts >= range.hi_ts && events.is_empty() {
                        pass.behind.insert(peer);
                    }
                    self.store.update_sync_range(range)?;
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
                    if !partial
                        && claims.claim(
                            peer,
                            self.trusted(&peer),
                            (range.cursor_ts, &range.cursor_id),
                        )
                    {
                        return Ok(End::Complete);
                    }
                    // Not confirmed yet: the next peer, from the same cursor.
                    break;
                }
            }
        }
        Ok(if answered { End::Open } else { End::Failed })
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
        let seen = (seen.len() == events.len()).then_some(seen);
        let mut reports: Vec<(String, u64)> = Vec::new();
        for (i, value) in events.iter().enumerate() {
            let Some(h) = head(value) else { continue };
            let (author, id) = (h.author.to_string(), h.id.to_string());
            (range.cursor_ts, range.cursor_id) = (h.ts, id.clone());
            // One key's share (ADR 0025 §4), kept-forever kinds included:
            // phase 0 fetched those already.
            let size = serde_json::to_vec(value).map_or(0, |b| b.len() as u64);
            let chain = pass.chain_of(&author).to_string();
            if let Some(cap) = walk.author_cap
                && pass.author_bytes.get(&chain).copied().unwrap_or(0) >= cap
            {
                continue;
            }
            match self.accept_value(value.clone(), Origin::Pulled).await {
                Ok(Some((event, true))) => {
                    if walk.author_cap.is_some() {
                        let used = pass.author_bytes.entry(chain).or_default();
                        *used = used.saturating_add(size);
                    }
                    if let Some(at) = seen.and_then(|s| s.get(i)) {
                        reports.push((event.author().to_string(), *at));
                    }
                }
                Ok(_) => {}
                // Stopped half-way: the cursor must not pass what was not
                // stored.
                Err(e) if self.endpoint().is_closed() => return Err(e),
                Err(e) => debug!(%id, "history: rejected event: {e:#}"),
            }
        }
        if let Err(e) = self.store.record_seen(&peer.to_string(), &reports) {
            debug!("seen reports not recorded: {e:#}");
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

    /// One `get` to `peer`; stores what comes, as long as it is what was
    /// asked: the events of `ids`, their threads, or the replies to them
    /// and the posts citing them. Returns how many events were new.
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
        let asked: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let field = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        let wanted = |v: &Value| -> bool {
            let is = |key: &str| field(v, key).is_some_and(|x| asked.contains(x.as_str()));
            match include {
                "thread" => is("id") || is("thread"),
                "signals" => {
                    is("parent")
                        || v.get("refs").and_then(Value::as_array).is_some_and(|r| {
                            r.iter()
                                .filter_map(Value::as_str)
                                .any(|x| asked.contains(x))
                        })
                }
                _ => is("id"),
            }
        };
        let seen = (seen.len() == events.len()).then_some(seen);
        let mut reports: Vec<(String, u64)> = Vec::new();
        let mut new: usize = 0;
        for (i, value) in events.into_iter().enumerate() {
            if value.get("net").and_then(Value::as_str) != Some(net) {
                bail!("peer sent an event of another network");
            }
            if !wanted(&value) {
                debug!(peer = %peer.fmt_short(), "get: an event not asked for, not stored");
                continue;
            }
            match self.accept_value(value, Origin::Pulled).await {
                Ok(Some((event, true))) => {
                    new = new.saturating_add(1);
                    if let Some(at) = seen.as_ref().and_then(|s| s.get(i)) {
                        reports.push((event.author().to_string(), *at));
                    }
                }
                Ok(_) => {}
                Err(e) => debug!("get: rejected event: {e:#}"),
            }
        }
        let _ = self.store.record_seen(&peer.to_string(), &reports);
        Ok(new)
    }

    /// Older peers (no `sync_desc`): ascending windows from now - 7 days,
    /// then 14, 28…, each read until it meets the history already held,
    /// until the budget is spent or the range's `lo` is reached. A fill
    /// with no history `held` yet reads the first window to its end
    /// whatever the budget. The range is complete only if every window was
    /// read to its end from a trusted peer; the cursor moves down only
    /// then.
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
        let fill = !range.phase.is_empty();
        let author_cap = if fill {
            budget.map(|b| percent(b, AUTHOR_SHARE))
        } else {
            None
        };
        let stop_at = budget.map(|b| range.base_bytes.saturating_add(b));
        let mut span = OLDER_WINDOW_SECS;
        // `held`: the oldest `ts` held from the windows read so far.
        let mut first = fill && held.is_none();
        let (mut confirmed, mut answered) = (true, false);
        let end = loop {
            let start = now.saturating_sub(span).max(floor);
            let walk = Walk {
                floor: start,
                stop_at: if first { None } else { stop_at },
                author_cap,
                ..Walk::default()
            };
            match self.older_window(range, peers, &walk, held, pass).await? {
                (End::Complete, oldest, trusted) => {
                    answered = true;
                    confirmed &= trusted;
                    held = match (held, oldest) {
                        (Some(h), Some(o)) => Some(h.min(o)),
                        (h, o) => h.or(o),
                    };
                    // Held from `start` up, on a trusted peer's word.
                    if confirmed && start < range.cursor_ts {
                        (range.cursor_ts, range.cursor_id) = (start, String::new());
                    }
                    self.store.update_sync_range(range)?;
                }
                (End::Failed, _, _) if answered => break End::Open,
                (end, _, _) => break end,
            }
            if start <= floor {
                break if confirmed { End::Complete } else { End::Open };
            }
            first = false;
            span = span.saturating_mul(2);
        };
        Ok(end)
    }

    /// One ascending window from an older peer, from `walk.floor` up to the
    /// history already `held` (or the end). Returns how it ended, the
    /// oldest `ts` it brought, and whether the peer that ended it is
    /// trusted.
    async fn older_window(
        &self,
        range: &SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        held: Option<u64>,
        pass: &mut Pass,
    ) -> Result<(End, Option<u64>, bool)> {
        let mut oldest = None;
        // A cursor of its own: ascending, from the window's start.
        let mut window = range.clone();
        (window.cursor_ts, window.cursor_id) = (walk.floor.saturating_sub(1), "~".repeat(64));
        if walk.floor == 0 {
            window.cursor_id = String::new();
        }
        for &peer in peers {
            loop {
                self.still_running()?;
                if walk
                    .stop_at
                    .is_some_and(|stop| self.store.store_bytes() >= stop)
                {
                    return Ok((End::Budget, oldest, false));
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
                    return Ok((End::Complete, oldest, self.trusted(&peer)));
                }
            }
        }
        Ok((End::Failed, oldest, false))
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
            (false, true) => self.holes(net, Holes::Serving),
            (false, false) => self.holes(net, Holes::Serving),
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
    /// At most [`GET_ROWS_MAX`] events and [`GET_MAX_BYTES`] in all, each
    /// id once; a thread stops being loaded once the bytes are reached.
    pub(super) fn serve_get(&self, net: &str, ids: &[String], include: &str) -> Response {
        if ids.len() > proto::MAX_GET_IDS {
            return Response::Refused {
                reason: format!("at most {} ids", proto::MAX_GET_IDS),
            };
        }
        let mut unique: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            if !unique.contains(id) {
                unique.push(id.clone());
            }
        }
        let ids = unique;
        let rows = match include {
            "event" => self.store.events_by_ids(net, &ids),
            "thread" => {
                let (mut rows, mut bytes) = (Vec::new(), 0usize);
                for id in &ids {
                    let room = GET_ROWS_MAX.saturating_sub(rows.len());
                    if room == 0 || bytes > GET_MAX_BYTES {
                        break;
                    }
                    match self.store.thread_rows(net, id, GET_THREAD_MAX.min(room)) {
                        Ok(more) => {
                            bytes = more.iter().fold(bytes, |b, (v, _)| {
                                b.saturating_add(serde_json::to_vec(v).map_or(0, |j| j.len()))
                            });
                            rows.extend(more);
                        }
                        Err(e) => {
                            return Response::Refused {
                                reason: format!("{e:#}"),
                            };
                        }
                    }
                }
                Ok(rows)
            }
            "signals" => self.store.signal_events(net, &ids, GET_ROWS_MAX),
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
            // The members of the network up now: trusted ones first.
            let mut others: Vec<EndpointId> = self.neighbors_of(&net);
            others.sort_by_key(|p| {
                (
                    !self.trusted(p),
                    !self.has_feature(p, proto::FEATURE_HISTORY_FULL),
                )
            });
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

    /// Fails once the node stopped: a pass still running then writes no
    /// cursor (a restarted node may already use the store).
    fn still_running(&self) -> Result<()> {
        if self.endpoint().is_closed() {
            bail!("the node stopped");
        }
        Ok(())
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

    fn id(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn a_range_completes_on_a_trusted_word_or_two_at_one_cursor() {
        let mut claims = Claims::default();
        assert!(
            claims.claim(id(1), true, (10, "a")),
            "a trusted peer settles it"
        );
        let mut claims = Claims::default();
        assert!(
            !claims.claim(id(1), false, (10, "a")),
            "one untrusted peer does not"
        );
        assert!(
            !claims.claim(id(1), false, (10, "a")),
            "nor twice the same peer"
        );
        // A second peer that served more: the first one's word is void.
        assert!(!claims.claim(id(2), false, (5, "b")));
        assert!(
            claims.claim(id(3), false, (5, "b")),
            "two distinct peers agree"
        );
    }

    #[test]
    fn walks_combine_with_the_budget_first() {
        assert_eq!(combine(End::Open, End::Budget), End::Budget);
        assert_eq!(combine(End::Open, End::Complete), End::Complete);
        assert_eq!(combine(End::Failed, End::Open), End::Open);
        assert_eq!(combine(End::Failed, End::Failed), End::Failed);
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
