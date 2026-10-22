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
//! A range is confirmed only when a peer this node trusts (one of its
//! configured peers or bootstrap nodes) has nothing more below the cursor,
//! or when two distinct peers that served events of the range say so at
//! the same cursor. Walked to its end without that, it is unconfirmed:
//! held and served as held, shown as such, until a trusted peer confirms
//! it. A failed walk stays pending. Then a bounded union check asks the
//! other neighbors what they hold that the walk did not bring.
//!
//! A node serves its history honestly: a descending page stops at the top
//! of a hole (its cut, a pending or skipped range) with a `gap`. An
//! ascending pull, as 0.4.0 makes, is refused only below the cut or a
//! skipped range, where the budget left history out on purpose.

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
use crate::store::{PHASE_WINDOW, RangeState, SEEN_PEERS, SyncRange};

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
/// Pages of announces each neighbor is asked for, once the fill is done,
/// for when it first saw each key (ADR 0025, "Key age").
const SEEN_PROBE_PAGES: usize = 16;
/// Most events a `get` answers per root (`thread`) and in all, and most
/// bytes in one answer, under the 16 MiB response.
const GET_THREAD_MAX: usize = 500;
const GET_ROWS_MAX: usize = 2000;
const GET_MAX_BYTES: usize = 15 * 1024 * 1024;
/// The union check (ADR 0025 §5): a `sync_desc` peer is read until this
/// many pages in a row bring nothing new; an older peer up to this share
/// of the budget (in percent) in what it sends, or this many bytes with no
/// budget.
const UNION_HELD_PAGES: usize = 2;
const UNION_SHARE: u64 = 5;
const UNION_OLDER_MAX: u64 = 128 << 20;
/// Confirmation probes: the first delay, doubling up to the last.
const PROBE_FIRST: Duration = Duration::from_secs(60);
const PROBE_MAX: Duration = Duration::from_secs(3600);
/// A failed range is tried again after `retry`, doubling up to this.
const RETRY_MAX: Duration = Duration::from_secs(1800);
/// Checks of neighbors that came up while online, per hour.
const JOIN_CHECKS_PER_HOUR: usize = 12;
/// The first window asked from older peers; each next one doubles.
const OLDER_WINDOW_SECS: u64 = 7 * 24 * 3600;

/// How the history task behaves (tests shorten or slow it).
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Events asked per page.
    pub page: u32,
    /// A pause after each page (tests: to stop a walk half-way).
    pub page_pause: Duration,
    /// Pending ranges are tried again this often.
    pub retry: Duration,
    /// Test only: behave as 0.4.0 on `/wats/1` (no `sync_desc`, no `get`,
    /// no history task, an ascending pull from every new neighbor).
    pub legacy: bool,
    /// How often the store is checked against its share for rotation (ADR
    /// 0025 §7).
    pub rotation_check: Duration,
    /// A pause after each rotation transaction, so that inserts get in.
    pub rotation_pause: Duration,
    /// Rotation deletes no row before the node has run this long (ADR 0025
    /// PR B: a profile used before 0.4.3 only through MCP is known again
    /// once a session uses it).
    pub rotation_start_delay: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            page: proto::MAX_SYNC_PAGE,
            page_pause: Duration::ZERO,
            retry: Duration::from_secs(60),
            legacy: false,
            rotation_check: Duration::from_secs(30),
            rotation_pause: Duration::from_millis(5),
            rotation_start_delay: Duration::from_secs(600),
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
    /// Bytes stored per key chain by the fill, across its passes (ADR
    /// 0025 §4: one key's share).
    author_bytes: HashMap<String, u64>,
    /// Confirmation probes per (unconfirmed range id, neighbor): when next,
    /// and the delay that led there.
    probes: HashMap<(i64, EndpointId), (std::time::Instant, Duration)>,
    /// Claimers of each unconfirmed range so far, across passes.
    claims: HashMap<i64, HashSet<EndpointId>>,
    /// Neighbors whose read of an unconfirmed range failed partway: it is
    /// confirmed only once each has read it to its end.
    blockers: HashMap<i64, HashSet<EndpointId>>,
    /// (range id, trusted peer) pairs that answered a probe with a gap.
    probe_skip: HashSet<(i64, EndpointId)>,
    /// Passes in a row where a range failed: the retry backs off.
    failed: u32,
    /// Checks of neighbors that came up while online, in the last hour.
    joins: std::collections::VecDeque<std::time::Instant>,
    /// Neighbors asked for their announces' `seen` since the node started
    /// (the key age, [`Node::probe_seen`]).
    seen_asked: HashSet<EndpointId>,
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
    /// Every one: pending, unconfirmed and skipped ranges, the cut, and no
    /// history yet. What `history_full` counts, and where a descending
    /// page stops: a 0.4.1 peer is never told complete what this node does
    /// not hold for sure.
    All,
    /// The cut and the skipped ranges, where the budget left history out
    /// on purpose: a `partial` request stops there, and an ascending pull
    /// from below one is refused.
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
    /// Walked as far as the peers go, but nothing confirms that nothing
    /// is missing: gaps, `partial` answers, untrusted peers only.
    Unconfirmed,
    /// No peer answered: try again later.
    Failed,
}

/// What one walk asks for.
#[derive(Debug, Default, Clone)]
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
    /// Peers that covered a range from its top down to where they
    /// stopped, or had their union check: no union check for them.
    served: HashSet<EndpointId>,
    /// Bytes stored per key chain by the fill (kept across passes).
    author_bytes: HashMap<String, u64>,
    /// Key -> the first key of its rotation chain.
    chains: HashMap<String, String>,
    /// Neighbors without `sync_desc`.
    older: Vec<EndpointId>,
    /// For the range being walked: peers that covered it from its top
    /// and found nothing more below its cursor (ADR 0025 §9).
    claimers: HashSet<EndpointId>,
    /// Peers that answered the union check with a gap.
    gapped: HashSet<EndpointId>,
    /// Neighbors whose read failed partway: no confirmation until each
    /// reads the range to its end (ADR 0025 §9).
    incomplete: HashSet<EndpointId>,
    /// Neighbors whose union read got to the end of the range.
    completed: HashSet<EndpointId>,
    /// Neighbors whose union read brought events this node did not hold.
    brought: HashSet<EndpointId>,
}

impl Pass {
    fn chain_of<'a>(&'a self, author: &'a str) -> &'a str {
        self.chains.get(author).map_or(author, String::as_str)
    }
}

/// Map every key to the first key of its rotation chain (as held here).
pub(super) fn chains(rotations: &[(String, String)]) -> HashMap<String, String> {
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
        (End::Unconfirmed, _) | (_, End::Unconfirmed) => End::Unconfirmed,
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
    if let Some(cut) = history["cut_ts"].as_u64() {
        text.push_str(&format!(" · cut {}", date(cut)));
        if let Some(at) = history["rotated_ts"].as_u64() {
            text.push_str(&format!(" (rotation, {})", date(at)));
        }
    }
    for range in history["ranges"].as_array().into_iter().flatten() {
        let (Some(lo), Some(hi)) = (range["lo_ts"].as_u64(), range["hi_ts"].as_u64()) else {
            continue;
        };
        let why = match range["state"].as_str() {
            Some("pending") => "resuming",
            Some("skipped") => "skipped: budget",
            Some("unconfirmed") if lo == 0 => {
                text.push_str(" · unconfirmed: no trusted peer yet");
                continue;
            }
            Some("unconfirmed") => {
                text.push_str(&format!(" · unconfirmed since {}", date(lo)));
                continue;
            }
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
    let ranges = history["ranges"].as_array().cloned().unwrap_or_default();
    if ranges.iter().any(|r| r["state"] != "unconfirmed") {
        text.push_str(" +gaps");
    }
    if ranges.iter().any(|r| r["state"] == "unconfirmed") {
        text.push_str(" +unconfirmed");
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
    let holes = ranges
        .iter()
        .any(|r| r.hole_top().is_some() || r.state == RangeState::Unconfirmed);
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
    // When rotation last raised it (ADR 0025 §7).
    let rotated = ranges
        .iter()
        .filter(|r| r.state == RangeState::Cut && r.phase == crate::store::PHASE_ROTATION)
        .map(|r| r.opened)
        .max();
    let shown: Vec<Value> = ranges
        .iter()
        .filter_map(|r| match r.state {
            RangeState::Pending | RangeState::Skipped if r.phase.is_empty() => Some(json!({
                "state": r.state.as_str(),
                "lo_ts": r.hole_lo(),
                "hi_ts": r.hole_top()?,
            })),
            RangeState::Unconfirmed => Some(json!({
                "state": r.state.as_str(),
                "lo_ts": r.lo_ts,
                "hi_ts": r.hi_ts,
            })),
            _ => None,
        })
        .collect();
    json!({
        "state": state,
        "oldest_ts": store.oldest_ts(net).ok().flatten(),
        "cut_ts": cut,
        "rotated_ts": rotated,
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
        self.holes(net, Holes::All, false)
    }

    /// The holes of `net` that count for `which` request. `kept_only`: a
    /// request for kept-forever kinds only, which rotation never deletes,
    /// so its cut is no hole for them (the fill's cut and skipped ranges
    /// still are).
    fn holes(&self, net: &str, which: Holes, kept_only: bool) -> Vec<(u64, u64)> {
        let ranges = self.store.sync_ranges(net).unwrap_or_default();
        let mut holes: Vec<(u64, u64)> = ranges
            .iter()
            .filter(|r| match which {
                Holes::All => true,
                Holes::Intentional => r.state != RangeState::Pending,
            })
            .filter(|r| {
                !(kept_only
                    && r.state == RangeState::Cut
                    && r.phase == crate::store::PHASE_ROTATION)
            })
            .filter_map(|r| match r.state {
                // Held, but not confirmed: no confirmation for a 0.4.1
                // peer either, which takes it with `partial`.
                RangeState::Unconfirmed if which == Holes::All => Some((r.lo_ts, r.hi_ts)),
                _ => Some((r.hole_lo(), r.hole_top()?)),
            })
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
    /// joined (`history_full`): no budget, a fill done, no hole, nothing
    /// unconfirmed, and no row ever deleted by rotation (ADR 0025 §7).
    pub fn history_full(&self) -> bool {
        self.history_budget().is_none()
            && !self.config.history.legacy
            && !self.store.history_lossy().unwrap_or(true)
            && self.joined_nets().iter().all(|net| self.holds_all(net))
    }

    /// Whether this node holds the confirmed, complete history of `net`.
    fn holds_all(&self, net: &str) -> bool {
        self.store.sync_watermark(net).unwrap_or(0) > 0
            && self.history_holes(net).is_empty()
            && !self
                .store
                .sync_ranges(net)
                .unwrap_or_default()
                .iter()
                .any(|r| r.state == RangeState::Unconfirmed)
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
        } else if self.history_budget().is_none() && self.holds_all(net) {
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
    pub(super) fn neighbors_of(&self, net: &str) -> Vec<EndpointId> {
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

    pub(super) fn has_feature(&self, peer: &EndpointId, feature: &str) -> bool {
        self.peer_info(peer)
            .is_some_and(|i| i.features.iter().any(|f| f == feature))
    }

    /// Whether this node takes `peer`'s word that it holds nothing more
    /// (ADR 0025 §9): one of its configured peers or bootstrap nodes.
    /// Anyone else's `history_full` is a hint for the order, no more.
    pub(super) fn trusted(&self, peer: &EndpointId) -> bool {
        self.seed_ids.contains(peer)
    }

    /// The neighbors of `net` to walk history from, in the order to ask
    /// them: trusted `history_full` peers, trusted peers, then the others
    /// in a random order (an endpoint id can be ground; their
    /// `history_full` claims are ignored). Split into those with
    /// `sync_desc` and the older ones.
    fn history_peers(&self, net: &str) -> (Vec<EndpointId>, Vec<EndpointId>) {
        let me = self.endpoint_id();
        let shuffle = std::collections::hash_map::RandomState::new();
        let mut peers = self.neighbors_of(net);
        peers.retain(|p| *p != me);
        peers.sort_by_cached_key(|p| {
            let trusted = self.trusted(p);
            (
                !trusted,
                !(trusted && self.has_feature(p, proto::FEATURE_HISTORY_FULL)),
                shuffle.hash_one(p),
            )
        });
        peers
            .into_iter()
            .partition(|p| self.has_feature(p, proto::FEATURE_SYNC_DESC))
    }

    /// One pass of the history task of `net`: open a range if needed, walk
    /// the pending ones newest first, try to confirm the unconfirmed ones,
    /// then check the neighbors that came up while online.
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
        let mut outcome = self
            .walk_pending(net, &peers, &mut pass)
            .await
            .map(|failed| {
                self.set_progress(net, |p| {
                    p.failed = if failed {
                        p.failed.saturating_add(1)
                    } else {
                        0
                    };
                });
            });
        if outcome.is_ok() {
            let mut all = peers.clone();
            all.extend(pass.older.iter().copied());
            outcome = self.confirm_unconfirmed(net, &all).await;
        }
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
        if !fill_running {
            let (desc, _) = self.history_peers(net);
            self.probe_seen(net, &desc).await;
        }
        // Neighbors that came up while this node was online (no range
        // opened for them): what they hold since the watermark the pass
        // started with, bounded like the union check (ADR 0025 §5). Not
        // after the fill (watermark 0): its union check ran.
        if self.history_pending(net) || watermark == 0 {
            // Not drained: they wait for the range to end.
            return Ok(());
        }
        // At most so many an hour, as the check has no budget of its own;
        // over the cap, the neighbors stay queued for later.
        let check: Vec<EndpointId> = self
            .set_progress(net, |p| {
                let now = std::time::Instant::now();
                while p
                    .joins
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(3600))
                {
                    p.joins.pop_front();
                }
                if p.check.is_empty() || p.joins.len() >= JOIN_CHECKS_PER_HOUR {
                    return Vec::new();
                }
                p.joins.push_back(now);
                p.check.drain().collect()
            })
            .unwrap_or_default();
        let check: Vec<EndpointId> = check
            .into_iter()
            .filter(|p| !pass.served.contains(p) && !pass.behind.contains(p))
            .collect();
        if check.is_empty() {
            return Ok(());
        }
        let started = now_secs();
        let stop_at = self.history_budget().map(|b| {
            self.store
                .store_bytes()
                .saturating_add(percent(b, UNION_SHARE))
        });
        let walk = Walk {
            floor: watermark.saturating_sub(MAX_CLOCK_SKEW_SECS),
            stop_at,
            ..Walk::default()
        };
        let top = started.saturating_add(MAX_CLOCK_SKEW_SECS);
        pass.incomplete.clear();
        pass.brought.clear();
        self.union_check(net, top, &check, &walk, &mut pass, false)
            .await?;
        // The check reads from the watermark the pass started with, so it
        // can cover a range the pass just confirmed. A neighbor that
        // brought what that range lacked, then stopped partway (an older
        // peer's rate limit): what it holds further is unknown, so the
        // span is unconfirmed again until it reads it to its end (ADR
        // 0025, review 5).
        let blockers: HashSet<EndpointId> = pass
            .incomplete
            .intersection(&pass.brought)
            .copied()
            .collect();
        if !blockers.is_empty() {
            let lo = walk.floor.max(self.store.cut_ts(net)?.unwrap_or(0));
            info!(
                %net,
                "history: {} – {} unconfirmed: a neighbor that came up holds more",
                date(lo),
                date(top)
            );
            let id = self.store.add_sync_range(&SyncRange {
                id: 0,
                net: net.to_string(),
                lo_ts: lo,
                hi_ts: top,
                cursor_ts: lo,
                cursor_id: String::new(),
                state: RangeState::Unconfirmed,
                phase: String::new(),
                opened: started,
                base_bytes: self.store.store_bytes(),
            })?;
            self.set_progress(net, |p| p.blockers.entry(id).or_default().extend(blockers));
        }
        if !self.history_pending(net) {
            self.store.set_sync_watermark(net, started)?;
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
        // Never below the cut, which rotation may have raised above the
        // watermark (ADR 0025 §7): what is below it is not held on purpose.
        let lo = lo.max(self.store.cut_ts(net)?.unwrap_or(0));
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
    /// `sync_desc`), then from the older neighbors of the pass; then the
    /// union check of each range with every neighbor that did not cover it
    /// from its top. Returns whether a range failed (no peer answered).
    async fn walk_pending(&self, net: &str, peers: &[EndpointId], pass: &mut Pass) -> Result<bool> {
        let ranges: Vec<SyncRange> = self
            .store
            .sync_ranges(net)?
            .into_iter()
            .filter(|r| r.state == RangeState::Pending)
            .collect();
        for mut range in ranges {
            let fill = !range.phase.is_empty();
            pass.claimers.clear();
            pass.incomplete.clear();
            let mut end = if fill {
                self.walk_fill(&mut range, peers, pass).await?
            } else {
                self.walk_catch_up(&mut range, peers, pass).await?
            };
            if end == End::Failed {
                self.finish(&mut range, end)?;
                debug!(%net, "history: no peer could serve the range; kept pending");
                return Ok(true);
            }
            // The union check (ADR 0025 §5): what the other neighbors hold
            // that the walk did not bring, from the range's top down to its
            // floor, within the range's budget.
            let others: Vec<EndpointId> = self
                .neighbors_of(net)
                .into_iter()
                .filter(|p| *p != self.endpoint_id() && !pass.served.contains(p))
                .collect();
            if end != End::Budget && !others.is_empty() {
                let walk = Walk {
                    floor: range.lo_ts,
                    stop_at: self
                        .history_budget()
                        .map(|b| range.base_bytes.saturating_add(b)),
                    author_cap: if fill {
                        self.history_budget().map(|b| percent(b, AUTHOR_SHARE))
                    } else {
                        None
                    },
                    ..Walk::default()
                };
                // Exhaustive unless a trusted peer walked the range.
                let exhaustive = end != End::Complete;
                self.union_check(net, range.hi_ts, &others, &walk, pass, exhaustive)
                    .await?;
            }
            if end == End::Unconfirmed
                && pass.incomplete.is_empty()
                && self.confirmed_by(&pass.claimers)
            {
                end = End::Complete;
            }
            let span = (range.lo_ts, range.hi_ts);
            self.finish(&mut range, end)?;
            // Claims carried over to later passes, for an unconfirmed range
            // as walked: merged with another span, it starts over.
            if range.state == RangeState::Unconfirmed {
                let claims = std::mem::take(&mut pass.claimers);
                let blockers = std::mem::take(&mut pass.incomplete);
                let kept = (range.lo_ts, range.hi_ts) == span;
                let id = range.id;
                self.set_progress(net, |p| {
                    // A merged span keeps its blockers: its claims start
                    // over, not what is known missing.
                    p.blockers.entry(id).or_default().extend(blockers);
                    if kept {
                        p.claims.insert(id, claims);
                    } else {
                        p.claims.remove(&id);
                    }
                });
            }
        }
        Ok(false)
    }

    /// Whether these peers, each of which covered a range from its top
    /// and found nothing more below its cursor, confirm it (ADR 0025 §9):
    /// a trusted one, or two distinct ones.
    fn confirmed_by(&self, claimers: &HashSet<EndpointId>) -> bool {
        claimers.len() >= 2 || claimers.iter().any(|p| self.trusted(p))
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
        Ok(end)
    }

    /// The union check (ADR 0025 §5): what `peers` hold between `walk.floor`
    /// and `top` that this node does not, so that no single peer's
    /// omission sticks.
    ///
    /// - **Bounded** (`exhaustive` false), when a trusted peer walked the
    ///   range: a `sync_desc` peer is read newest first until
    ///   [`UNION_HELD_PAGES`] pages in a row bring nothing new. It trades
    ///   exhaustiveness for bandwidth, and claims nothing.
    /// - **Exhaustive**, when no trusted peer did: a `sync_desc` peer is
    ///   read down to the floor, as 0.4.0 read each neighbor. One that gets
    ///   there with no gap and nothing new, having served events of the
    ///   range (or trusted), covered the range: it becomes a claimer. Events
    ///   it brings void the earlier claims.
    ///
    /// An older peer is read ascending from the floor, up to [`UNION_SHARE`]
    /// % of the budget in what it sends; read to its end with nothing new,
    /// it is a claimer too. Every read stops at `walk.stop_at`.
    async fn union_check(
        &self,
        net: &str,
        top: u64,
        peers: &[EndpointId],
        walk: &Walk,
        pass: &mut Pass,
        exhaustive: bool,
    ) -> Result<()> {
        // Never below the cut: what is there, the next rotation would
        // delete again (ADR 0025 PR B).
        let cut = self.store.cut_ts(net)?.unwrap_or(0);
        let clamped;
        let walk = if cut > walk.floor {
            clamped = Walk {
                floor: cut,
                ..walk.clone()
            };
            &clamped
        } else {
            walk
        };
        let cap = self
            .history_budget()
            .or(self.events_share())
            .map_or(UNION_OLDER_MAX, |b| percent(b, UNION_SHARE))
            .max(1 << 20);
        for &peer in peers {
            if !pass.served.insert(peer) {
                continue;
            }
            let desc = self.has_feature(&peer, proto::FEATURE_SYNC_DESC);
            let mut scratch = SyncRange {
                id: 0,
                net: net.to_string(),
                lo_ts: walk.floor,
                hi_ts: top,
                cursor_ts: top,
                cursor_id: String::new(),
                state: RangeState::Pending,
                phase: String::new(),
                opened: 0,
                base_bytes: 0,
            };
            if !desc {
                (scratch.cursor_ts, scratch.cursor_id) = if walk.floor == 0 {
                    (0, String::new())
                } else {
                    (walk.floor.saturating_sub(1), "~".repeat(64))
                };
            }
            let (mut held_pages, mut received, mut new_total) = (0usize, 0u64, 0usize);
            let mut served = 0usize;
            // An exhaustive read asks for what the peer holds for sure; after
            // a gap, what it holds at all (`partial`), and claims nothing.
            let mut partial = !exhaustive;
            // Whether it read to the end, with no gap and under the cap.
            let mut to_end = false;
            loop {
                self.still_running()?;
                if walk
                    .stop_at
                    .is_some_and(|stop| self.store.store_bytes() >= stop)
                {
                    return Ok(());
                }
                let request = if desc {
                    Request::Sync {
                        net: net.to_string(),
                        after_ts: walk.floor,
                        after_id: String::new(),
                        before_ts: Some(scratch.cursor_ts),
                        before_id: Some(scratch.cursor_id.clone()),
                        kinds: Vec::new(),
                        authors: Vec::new(),
                        partial,
                        limit: self.config.history.page,
                    }
                } else {
                    Request::sync_after(
                        net,
                        scratch.cursor_ts,
                        &scratch.cursor_id,
                        self.config.history.page,
                    )
                    .partial()
                };
                let Some((events, more, gap, seen)) = self.page(peer, &request).await else {
                    // Partway: what it holds further is unknown.
                    if served > 0 || received > 0 {
                        pass.incomplete.insert(peer);
                    }
                    break;
                };
                let new = self.unheld(&events);
                new_total = new_total.saturating_add(new);
                if new > 0 {
                    // The walk missed these: its "nothing more" was wrong.
                    pass.claimers.clear();
                    pass.brought.insert(peer);
                }
                served = served.saturating_add(events.len());
                received = events.iter().fold(received, |n, v| {
                    n.saturating_add(serde_json::to_vec(v).map_or(0, |b| b.len() as u64))
                });
                let floor_walk = Walk {
                    floor: if desc { walk.floor } else { 0 },
                    author_cap: walk.author_cap,
                    ..Walk::default()
                };
                if let Err(e) = self
                    .take_page(&mut scratch, peer, &events, &seen, &floor_walk, pass, desc)
                    .await
                {
                    debug!(peer = %peer.fmt_short(), "union check page refused: {e:#}");
                    pass.incomplete.insert(peer);
                    break;
                }
                held_pages = if new == 0 {
                    held_pages.saturating_add(1)
                } else {
                    0
                };
                if gap.is_some() {
                    pass.gapped.insert(peer);
                    if partial {
                        break;
                    }
                    partial = true;
                    continue;
                }
                if events.is_empty() || !more {
                    to_end = !partial || !desc;
                    pass.completed.insert(peer);
                    break;
                }
                if !exhaustive && desc && held_pages >= UNION_HELD_PAGES {
                    break;
                }
                if !desc && received >= cap {
                    break;
                }
            }
            // A claimer read the whole range, found nothing new, and served
            // events of it (or is trusted, whose word on nothing is enough).
            if (exhaustive || !desc)
                && to_end
                && new_total == 0
                && (served > 0
                    || self.trusted(&peer)
                    || self.holds_below(peer, net, walk.floor).await)
            {
                pass.claimers.insert(peer);
            }
        }
        Ok(())
    }

    /// One page from `peer`, or `None` when it did not answer with one.
    async fn page(
        &self,
        peer: EndpointId,
        request: &Request,
    ) -> Option<(Vec<Value>, bool, Option<Gap>, Vec<u64>)> {
        let answer = tokio::time::timeout(
            PAGE_TIMEOUT,
            proto::call_solving(self.endpoint(), peer, request),
        )
        .await;
        match answer {
            Ok(Ok(Response::SyncPage {
                events,
                more,
                gap,
                seen,
            })) => Some((events, more, gap, seen)),
            other => {
                debug!(peer = %peer.fmt_short(), "history page: {other:?}");
                None
            }
        }
    }

    /// Whether `peer` holds an event below `floor`: a peer with history
    /// can say a quiet span is empty; one with none (a fresh id) cannot
    /// (ADR 0025 §9).
    async fn holds_below(&self, peer: EndpointId, net: &str, floor: u64) -> bool {
        if floor == 0 {
            return false;
        }
        let request = Request::Sync {
            net: net.to_string(),
            after_ts: 0,
            after_id: String::new(),
            before_ts: Some(floor),
            before_id: Some(String::new()),
            kinds: Vec::new(),
            authors: Vec::new(),
            partial: false,
            limit: 1,
        };
        self.page(peer, &request)
            .await
            .is_some_and(|(events, ..)| !events.is_empty())
    }

    /// How many of `events` this node does not hold.
    /// The author of the event `id`, when this node holds it (a tombstone
    /// included).
    fn held_author(&self, id: &str) -> Option<String> {
        let (_, _, stored) = self.store.get_raw(id).ok().flatten()?;
        stored.get("author")?.as_str().map(str::to_string)
    }

    fn unheld(&self, events: &[Value]) -> usize {
        events
            .iter()
            .filter_map(head)
            .filter(|h| self.store.get_raw(h.id).ok().flatten().is_none())
            .count()
    }

    /// Unconfirmed ranges (ADR 0025 §9): each neighbor that has not claimed
    /// one yet gets its exhaustive union check over the whole span, trusted
    /// neighbors first, so that a neighbor that comes up later can confirm
    /// it. What comes is stored; the claims add up across passes (in
    /// memory), and a trusted one or two distinct ones confirm the range.
    /// Each (range, neighbor) is tried again with a backoff (1 min,
    /// doubling up to an hour); a neighbor that answers with a gap there
    /// is not asked about that range again.
    async fn confirm_unconfirmed(&self, net: &str, peers: &[EndpointId]) -> Result<()> {
        let mut peers: Vec<EndpointId> = peers.to_vec();
        peers.sort_by_key(|p| !self.trusted(p));
        let ranges: Vec<SyncRange> = self
            .store
            .sync_ranges(net)?
            .into_iter()
            .filter(|r| r.state == RangeState::Unconfirmed)
            .collect();
        let now = std::time::Instant::now();
        for mut range in ranges {
            // Neighbors whose read failed partway: no confirmation until
            // each reads the range to its end.
            let mut blockers = self
                .set_progress(net, |p| {
                    p.blockers.get(&range.id).cloned().unwrap_or_default()
                })
                .unwrap_or_default();
            let mut claims = self
                .set_progress(net, |p| {
                    p.claims.get(&range.id).cloned().unwrap_or_default()
                })
                .unwrap_or_default();
            for &peer in &peers {
                let key = (range.id, peer);
                let due = self
                    .set_progress(net, |p| {
                        !claims.contains(&peer)
                            && !p.probe_skip.contains(&key)
                            && p.probes.get(&key).is_none_or(|(at, _)| *at <= now)
                    })
                    .unwrap_or(false);
                if !due {
                    continue;
                }
                let mut pass = Pass {
                    claimers: claims.clone(),
                    ..Pass::default()
                };
                let walk = Walk {
                    floor: range.lo_ts,
                    stop_at: self
                        .history_budget()
                        .map(|b| self.store.store_bytes().saturating_add(b)),
                    ..Walk::default()
                };
                self.union_check(net, range.hi_ts, &[peer], &walk, &mut pass, true)
                    .await?;
                claims = pass.claimers;
                if pass.incomplete.contains(&peer) {
                    blockers.insert(peer);
                } else if pass.completed.contains(&peer) {
                    blockers.remove(&peer);
                }
                let gapped = pass.gapped.contains(&peer);
                let claimed = claims.contains(&peer);
                self.set_progress(net, |p| {
                    if gapped {
                        p.probe_skip.insert(key);
                    }
                    if claimed {
                        p.probes.remove(&key);
                    } else {
                        let delay = p
                            .probes
                            .get(&key)
                            .map_or(PROBE_FIRST, |(_, d)| d.saturating_mul(2).min(PROBE_MAX));
                        let at = now.checked_add(delay).unwrap_or(now);
                        p.probes.insert(key, (at, delay));
                    }
                });
                if blockers.is_empty() && self.confirmed_by(&claims) {
                    info!(
                        %net,
                        "history: {} – {} confirmed",
                        date(range.lo_ts),
                        date(range.hi_ts)
                    );
                    range.state = RangeState::Done;
                    self.store.update_sync_range(&range)?;
                    break;
                }
            }
            self.set_progress(net, |p| {
                if range.state == RangeState::Done {
                    p.claims.remove(&range.id);
                    p.blockers.remove(&range.id);
                    p.probes.retain(|(id, _), _| *id != range.id);
                } else {
                    p.claims.insert(range.id, claims);
                    p.blockers.insert(range.id, blockers);
                }
            });
        }
        Ok(())
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
                    return Ok(end);
                }
            };
            match end {
                End::Failed => return Ok(End::Failed),
                End::Budget => info!(
                    %net,
                    phase = %range.phase,
                    "history: the phase reached its share of the budget; moving on"
                ),
                End::Unconfirmed => {
                    debug!(%net, phase = %range.phase, "history: phase not confirmed; moving on");
                }
                End::Complete => {}
            }
            range.phase = next.to_string();
            (range.cursor_ts, range.cursor_id) = (range.hi_ts, String::new());
            self.store.update_sync_range(range)?;
        }
    }

    /// The end of a walk over `range`:
    /// - complete: held;
    /// - unconfirmed: walked to its end, but no trusted peer confirmed it:
    ///   held and served as held, shown as unconfirmed;
    /// - the budget: a cut for the fill, at the cursor reached, or a
    ///   skipped range for a catch-up;
    /// - failed: still pending. Failures never close a range.
    fn finish(&self, range: &mut SyncRange, end: End) -> Result<End> {
        let net = range.net.clone();
        let fill = !range.phase.is_empty();
        match end {
            End::Failed => {
                self.store.update_sync_range(range)?;
                return Ok(end);
            }
            End::Complete => {
                range.state = RangeState::Done;
                if fill {
                    info!(%net, "history: initial fill done, confirmed");
                }
            }
            End::Unconfirmed => {
                info!(
                    %net,
                    "history: {} – {} fetched; no trusted peer confirmed it yet",
                    date(range.lo_ts),
                    date(range.hi_ts)
                );
                range.state = RangeState::Unconfirmed;
                // Overlapping or adjacent unconfirmed spans make one;
                // disjoint ones stay apart.
                for other in self.store.sync_ranges(&net)? {
                    let touches = other.lo_ts <= range.hi_ts.saturating_add(MAX_CLOCK_SKEW_SECS)
                        && range.lo_ts <= other.hi_ts.saturating_add(MAX_CLOCK_SKEW_SECS);
                    if other.id != range.id && other.state == RangeState::Unconfirmed && touches {
                        range.lo_ts = range.lo_ts.min(other.lo_ts);
                        range.hi_ts = range.hi_ts.max(other.hi_ts);
                        if (other.cursor_ts, other.cursor_id.as_str())
                            < (range.cursor_ts, range.cursor_id.as_str())
                        {
                            (range.cursor_ts, range.cursor_id) =
                                (other.cursor_ts, other.cursor_id.clone());
                        }
                        self.store.delete_sync_range(other.id)?;
                    }
                }
            }
            End::Budget if fill => {
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
                    cursor_id: String::new(),
                    state: RangeState::Cut,
                    phase: String::new(),
                    opened: now_secs(),
                    base_bytes: 0,
                })?;
                range.state = RangeState::Done;
            }
            End::Budget => {
                warn!(
                    %net,
                    "history: catch-up larger than sync.initial_max; {} – {} skipped",
                    date(range.lo_ts),
                    date(range.cursor_ts)
                );
                range.state = RangeState::Skipped;
                range.hi_ts = range.cursor_ts;
            }
        }
        if fill {
            range.phase = PHASE_WINDOW.into();
            self.set_progress(&net, |p| p.author_bytes.clear());
        }
        self.store.update_sync_range(range)?;
        Ok(end)
    }

    /// Walk down `range` from its cursor, page by page and peer by peer,
    /// persisting the cursor after each page: first from peers that hold
    /// the range complete, then, when that confirms nothing, what they hold
    /// in ranges of their own still pending (`partial`). A `partial`
    /// answer never confirms the range.
    async fn walk(
        &self,
        range: &mut SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        pass: &mut Pass,
    ) -> Result<End> {
        match self.walk_round(range, peers, walk, pass, false).await? {
            End::Unconfirmed => match self.walk_round(range, peers, walk, pass, true).await? {
                End::Budget => Ok(End::Budget),
                _ => Ok(End::Unconfirmed),
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
        let unfiltered = walk.kinds.is_empty() && walk.authors.is_empty();
        for &peer in peers {
            if !partial && pass.behind.contains(&peer) {
                answered = true;
                continue;
            }
            // A peer asked from the range's top covers it down to where it
            // stops: it needs no union check, and its "nothing more" counts
            // (ADR 0025 §9). One asked from lower down covers nothing.
            let covering =
                unfiltered && (range.cursor_ts, range.cursor_id.as_str()) == (range.hi_ts, "");
            // Whether it served events: a peer that served nothing confirms
            // nothing, unless trusted (ADR 0025 §9).
            let mut content = false;
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
                if covering {
                    pass.served.insert(peer);
                }
                if !events.is_empty() {
                    // Below every earlier "nothing more": those were wrong.
                    pass.claimers.clear();
                    content = true;
                }
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
                    let trusted = self.trusted(&peer);
                    if !partial
                        && covering
                        && (content
                            || trusted
                            || self.holds_below(peer, &range.net, walk.floor).await)
                    {
                        pass.claimers.insert(peer);
                        if trusted {
                            return Ok(End::Complete);
                        }
                    }
                    // Not confirmed: the next peer, from the same cursor;
                    // the union check asks the others from the top.
                    break;
                }
            }
        }
        Ok(if answered {
            End::Unconfirmed
        } else {
            End::Failed
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
                Ok(Some((event, new))) => {
                    if new && walk.author_cap.is_some() {
                        let used = pass.author_bytes.entry(chain).or_default();
                        *used = used.saturating_add(size);
                    }
                    if let Some(at) = seen.and_then(|s| s.get(i)) {
                        reports.push((event.author().to_string(), *at));
                    }
                }
                Ok(None) => {}
                // Stopped half-way: the cursor must not pass what was not
                // stored.
                Err(e) if self.endpoint().is_closed() => return Err(e),
                Err(e) => debug!(%id, "history: rejected event: {e:#}"),
            }
        }
        self.record_seen(peer, &reports);
        Ok(())
    }

    /// Record what `peer` says about when it first stored these events'
    /// keys (`seen`), with whether it advertises full history: the key age
    /// of reputation (ADR 0025, "Key age"). Reports come for events this
    /// node holds, stored now or already (their signature checked either
    /// way), so that each peer serving a key counts.
    pub(super) fn record_seen(&self, peer: EndpointId, reports: &[(String, u64)]) {
        let full = self.has_feature(&peer, proto::FEATURE_HISTORY_FULL);
        if let Err(e) = self.store.record_seen(&peer.to_string(), full, reports) {
            debug!("seen reports not recorded: {e:#}");
        }
    }

    /// Ask `peers` (with `sync_desc`) not asked yet since the node started,
    /// [`SEEN_PEERS`] at most per network, for the announces they hold,
    /// newest first, [`SEEN_PROBE_PAGES`] pages at most each: their `seen`
    /// is when each peer first stored a key's announce, usually the key's
    /// first event. The fill reads most history from one peer, and a key
    /// needs [`crate::reputation::SEEN_QUORUM`] peers' reports for its age
    /// (ADR 0025, "Key age"). It stores nothing: announces not held here
    /// are skipped (the fill's phase 0 fetches them, under the per-key
    /// share and the budget).
    async fn probe_seen(&self, net: &str, peers: &[EndpointId]) {
        for &peer in peers {
            let ask = self
                .set_progress(net, |p| {
                    p.seen_asked.len() < SEEN_PEERS as usize && p.seen_asked.insert(peer)
                })
                .unwrap_or(false);
            if !ask {
                continue;
            }
            let mut before = (
                now_secs().saturating_add(MAX_CLOCK_SKEW_SECS),
                String::new(),
            );
            for _ in 0..SEEN_PROBE_PAGES {
                if self.still_running().is_err() {
                    return;
                }
                let request = Request::Sync {
                    net: net.to_string(),
                    after_ts: 0,
                    after_id: String::new(),
                    before_ts: Some(before.0),
                    before_id: Some(before.1.clone()),
                    kinds: vec!["announce".to_string()],
                    authors: Vec::new(),
                    partial: true,
                    limit: self.config.history.page,
                };
                let Some((events, more, _, seen)) = self.page(peer, &request).await else {
                    break;
                };
                let seen = (seen.len() == events.len()).then_some(seen);
                let mut reports: Vec<(String, u64)> = Vec::new();
                let mut ordered = true;
                for (i, value) in events.iter().enumerate() {
                    let Some(h) = head(value) else {
                        ordered = false;
                        break;
                    };
                    if (h.ts, h.id) >= (before.0, before.1.as_str())
                        || h.net != net
                        || h.kind != "announce"
                    {
                        ordered = false;
                        break;
                    }
                    before = (h.ts, h.id.to_string());
                    // Stores nothing: only events held here count, under
                    // the author this node verified when it stored them.
                    if let Some(at) = seen.as_ref().and_then(|s| s.get(i))
                        && let Some(author) = self.held_author(h.id)
                        && author == h.author
                    {
                        reports.push((author, *at));
                    }
                }
                self.record_seen(peer, &reports);
                if !ordered {
                    debug!(peer = %peer.fmt_short(), "seen probe: a page out of order");
                    break;
                }
                if events.is_empty() || !more {
                    break;
                }
            }
        }
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
                    .get_from(*peer, net, batch, include, PAGE_TIMEOUT, Origin::Pulled)
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
    /// `origin`: [`Origin::Fetched`] for a `read`, the only way an evicted
    /// body comes back; [`Origin::Pulled`] for the fill.
    async fn get_from(
        &self,
        peer: EndpointId,
        net: &str,
        ids: &[String],
        include: &str,
        timeout: Duration,
        origin: Origin,
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
            match self.accept_value(value, origin).await {
                Ok(Some((event, stored))) => {
                    if stored {
                        new = new.saturating_add(1);
                    }
                    if let Some(at) = seen.as_ref().and_then(|s| s.get(i)) {
                        reports.push((event.author().to_string(), *at));
                    }
                }
                Ok(None) => {}
                Err(e) => debug!("get: rejected event: {e:#}"),
            }
        }
        self.record_seen(peer, &reports);
        Ok(new)
    }

    /// Older peers (no `sync_desc`): ascending windows from now - 7 days,
    /// then 14, 28…, each read until it meets the history already held,
    /// until the budget is spent or the range's `lo` is reached. A fill
    /// with no history `held` yet reads the first window to its end
    /// whatever the budget. Confirmed only if every window was read to its
    /// end from a trusted peer; otherwise unconfirmed.
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
        // The one peer that read every window, from the top (no history
        // `held` above them yet): it covered the range.
        let (mut coverer, mut mixed) = (None, held.is_some());
        let end = loop {
            let start = now.saturating_sub(span).max(floor);
            let walk = Walk {
                floor: start,
                stop_at: if first { None } else { stop_at },
                author_cap,
                ..Walk::default()
            };
            match self.older_window(range, peers, &walk, held, pass).await? {
                (End::Complete, oldest, by) => {
                    answered = true;
                    confirmed &= by.is_some_and(|p| self.trusted(&p));
                    match (coverer, by) {
                        (None, Some(p)) => coverer = Some(p),
                        (Some(c), Some(p)) if c == p => {}
                        _ => mixed = true,
                    }
                    held = match (held, oldest) {
                        (Some(h), Some(o)) => Some(h.min(o)),
                        (h, o) => h.or(o),
                    };
                    // Walked from `start` up.
                    if start < range.cursor_ts {
                        (range.cursor_ts, range.cursor_id) = (start, String::new());
                    }
                    self.store.update_sync_range(range)?;
                }
                (End::Failed, _, _) if answered => {
                    // Windows still unread: what they hold is unknown.
                    pass.incomplete.extend(peers.iter().copied());
                    break End::Unconfirmed;
                }
                (end, _, _) => break end,
            }
            if start <= floor {
                if let (Some(peer), false) = (coverer, mixed) {
                    pass.served.insert(peer);
                    if held.is_some() || self.trusted(&peer) {
                        pass.claimers.insert(peer);
                    }
                }
                break if confirmed {
                    End::Complete
                } else {
                    End::Unconfirmed
                };
            }
            first = false;
            span = span.saturating_mul(2);
        };
        Ok(end)
    }

    /// One ascending window from an older peer, from `walk.floor` up to the
    /// history already `held` (or the end). Returns how it ended, the
    /// oldest `ts` it brought, and the peer that read it to its end.
    async fn older_window(
        &self,
        range: &SyncRange,
        peers: &[EndpointId],
        walk: &Walk,
        held: Option<u64>,
        pass: &mut Pass,
    ) -> Result<(End, Option<u64>, Option<EndpointId>)> {
        let mut oldest = None;
        // A cursor of its own: ascending, from the window's start.
        let mut window = range.clone();
        (window.cursor_ts, window.cursor_id) = (walk.floor.saturating_sub(1), "~".repeat(64));
        if walk.floor == 0 {
            window.cursor_id = String::new();
        }
        for &peer in peers {
            let mut pages = 0usize;
            loop {
                self.still_running()?;
                if walk
                    .stop_at
                    .is_some_and(|stop| self.store.store_bytes() >= stop)
                {
                    return Ok((End::Budget, oldest, None));
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
                        // Partway: what it holds further is unknown.
                        if pages > 0 {
                            pass.incomplete.insert(peer);
                        }
                        break;
                    }
                };
                pages = pages.saturating_add(1);
                // Stop at the history the previous window brought.
                let reached = held.and_then(|h| {
                    events
                        .iter()
                        .position(|v| head(v).is_some_and(|x| x.ts >= h))
                });
                if let Some(at) = reached {
                    events.truncate(at);
                }
                if self.unheld(&events) > 0 {
                    // Missed by every earlier "nothing more": void.
                    pass.claimers.clear();
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
                    pass.incomplete.insert(peer);
                    break;
                }
                if oldest.is_none() {
                    oldest = events.first().and_then(|v| head(v).map(|h| h.ts));
                }
                self.set_progress(&range.net, |p| p.source = Some(peer));
                if !self.config.history.page_pause.is_zero() {
                    tokio::time::sleep(self.config.history.page_pause).await;
                }
                if reached.is_some() || !more || events.is_empty() {
                    return Ok((End::Complete, oldest, Some(peer)));
                }
            }
        }
        Ok((End::Failed, oldest, None))
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
        // Kept-forever kinds only (phase 0 of a fill), and reacts: rotation
        // never deletes those kinds, nor erasure labels, so they are served
        // below its cut (ADR 0025 PR B); among reacts, below it, only the
        // erasure labels, which it keeps, so that they still travel by sync.
        let kept_only = !kinds.is_empty()
            && kinds
                .iter()
                .all(|k| KEEP_KINDS.contains(&k.as_str()) || k == "react");
        let reacts_below = if kept_only && kinds.iter().any(|k| k == "react") {
            self.store
                .sync_ranges(net)
                .unwrap_or_default()
                .iter()
                .filter(|r| r.state == RangeState::Cut && r.phase == crate::store::PHASE_ROTATION)
                .map(|r| r.cursor_ts)
                .max()
        } else {
            None
        };
        // A `partial` requester takes what is held in a pending range.
        let holes = match (partial, before.is_some()) {
            // A descending page stops at every hole, so that a 0.4.1 peer
            // never takes what is pending here as held.
            (false, true) => self.holes(net, Holes::All, kept_only),
            // A 0.4.0 pull is refused only below what the budget left out
            // on purpose: anything else is served as held, as a 0.4.0
            // node serves what it holds.
            _ => self.holes(net, Holes::Intentional, kept_only),
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
            reacts_below,
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
                match self
                    .get_from(peer, &net, &ids, include, GET_TIMEOUT, Origin::Fetched)
                    .await
                {
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
    pub(super) fn still_running(&self) -> Result<()> {
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
                let failed = node.set_progress(&net, |p| p.failed).unwrap_or(0);
                let retry = node.config.history.retry;
                retry
                    .saturating_mul(1u32 << failed.min(10))
                    .min(RETRY_MAX.max(retry))
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
    fn walks_combine_with_the_budget_first() {
        assert_eq!(combine(End::Unconfirmed, End::Budget), End::Budget);
        assert_eq!(combine(End::Unconfirmed, End::Complete), End::Complete);
        assert_eq!(combine(End::Failed, End::Unconfirmed), End::Unconfirmed);
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
