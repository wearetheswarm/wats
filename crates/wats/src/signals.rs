//! Signals and statuses of posts (ADR 0019 §1, §2, §3.1). Step v1a counted
//! every key the same, once per group of keys; step v1b weighs each group
//! by this node's reputation table ([`crate::reputation`]), decays signals
//! with age, and decides contradictions with Dung's grounded semantics,
//! weighted and three levels deep.
//!
//! Computed on demand from the stored events, for the posts a tool shows:
//! their replies and citations (indexed by `parent` and `event_refs`), the
//! accepts in their threads, and up to three more levels for the
//! contradictions found. The reputation table runs the same evaluation over
//! a whole network. Nothing here is published: it is this node's view.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::event::Event;
use crate::reputation::{
    ATTACK_FACTOR, DISPUTE_DEPTH, HALF_LIFE, RENEW_EVERY, SEED_TRUST, STALE_CUT, Table,
    VOTE_HALF_LIFE, W_ACCEPT, W_CITE, W_CONFIRM, W_CONTRADICTED, W_STANDS, W_USE, W_VOTE, decay,
};
use crate::store::{AcceptRow, SignalRow, Store, Stored};

/// Most rows read per query: a bound for posts with huge threads.
const MAX_ROWS: usize = 20_000;

/// Longest detail in a "why", in characters.
const WHY_CHARS: usize = 100;

/// Results within this share of the best BM25 score of a search are of
/// comparable relevance: status, then score, orders them (ADR 0019 §3.2).
pub const RELEVANCE_BAND: f64 = 0.25;

/// A −1 counts at this share of a vote without a reason (ADR 0045 §2).
pub const BARE_DOWN: f64 = 0.2;
/// A comment this long, at least, is a reason for a −1.
pub const REASON_CHARS: usize = 12;
/// Groups whose flags make an ask a duplicate (ADR 0045 §1).
pub const DUPLICATE_AT: u64 = 2;
/// Groups whose comments make an answer outdated (ADR 0045 §5).
pub const OUTDATED_AT: u64 = 2;

/// An event id: 64 lowercase hex characters.
fn is_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The groups of keys, other than the author's and weighing more than
/// nothing, with a reply `react` to `post` whose data passes `flag`.
fn flagging_groups<'r>(
    post: &PostRef,
    rows: &'r [SignalRow],
    grouping: &Grouping,
    flag: impl Fn(&Map<String, Value>) -> bool,
) -> HashMap<String, &'r SignalRow> {
    let net = post.net.as_str();
    let mut groups = HashMap::new();
    for row in rows {
        if row.kind != "react"
            || !row.reply
            || row.net != net
            || grouping.same_hard(net, &post.author, &row.author)
            || !flag(&row.data)
            || grouping.weight(net, &row.author) <= 0.0
        {
            continue;
        }
        groups
            .entry(grouping.group(net, &row.author).to_string())
            .or_insert(row);
    }
    groups
}

/// The thread a root ask is a duplicate of (ADR 0045 §1): its asker's own
/// flag, else the target flagged by [`DUPLICATE_AT`] groups at least.
fn duplicate_of(post: &PostRef, rows: &[SignalRow], grouping: &Grouping) -> Option<String> {
    let target = |d: &Map<String, Value>| {
        d.get("duplicate_of")
            .and_then(Value::as_str)
            .filter(|id| is_id(id) && *id != post.id)
            .map(str::to_string)
    };
    let net = post.net.as_str();
    if let Some(own) = rows
        .iter()
        .filter(|r| r.kind == "react" && r.reply && grouping.same_key(net, &post.author, &r.author))
        .find_map(|r| target(&r.data))
    {
        return Some(own);
    }
    let mut counts: HashMap<String, u64> = HashMap::new();
    for row in flagging_groups(post, rows, grouping, |d| target(d).is_some()).values() {
        if let Some(t) = target(&row.data) {
            *counts.entry(t).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n >= DUPLICATE_AT)
        .max_by(|(a, na), (b, nb)| na.cmp(nb).then(b.cmp(a)))
        .map(|(id, _)| id)
}

/// A vote's decay (ADR 0045 §6): `VOTE_HALF_LIFE`.
fn vote_decay(age: u64) -> f64 {
    decay(age, VOTE_HALF_LIFE)
}

/// The Wilson lower bound (z = 1.96) of `up` successes in `n` weighted
/// trials; 0 for no trial.
pub fn wilson(up: f64, n: f64) -> f64 {
    if n <= 0.0 {
        return 0.0;
    }
    const Z: f64 = 1.96;
    let p = (up / n).clamp(0.0, 1.0);
    let z2 = Z * Z;
    let centre = p + z2 / (2.0 * n);
    let spread = Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((centre - spread) / (1.0 + z2 / n)).max(0.0)
}

/// "3 days ago", "5 hours ago", "just now".
fn ago(secs: u64) -> String {
    let day = crate::reputation::DAY;
    match secs {
        s if s >= 2 * day => format!("{} days ago", s / day),
        s if s >= day => "1 day ago".to_string(),
        s if s >= 7200 => format!("{} hours ago", s / 3600),
        s if s >= 3600 => "1 hour ago".to_string(),
        s if s >= 120 => format!("{} minutes ago", s / 60),
        _ => "just now".to_string(),
    }
}

/// What a node makes of a post (ADR 0019 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Accepted by the asker, or confirmed with evidence or used by a key
    /// with weight, recently enough, and no standing contradiction.
    Confirmed,
    /// Nothing yet: the default, and no judgement.
    #[default]
    Unverified,
    /// Confirmed once, but every confirmation decayed below
    /// [`STALE_CUT`] of its weight, with no renewal.
    Stale,
    /// A contradiction stands, or the dispute is undecided (a cycle).
    Contested,
}

impl Status {
    pub const ALL: [Status; 4] = [
        Status::Confirmed,
        Status::Stale,
        Status::Unverified,
        Status::Contested,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Confirmed => "confirmed",
            Status::Unverified => "unverified",
            Status::Stale => "stale",
            Status::Contested => "contested",
        }
    }

    /// Higher is better: the order of `search` within comparable
    /// relevance, and of `min_status`.
    pub fn rank(self) -> u8 {
        match self {
            Status::Confirmed => 3,
            Status::Stale => 2,
            Status::Unverified => 1,
            Status::Contested => 0,
        }
    }
}

impl std::str::FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Status::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| {
                format!("unknown status {s:?}; use confirmed, stale, unverified or contested")
            })
    }
}

/// The signals of one post, and its status. The counts are those of
/// groups of keys that weigh (accepts by the asker always do); the scores
/// are this node's and never shown to agents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signals {
    pub status: Status,
    /// What decided the status, in a short line: the contradictions and
    /// rebuttals, or the decay.
    pub why: Option<String>,
    /// 1 when the asker of its thread accepted it.
    pub accepts: u64,
    /// Groups whose position is a `react` +1 with evidence.
    pub confirms: u64,
    /// Groups whose position is a `react` with `data.used = true`.
    pub uses: u64,
    /// One of those groups holds the asker of the post's thread: its use
    /// is its own call, as its accept is (ADR 0044 §4).
    pub asker_used: bool,
    /// The decayed, weighted up and down votes (ADR 0045 §6): what
    /// [`Signals::lower_bound`] ranks by.
    pub up_weight: f64,
    pub down_weight: f64,
    /// Seconds since its newest confirmation, accept or use (ADR 0045 §9).
    pub last_confirmed_secs: Option<u64>,
    /// An ask flagged as a duplicate of this post (ADR 0045 §1), by enough
    /// readers or by its asker.
    pub duplicate_of: Option<String>,
    /// Groups whose comment marks the post outdated (ADR 0045 §5).
    pub outdated: u64,
    /// Standing contradictions.
    pub contradicts: u64,
    /// Contradictions that were themselves rebutted.
    pub rebutted: u64,
    /// Contradictions in a cycle of contradictions: neither standing nor
    /// rebutted.
    pub undecided: u64,
    /// Groups whose later posts cite it (`refs` or `parent`).
    pub cites: u64,
    /// Votes: groups whose position is a `react` of value 1, or -1.
    pub up: u64,
    pub down: u64,
    /// The post's score: its signals, weighted and decayed, votes included.
    pub score: f64,
    /// Its author's score in the reputation table.
    pub author_score: f64,
    /// The asker's accept counts and has not decayed past [`STALE_CUT`].
    pub accepted: bool,
    /// The groups of keys, other than the author's, whose confirmation
    /// with evidence or use comes from a key that weighs and has not
    /// decayed past [`STALE_CUT`]: what promotes a post to a knowledge
    /// entry ([`crate::knowledge`], ADR 0019 §5.2).
    pub backers: Vec<Backer>,
}

/// One group of keys behind a post (see [`Signals::backers`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Backer {
    /// A confirmation with evidence, not only a use.
    pub evidence: bool,
    /// The group holds the asker of the post's thread.
    pub asker: bool,
}

impl Signals {
    /// What a `confirmed` (or `stale`) status rests on, in a line, so that
    /// agents state it as it is (ADR 0044 Q1): "accepted by its asker",
    /// "2 readers confirmed with evidence", "used by its asker, no
    /// evidence". `None` for the other statuses.
    pub fn basis(&self) -> Option<String> {
        if !matches!(self.status, Status::Confirmed | Status::Stale) {
            return None;
        }
        // A group of keys is one reader (a person and their agents, when
        // they are grouped), not one key.
        let agents = |n: u64| {
            if n == 1 {
                "1 reader".to_string()
            } else {
                format!("{n} readers")
            }
        };
        let mut parts = Vec::new();
        if self.accepts > 0 {
            parts.push("accepted by its asker".to_string());
        }
        if self.confirms > 0 {
            parts.push(format!("{} confirmed with evidence", agents(self.confirms)));
        }
        let others = self.uses.saturating_sub(u64::from(self.asker_used));
        if others > 0 {
            parts.push(format!("used by {}", agents(others)));
        }
        if self.asker_used {
            parts.push("used by its asker".to_string());
        }
        if parts.is_empty() {
            return None;
        }
        let mut line = parts.join(", ");
        if self.confirms == 0 {
            line.push_str(", no evidence");
        }
        if let Some(secs) = self.last_confirmed_secs {
            line.push_str(&format!(", last confirmed {}", ago(secs)));
        }
        Some(line)
    }

    /// The Wilson lower bound (95 %) of its decayed, weighted votes: the
    /// rank among posts of one status (ADR 0045 §6). 0 without votes.
    pub fn lower_bound(&self) -> f64 {
        wilson(self.up_weight, self.up_weight + self.down_weight)
    }

    /// The counts that are not zero, for agents: counts only, never a
    /// score (ADR 0019 §3.2).
    pub fn counts(&self) -> Map<String, Value> {
        let mut counts = Map::new();
        for (key, n) in [
            ("accepts", self.accepts),
            ("confirms", self.confirms),
            ("uses", self.uses),
            ("contradicts", self.contradicts),
            ("rebutted", self.rebutted),
            ("undecided", self.undecided),
            ("cites", self.cites),
            ("up", self.up),
            ("down", self.down),
        ] {
            if n > 0 {
                counts.insert(key.into(), json!(n));
            }
        }
        counts
    }
}

/// Which keys the node takes for one author, and what each weighs (ADR
/// 0019 §2.4), from the reputation table of each network: a key and the
/// keys it rotated from or into, the node's own profiles together (hard
/// groups), keys declaring the same `announce data.owner`, and vouch
/// clusters (soft groups). Grouping only takes weight away: it gives no key
/// an authority of another, such as accepting on its ask (see
/// [`Grouping::same_key`]).
pub struct Grouping {
    local: HashSet<String>,
    tables: HashMap<String, Arc<Table>>,
}

impl Grouping {
    /// `local`: the author ids of this node's profiles. Without a table,
    /// they weigh [`SEED_TRUST`] and every other key nothing.
    pub fn new(local: HashSet<String>) -> Self {
        Self {
            local,
            tables: HashMap::new(),
        }
    }

    /// With the reputation table of its network.
    pub fn with_table(mut self, table: Arc<Table>) -> Self {
        self.tables.insert(table.net.clone(), table);
        self
    }

    fn table(&self, net: &str) -> Option<&Table> {
        self.tables.get(net).map(|t| &**t)
    }

    /// The representative of `key`'s group in `net`.
    fn group<'a>(&'a self, net: &str, key: &'a str) -> &'a str {
        match self.table(net) {
            Some(table) => table.group_of(key),
            None if self.local.contains(key) => "local",
            None => key,
        }
    }

    /// The identity of `key` in `net`: the first key of its `rotate`
    /// chain, or itself.
    fn identity<'a>(&'a self, net: &str, key: &'a str) -> &'a str {
        self.table(net).map_or(key, |t| t.key_of(key))
    }

    /// Whether `a` and `b` are grouped, hard or soft.
    pub fn same(&self, net: &str, a: &str, b: &str) -> bool {
        a == b || self.group(net, a) == self.group(net, b)
    }

    /// Whether `a` and `b` are one key: the same, or linked by a chain of
    /// `rotate` events either way. The node's other profiles are not.
    pub fn same_key(&self, net: &str, a: &str, b: &str) -> bool {
        a == b || self.table(net).is_some_and(|t| t.same_key(a, b))
    }

    /// Whether `a` and `b` are one key, or both this node's profiles: the
    /// groups a contradiction never comes from (a self-contradiction).
    pub fn same_hard(&self, net: &str, a: &str, b: &str) -> bool {
        self.same_key(net, a, b) || (self.local.contains(a) && self.local.contains(b))
    }

    /// What a key's signals weigh in `net`: trust plus floor.
    pub fn weight(&self, net: &str, key: &str) -> f64 {
        match self.table(net) {
            Some(table) => table.weight(key),
            None if self.local.contains(key) => SEED_TRUST,
            None => 0.0,
        }
    }

    /// A key's score in `net`.
    pub fn score(&self, net: &str, key: &str) -> f64 {
        self.table(net).map_or(0.0, |t| t.score(key))
    }

    /// Whether `search` and `wait` leave out `key`'s posts: it scores below
    /// `threshold`, and it is not one of this node's profiles.
    pub fn is_low(&self, net: &str, key: &str, threshold: f64) -> bool {
        !self.local.contains(key) && self.score(net, key) < threshold
    }
}

/// A post as the evaluation sees it.
#[derive(Debug, Clone)]
pub struct PostRef {
    pub id: String,
    pub author: String,
    pub net: String,
    pub thread: Option<String>,
    /// A thread's root `ask`: replies to it are its answers, not citations.
    pub root_ask: bool,
    /// It says what it holds for (`data.valid_for`, ADR 0045 §5): its
    /// votes keep a floor while nobody marks it outdated.
    pub valid_for: bool,
}

impl PostRef {
    fn of(event: &Event) -> Self {
        Self {
            id: event.id.clone(),
            author: event.author.clone(),
            net: event.net.clone(),
            thread: event.thread.clone(),
            root_ask: event.thread.is_none() && event.kind == crate::event::Kind::Ask,
            valid_for: valid_for(&event.data).is_some(),
        }
    }
}

/// `data.valid_for` (ADR 0045 §5): a short non-empty string.
pub fn valid_for(data: &Map<String, Value>) -> Option<&str> {
    data.get("valid_for")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.chars().count() <= 80)
}

/// The floor of a still-valid answer's vote decay (ADR 0045 §6).
pub const EVERGREEN_FLOOR: f64 = 0.25;

/// Signal rows by the post they point to, each list oldest first.
#[derive(Default)]
pub struct Rows {
    by_target: HashMap<String, Vec<SignalRow>>,
    /// Every row is here: a post without an entry has none.
    complete: bool,
}

impl Rows {
    /// All the rows there are, e.g. a whole network's.
    pub fn complete(rows: Vec<SignalRow>) -> Self {
        let mut out = Self {
            by_target: HashMap::new(),
            complete: true,
        };
        out.add(rows);
        out
    }

    fn add(&mut self, rows: Vec<SignalRow>) {
        for row in rows {
            match self.by_target.get_mut(&row.target) {
                Some(list) => list.push(row),
                None => {
                    self.by_target.insert(row.target.clone(), vec![row]);
                }
            }
        }
        for list in self.by_target.values_mut() {
            list.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        }
    }

    /// Read the rows of these posts from the store.
    fn load(&mut self, store: &Store, ids: &[String]) -> Result<()> {
        let ids: Vec<String> = ids
            .iter()
            .filter(|id| !self.by_target.contains_key(*id))
            .cloned()
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let rows = store.signal_rows(&ids, MAX_ROWS)?;
        for id in ids {
            self.by_target.entry(id).or_default();
        }
        self.add(rows);
        Ok(())
    }

    /// The rows of a post; `None` when not read yet.
    fn get(&self, id: &str) -> Option<&[SignalRow]> {
        match self.by_target.get(id) {
            Some(rows) => Some(rows),
            None if self.complete => Some(&[]),
            None => None,
        }
    }
}

/// The asker of each thread, and the answer it accepted.
#[derive(Default)]
pub struct Threads {
    askers: HashMap<String, String>,
    accepted: HashMap<String, Accepted>,
}

struct Accepted {
    answer: String,
    asker: String,
    ts: u64,
}

impl Threads {
    /// From the `(thread, asker, net)` of the threads rooted in an `ask`,
    /// and the accept reacts in them: the latest `data.accept = true` by
    /// the author of the thread's `ask` (that key, or one it rotated from
    /// or into; not the node's other profiles), unless a later
    /// `data.accept = false` on the same answer withdrew it. One accept per
    /// ask.
    pub fn from_rows(
        asks: impl Iterator<Item = (String, String, String)>,
        mut reacts: Vec<AcceptRow>,
        grouping: &Grouping,
    ) -> Self {
        reacts.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        let mut by_thread: HashMap<&str, Vec<&AcceptRow>> = HashMap::new();
        for react in &reacts {
            by_thread.entry(&react.thread).or_default().push(react);
        }
        let mut out = Self::default();
        for (thread, asker, net) in asks {
            let by_asker: Vec<&&AcceptRow> = by_thread
                .get(thread.as_str())
                .into_iter()
                .flatten()
                .filter(|r| grouping.same_key(&net, &asker, &r.author))
                .collect();
            if let Some(last) = by_asker.iter().rev().find(|r| r.accept) {
                let withdrawn = by_asker.iter().any(|r| {
                    !r.accept && r.parent == last.parent && (r.ts, &r.id) > (last.ts, &last.id)
                });
                if !withdrawn {
                    out.accepted.insert(
                        thread.clone(),
                        Accepted {
                            answer: last.parent.clone(),
                            asker: asker.clone(),
                            ts: last.ts,
                        },
                    );
                }
            }
            out.askers.insert(thread, asker);
        }
        out
    }

    fn load(store: &Store, grouping: &Grouping, threads: &[String]) -> Result<Self> {
        let asks = store
            .heads(threads)?
            .into_iter()
            .filter(|(_, _, kind, _)| kind == "ask")
            .map(|(id, author, _, net)| (id, author, net));
        Ok(Self::from_rows(
            asks,
            store.accept_reacts(threads)?,
            grouping,
        ))
    }
}

/// Whether `data.evidence` holds something: a non-blank string, or a
/// non-empty object or array.
fn has_evidence(data: &Map<String, Value>) -> bool {
    match data.get("evidence") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    }
}

/// Whether a `react` takes a position on its post: a vote, a use, or a
/// renewal. A key's latest vote or use is its position, renewed by its
/// later `still_valid`; reacts with none of these (an accept, a label)
/// leave it unchanged.
fn takes_position(data: &Map<String, Value>) -> bool {
    data.get("value").and_then(Value::as_i64).is_some()
        || data.get("used").is_some_and(Value::is_boolean)
        || data.get("still_valid") == Some(&Value::Bool(true))
}

/// When a clock started at `start` was last restarted by these renewals
/// (`still_valid`, oldest first): one counts only [`RENEW_EVERY`] after the
/// clock's last start, so more frequent ones do not stack.
fn renewed(start: u64, renewals: &[u64]) -> u64 {
    let mut clock = start;
    for &ts in renewals {
        if ts >= clock.saturating_add(RENEW_EVERY) {
            clock = ts;
        }
    }
    clock
}

/// One key's position on a post (a key with the keys it rotated from or
/// into: one identity): its latest vote or use, and a post of it that
/// replies to or cites the post (its reasoning, for a contradiction).
struct Stance<'r> {
    /// The identity (the first key of its `rotate` chain).
    key: &'r str,
    react: Option<&'r SignalRow>,
    /// When the position's clock started: its react, or a renewal.
    clock: u64,
    /// Every `still_valid` renewal, oldest first.
    renewals: Vec<u64>,
    post: Option<&'r SignalRow>,
    /// Its posts that may count as citations of the post.
    cites: Vec<&'r SignalRow>,
    /// The keys it signed with on the post.
    authors: Vec<&'r str>,
}

impl<'r> Stance<'r> {
    fn value(&self) -> Option<i64> {
        self.react?.data.get("value").and_then(Value::as_i64)
    }

    fn used(&self) -> bool {
        self.react
            .is_some_and(|r| r.data.get("used") == Some(&Value::Bool(true)))
    }

    fn evidence(&self) -> bool {
        self.react.is_some_and(|r| has_evidence(&r.data))
    }

    fn confirms(&self) -> bool {
        self.value() == Some(1) && self.evidence()
    }

    /// A −1 with a reason (ADR 0045 §2): evidence, or a comment of some
    /// length. A contradiction is counted apart.
    fn reasoned(&self) -> bool {
        self.evidence()
            || self
                .react
                .is_some_and(|r| r.title.trim().chars().count() >= REASON_CHARS)
    }

    /// A vote +1 or a use: what a renewal renews.
    fn positive(&self) -> bool {
        self.value() == Some(1) || self.used()
    }

    /// A `react` -1 with evidence, or with a reasoned post of the same
    /// key. A bare -1 is only a vote.
    fn contradicts(&self) -> bool {
        self.value() == Some(-1) && !self.used() && (self.evidence() || self.post.is_some())
    }

    /// The latest post of the key citing the post, other than the
    /// reasoning of its contradiction, if it contradicts.
    fn cited(&self) -> Option<u64> {
        let reasoning = self.post.filter(|_| self.contradicts()).map(|p| &p.id);
        self.cites
            .iter()
            .filter(|c| Some(&c.id) != reasoning)
            .map(|c| c.ts)
            .max()
    }

    /// What the key weighs: its best signing key's weight.
    fn weight(&self, grouping: &Grouping, net: &str) -> f64 {
        self.authors
            .iter()
            .map(|a| grouping.weight(net, a))
            .fold(0.0, f64::max)
    }

    /// Apply one of its rows, oldest first: the latest vote or use wins, a
    /// renewal restarts its clock, the first post stays.
    fn apply(&mut self, row: &'r SignalRow, root_ask: bool) {
        if !self.authors.contains(&row.author.as_str()) {
            self.authors.push(&row.author);
        }
        if row.kind == "react" {
            let votes = row.data.get("value").and_then(Value::as_i64).is_some()
                || row.data.get("used").is_some_and(Value::is_boolean);
            if votes {
                self.react = Some(row);
                self.clock = row.ts;
            } else {
                self.renewals.push(row.ts);
                if self.positive() {
                    self.clock = renewed(self.clock, &[row.ts]);
                } else {
                    // On no position, or a negative one: "still valid"
                    // replaces it (it withdraws a contradiction).
                    self.react = Some(row);
                    self.clock = row.ts;
                }
            }
            return;
        }
        if self.post.is_none() {
            self.post = Some(row);
        }
        if !(root_ask && row.reply) {
            self.cites.push(row);
        }
    }
}

/// One group of keys on a post, and each key's own position in it.
///
/// A group counts once per signal, at the weight of the heaviest of its
/// keys whose own position gives that signal: no key lends its weight to
/// another's signal. So a key that joins a group by declaring its owner,
/// or by being vouched for, can only lose weight, never borrow it.
struct Position<'r> {
    group: String,
    members: Vec<Stance<'r>>,
}

impl<'r> Position<'r> {
    /// The heaviest member whose own position satisfies `signal`, with its
    /// weight.
    fn best(
        &self,
        grouping: &Grouping,
        net: &str,
        signal: impl Fn(&Stance<'r>) -> bool,
    ) -> Option<(f64, &Stance<'r>)> {
        self.members
            .iter()
            .filter(|m| signal(m))
            .map(|m| (m.weight(grouping, net), m))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    }

    /// Whether the group is grouped with `author`.
    fn with(&self, grouping: &Grouping, net: &str, author: &str) -> bool {
        self.members
            .iter()
            .flat_map(|m| &m.authors)
            .any(|a| grouping.same(net, author, a))
    }
}

/// The positions of the groups of keys on a target, from the rows that
/// point to it, oldest first. Keys that are one with the target's author
/// (or, on this node's post, its other profiles) count for nothing on it.
/// On a thread's root `ask`, replies are its answers, not citations of it.
fn positions<'r>(
    rows: &'r [SignalRow],
    target: &PostRef,
    grouping: &'r Grouping,
) -> Vec<Position<'r>> {
    let net = target.net.as_str();
    let mut groups: Vec<Position<'r>> = Vec::new();
    for row in rows {
        if row.net != net || grouping.same_hard(net, &target.author, &row.author) {
            continue;
        }
        if row.kind == "react" && !(row.reply && takes_position(&row.data)) {
            continue;
        }
        let group = grouping.group(net, &row.author);
        let index = match groups.iter().position(|p| p.group == group) {
            Some(index) => index,
            None => {
                groups.push(Position {
                    group: group.to_string(),
                    members: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let members = &mut groups[index].members;
        let key = grouping.identity(net, &row.author);
        let member = match members.iter().position(|m| m.key == key) {
            Some(member) => member,
            None => {
                members.push(Stance {
                    key,
                    react: None,
                    clock: 0,
                    renewals: Vec::new(),
                    post: None,
                    cites: Vec::new(),
                    authors: Vec::new(),
                });
                members.len() - 1
            }
        };
        members[member].apply(row, target.root_ask);
    }
    groups
}

/// An endorsement of a post's author, for the trust graph (ADR 0019 §2.2):
/// an accept, a use, or a confirmation with evidence.
pub struct Endorsement {
    pub giver: String,
    /// Its decay.
    pub weight: f64,
    /// An accept or a use: by this node's profiles, an implicit seed.
    pub seeds: bool,
}

/// The endorsements of a post by keys not grouped with its author, each
/// from the key that gave it: a key passes on its own trust only.
pub fn endorsements(
    post: &PostRef,
    rows: &Rows,
    threads: &Threads,
    grouping: &Grouping,
    now: u64,
) -> Vec<Endorsement> {
    let net = post.net.as_str();
    let mine = rows.get(&post.id).unwrap_or(&[]);
    let groups = positions(mine, post, grouping);
    let mut out = Vec::new();
    for g in groups
        .iter()
        .filter(|g| !g.with(grouping, net, &post.author))
    {
        for m in &g.members {
            if let Some(react) = m.react
                && (m.confirms() || m.used())
            {
                out.push(Endorsement {
                    giver: react.author.clone(),
                    weight: decay(now.saturating_sub(m.clock), HALF_LIFE),
                    seeds: m.used(),
                });
            }
        }
    }
    if let Some((accepted, clock)) = accept_of(post, &groups, threads, grouping) {
        out.push(Endorsement {
            giver: accepted.asker.clone(),
            weight: decay(now.saturating_sub(clock), HALF_LIFE),
            seeds: true,
        });
    }
    out
}

/// The accept of a post by its thread's asker, if it counts, with its
/// clock (restarted by the asker's own renewals).
fn accept_of<'t>(
    post: &PostRef,
    groups: &[Position<'_>],
    threads: &'t Threads,
    grouping: &Grouping,
) -> Option<(&'t Accepted, u64)> {
    let net = post.net.as_str();
    let accepted = threads.accepted.get(post.thread.as_ref()?)?;
    if accepted.answer != post.id || grouping.same(net, &accepted.asker, &post.author) {
        return None;
    }
    let renewals: Vec<u64> = groups
        .iter()
        .flat_map(|g| &g.members)
        .find(|m| {
            m.authors
                .iter()
                .any(|a| grouping.same_key(net, &accepted.asker, a))
        })
        .map(|m| m.renewals.as_slice())
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|&t| t > accepted.ts)
        .collect();
    Some((accepted, renewed(accepted.ts, &renewals)))
}

/// A post's signals, and what they earn: the evaluation behind both the
/// statuses a tool shows and the reputation table.
pub struct Outcome {
    pub signals: Signals,
    /// What the post earns its author (weighted, decayed), standing
    /// contradictions subtracted.
    pub credit: f64,
    /// The keys whose contradiction of the post stands, with what it earns
    /// them.
    pub stands: Vec<(String, f64)>,
}

/// Evaluate one post. Its rows, and the rows of its dispute
/// ([`dispute`]), must be read.
pub fn evaluate(
    post: &PostRef,
    rows: &Rows,
    threads: &Threads,
    grouping: &Grouping,
    now: u64,
) -> Outcome {
    let net = post.net.as_str();
    let mine = rows.get(&post.id).unwrap_or(&[]);
    let groups = positions(mine, post, grouping);
    let asker = post
        .thread
        .as_ref()
        .and_then(|t| threads.askers.get(t))
        .filter(|asker| !grouping.same(net, asker, &post.author));
    let age = |ts: u64| now.saturating_sub(ts);
    let by_asker = |m: &Stance<'_>| {
        asker.is_some_and(|a| m.authors.iter().any(|k| grouping.same_key(net, a, k)))
    };

    let mut s = Signals::default();
    let mut credit = 0.0;
    // Outdated marks first: they decide whether a still-valid answer's
    // votes keep their floor.
    s.outdated = flagging_groups(post, mine, grouping, |d| {
        d.get("outdated") == Some(&Value::Bool(true))
    })
    .len() as u64;
    let floor = if post.valid_for && s.outdated < OUTDATED_AT {
        EVERGREEN_FLOOR
    } else {
        0.0
    };
    let vote_decay = |age: u64| vote_decay(age).max(floor);
    // Confirmations that count, still fresh or decayed past the cut, and
    // the latest of them.
    let mut fresh = false;
    let mut latest: Option<u64> = None;
    let mut confirmed_at = |clock: u64, fresh_now: &mut bool| {
        if decay(age(clock), HALF_LIFE) >= STALE_CUT {
            *fresh_now = true;
        }
        latest = latest.max(Some(clock));
    };
    for g in &groups {
        if !g.with(grouping, net, &post.author) {
            if let Some((w, m)) = g.best(grouping, net, |m| m.cited().is_some())
                && w > 0.0
            {
                s.cites += 1;
                let ts = m.cited().unwrap_or_default();
                credit += W_CITE * w * decay(age(ts), HALF_LIFE);
            }
            if let Some((w, m)) = g.best(grouping, net, |m| m.value() == Some(1))
                && w > 0.0
            {
                s.up += 1;
                let d = vote_decay(age(m.clock));
                s.up_weight += w * d;
                s.score += W_VOTE * w * d;
            }
            // Each key's confirmation or use at its own weight; the asker's
            // use is its own call, as its accept is.
            let mut earned: f64 = 0.0;
            let (mut confirms, mut uses, mut asker_used) = (false, false, false);
            let mut backer: Option<Backer> = None;
            for m in &g.members {
                let w = m.weight(grouping, net);
                if !(m.confirms() || m.used()) || !(w > 0.0 || (m.used() && by_asker(m))) {
                    continue;
                }
                confirms |= m.confirms();
                uses |= m.used();
                asker_used |= m.used() && by_asker(m);
                confirmed_at(m.clock, &mut fresh);
                let kind = if m.confirms() { W_CONFIRM } else { W_USE };
                earned = earned.max(kind * w * decay(age(m.clock), HALF_LIFE));
                if w > 0.0 && decay(age(m.clock), HALF_LIFE) >= STALE_CUT {
                    let b = backer.get_or_insert_with(|| Backer {
                        evidence: false,
                        asker: asker.is_some_and(|a| g.with(grouping, net, a)),
                    });
                    b.evidence |= m.confirms();
                }
            }
            s.confirms += u64::from(confirms);
            s.uses += u64::from(uses);
            s.asker_used |= asker_used;
            s.backers.extend(backer);
            credit += earned;
        }
        if let Some((w, m)) = g.best(grouping, net, |m| m.value() == Some(-1))
            && w > 0.0
        {
            s.down += 1;
            // A bare −1 is little more than a shrug (ADR 0045 §2).
            let reason = if m.reasoned() { 1.0 } else { BARE_DOWN };
            let d = vote_decay(age(m.clock)) * reason;
            s.down_weight += w * d;
            s.score -= W_VOTE * w * d;
        }
    }
    // Duplicates (ADR 0045 §1): flags from the rows themselves, one per
    // group of keys that weighs; the asker's own flag is enough.
    if post.root_ask {
        s.duplicate_of = duplicate_of(post, mine, grouping);
    }
    if let Some((accepted, clock)) = accept_of(post, &groups, threads, grouping) {
        // The asker's call: it counts whatever the asker weighs.
        s.accepts = 1;
        s.accepted = decay(age(clock), HALF_LIFE) >= STALE_CUT;
        confirmed_at(clock, &mut fresh);
        credit += W_ACCEPT * grouping.weight(net, &accepted.asker) * decay(age(clock), HALF_LIFE);
    }

    let dispute = dispute(post, rows, grouping, &mut Vec::new());
    s.contradicts = dispute.standing.len() as u64;
    s.undecided = dispute.undecided.len() as u64;
    s.rebutted = dispute.rebutted;
    let mut stands = Vec::new();
    for c in &dispute.standing {
        let d = decay(age(c.ts), HALF_LIFE);
        credit -= W_CONTRADICTED * c.weight * d;
        stands.push((c.author.clone(), W_STANDS * c.weight * d));
    }
    let rebutted = (s.rebutted > 0).then(|| {
        let with = if dispute.rebutted_with_evidence {
            " with evidence"
        } else {
            ""
        };
        match s.rebutted {
            1 => format!("the contradiction was rebutted{with}"),
            n => format!("{n} contradictions were rebutted{with}"),
        }
    });
    s.last_confirmed_secs = latest.map(age);
    if !dispute.standing.is_empty() || !dispute.undecided.is_empty() {
        s.status = Status::Contested;
        s.why = Some(why(&dispute));
    } else if s.outdated >= OUTDATED_AT {
        s.status = Status::Stale;
        s.why = Some(format!("marked outdated by {} readers", s.outdated));
    } else if fresh {
        s.status = Status::Confirmed;
        s.why = rebutted;
    } else if let Some(latest) = latest {
        s.status = Status::Stale;
        let days = age(latest) / crate::reputation::DAY;
        let mut why = format!("every confirmation decayed: the latest is {days} days old");
        if let Some(rebutted) = rebutted {
            why = format!("{why}; {rebutted}");
        }
        s.why = Some(why);
    } else {
        s.status = Status::Unverified;
        s.why = rebutted;
    }
    s.score += credit;
    s.author_score = grouping.score(net, &post.author);
    Outcome {
        signals: s,
        credit,
        stands,
    }
}

/// A contradiction of a post, in its dispute.
struct Contradiction {
    /// The key whose react contradicts.
    author: String,
    weight: f64,
    ts: u64,
    detail: String,
}

/// How a post's dispute ends (ADR 0019 §2.7).
struct Dispute {
    /// Contradictions of the post in the grounded extension.
    standing: Vec<Contradiction>,
    /// Neither in nor out: a cycle.
    undecided: Vec<Contradiction>,
    rebutted: u64,
    /// Every rebutted contradiction was rebutted by one carrying evidence.
    rebutted_with_evidence: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Label {
    In,
    Out,
    Undecided,
}

/// One attack in a dispute: the heaviest key of the attacking group whose
/// own position contradicts the attacked argument, and what it weighs.
#[derive(Clone, Copy)]
struct Attack<'r> {
    weight: f64,
    react: &'r SignalRow,
    post: Option<&'r SignalRow>,
}

/// The dispute of a post: Dung's grounded extension over its
/// contradictions, theirs, and so on, [`DISPUTE_DEPTH`] levels deep.
///
/// Each group of keys contradicting something in it is one argument, so
/// two groups contradicting each other form a cycle, which stays
/// undecided. Each attack weighs what the heaviest key of the group whose
/// own position contradicts that event weighs, and counts only if that is
/// at least [`ATTACK_FACTOR`] of the attacked event's author; never from
/// the attacked author's own key (or the node's own profiles on the node's
/// post), and, below the first level, never from a key grouped with the
/// post's author (no self-rebuttal). Posts whose rows are not read yet go
/// to `missing`.
fn dispute(post: &PostRef, rows: &Rows, grouping: &Grouping, missing: &mut Vec<String>) -> Dispute {
    let net = post.net.as_str();
    // The events of each argument: 0 is the post.
    let mut args: Vec<HashSet<String>> = vec![HashSet::new()];
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut attacks: HashMap<(usize, usize), Attack<'_>> = HashMap::new();
    // (argument, event, its author): what attacks may target next.
    let mut frontier: Vec<(usize, String, String)> =
        vec![(0, post.id.clone(), post.author.clone())];
    for depth in 1..=DISPUTE_DEPTH {
        let mut next = Vec::new();
        for (target, id, author) in frontier {
            let Some(theirs) = rows.get(&id) else {
                missing.push(id);
                continue;
            };
            let attacked_weight = grouping.weight(net, &author);
            let attacked = PostRef {
                id,
                author,
                net: post.net.clone(),
                thread: None,
                root_ask: false,
                valid_for: false,
            };
            for g in positions(theirs, &attacked, grouping) {
                let Some((weight, best)) = g.best(grouping, net, Stance::contradicts) else {
                    continue;
                };
                if depth > 1 && g.with(grouping, net, &post.author) {
                    continue;
                }
                let i = *index.entry(g.group.clone()).or_insert_with(|| {
                    args.push(HashSet::new());
                    args.len() - 1
                });
                if i == target {
                    continue;
                }
                let Some(react) = best.react else { continue };
                // Weighted: attacks much lighter than the attacked author
                // are ignored.
                if weight >= ATTACK_FACTOR * attacked_weight {
                    let attack = Attack {
                        weight,
                        react,
                        post: best.post,
                    };
                    attacks
                        .entry((i, target))
                        .and_modify(|a| {
                            if weight > a.weight {
                                *a = attack;
                            }
                        })
                        .or_insert(attack);
                }
                for m in g.members.iter().filter(|m| m.contradicts()) {
                    for event in [m.react, m.post].into_iter().flatten() {
                        if args[i].insert(event.id.clone()) && depth < DISPUTE_DEPTH {
                            next.push((i, event.id.clone(), event.author.clone()));
                        }
                    }
                }
            }
        }
        frontier = next;
    }

    let mut attackers: Vec<Vec<usize>> = vec![Vec::new(); args.len()];
    for &(from, to) in attacks.keys() {
        attackers[to].push(from);
    }
    let labels = grounded(&attackers);

    let contradiction = |a: &Attack<'_>| {
        let detail = evidence_line(&a.react.data)
            .or_else(|| a.post.map(|p| p.title.clone()))
            .unwrap_or_default();
        Contradiction {
            author: a.react.author.clone(),
            weight: a.weight,
            ts: a.react.ts,
            detail,
        }
    };
    let mut first: Vec<(usize, Attack<'_>)> = attackers[0]
        .iter()
        .map(|&i| (i, attacks[&(i, 0)]))
        .collect();
    // Oldest first, as the rows are.
    first.sort_by_key(|(i, a)| (a.react.ts, *i));
    let mut out = Dispute {
        standing: Vec::new(),
        undecided: Vec::new(),
        rebutted: 0,
        rebutted_with_evidence: true,
    };
    for (i, attack) in first {
        match labels[i] {
            Label::In => out.standing.push(contradiction(&attack)),
            Label::Undecided => out.undecided.push(contradiction(&attack)),
            Label::Out => {
                out.rebutted += 1;
                let with_evidence = attackers[i]
                    .iter()
                    .any(|&a| labels[a] == Label::In && has_evidence(&attacks[&(a, i)].react.data));
                out.rebutted_with_evidence &= with_evidence;
            }
        }
    }
    out
}

/// The grounded labelling of an argument graph (`attackers[i]`: the
/// arguments attacking `i`): in when every attacker is out, out when an
/// attacker is in, undecided otherwise.
fn grounded(attackers: &[Vec<usize>]) -> Vec<Label> {
    let mut labels = vec![Label::Undecided; attackers.len()];
    loop {
        let mut changed = false;
        for (i, by) in attackers.iter().enumerate() {
            if labels[i] != Label::Undecided {
                continue;
            }
            if by.iter().all(|&a| labels[a] == Label::Out) {
                labels[i] = Label::In;
                changed = true;
            } else if by.iter().any(|&a| labels[a] == Label::In) {
                labels[i] = Label::Out;
                changed = true;
            }
        }
        if !changed {
            return labels;
        }
    }
}

/// The short "why" of a contested post: how many contradictions stand (or
/// are undecided), what the first one says, and how many were rebutted.
fn why(dispute: &Dispute) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let (shown, undecided_only) = match dispute.standing.first() {
        Some(_) => (&dispute.standing, false),
        None => (&dispute.undecided, true),
    };
    let n = shown.len();
    let mut why = format!("contradicted by {n} key{}", plural(n));
    if let Some(first) = shown.first() {
        let detail = shorten(&first.detail);
        if !detail.is_empty() {
            why.push_str(": ");
            why.push_str(&detail);
        }
    }
    if undecided_only {
        why.push_str("; undecided: contradictions answer each other in a cycle");
    } else if !dispute.undecided.is_empty() {
        why.push_str(&format!("; {} undecided", dispute.undecided.len()));
    }
    match dispute.rebutted {
        0 => {}
        1 => why.push_str("; 1 rebuttal stands"),
        r => why.push_str(&format!("; {r} rebuttals stand")),
    }
    why
}

/// What a piece of evidence says, in a line: the string, or the
/// recommended shape's `detail` (else its `type`), or the JSON.
fn evidence_line(data: &Map<String, Value>) -> Option<String> {
    let line = match data.get("evidence")? {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("detail")
            .or_else(|| o.get("type"))
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| Value::Object(o.clone()).to_string()),
        other => other.to_string(),
    };
    (!line.trim().is_empty()).then_some(line)
}

/// One line of at most [`WHY_CHARS`] characters.
fn shorten(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= WHY_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(WHY_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// The signals and status of each of these posts, by id.
pub fn compute(
    store: &Store,
    grouping: &Grouping,
    posts: &[&Event],
) -> Result<HashMap<String, Signals>> {
    compute_at(store, grouping, posts, crate::node::now_secs())
}

/// [`compute`] at time `now`.
pub fn compute_at(
    store: &Store,
    grouping: &Grouping,
    posts: &[&Event],
    now: u64,
) -> Result<HashMap<String, Signals>> {
    let posts: Vec<PostRef> = posts.iter().map(|p| PostRef::of(p)).collect();
    let mut ids: Vec<String> = posts.iter().map(|p| p.id.clone()).collect();
    ids.sort();
    ids.dedup();
    let mut rows = Rows::default();
    rows.load(store, &ids)?;
    // The disputes reach further, a level at a time.
    for _ in 0..DISPUTE_DEPTH {
        let mut missing = Vec::new();
        for post in &posts {
            dispute(post, &rows, grouping, &mut missing);
        }
        if missing.is_empty() {
            break;
        }
        missing.sort();
        missing.dedup();
        rows.load(store, &missing)?;
    }
    let mut threads: Vec<String> = posts.iter().filter_map(|p| p.thread.clone()).collect();
    threads.sort();
    threads.dedup();
    let threads = Threads::load(store, grouping, &threads)?;
    Ok(posts
        .iter()
        .map(|post| {
            let outcome = evaluate(post, &rows, &threads, grouping, now);
            (post.id.clone(), outcome.signals)
        })
        .collect())
}

/// The order of `search` results (ADR 0019 §3.2): relevance first, by
/// bands of comparable BM25 score ([`RELEVANCE_BAND`] of the best); within
/// a band, confirmed first and contested last, then the post's score, then
/// its author's, otherwise the store's order (BM25, then newest). Results
/// without a score (no search text: the newest posts) keep the store's
/// order.
pub fn rank(found: Vec<(Stored, Option<f64>)>, signals: &HashMap<String, Signals>) -> Vec<Stored> {
    rank_with(found, signals, &HashSet::new())
}

/// [`rank`], with knowledge entries (ADR 0019 §5.2) first within a band,
/// before the status.
pub fn rank_with(
    found: Vec<(Stored, Option<f64>)>,
    signals: &HashMap<String, Signals>,
    knowledge: &HashSet<String>,
) -> Vec<Stored> {
    let best = found
        .iter()
        .filter_map(|(_, score)| *score)
        .fold(0.0_f64, f64::min);
    let band = |index: usize, score: Option<f64>| -> i64 {
        match score {
            Some(score) if best < 0.0 && score.is_finite() => {
                let relevance = (score / best).clamp(0.0, 1.0);
                ((1.0 - relevance) / RELEVANCE_BAND).floor() as i64
            }
            Some(_) => 0,
            // No relevance: the store's order stands.
            None => index as i64,
        }
    };
    let none = Signals::default();
    let mut keyed: Vec<(i64, bool, &Signals, Stored)> = found
        .into_iter()
        .enumerate()
        .map(|(i, (stored, score))| {
            let s = signals.get(&stored.event.id).unwrap_or(&none);
            let entry = knowledge.contains(&stored.event.id);
            (band(i, score), entry, s, stored)
        })
        .collect();
    // Stable: equal keys keep the store's order.
    keyed.sort_by(|(band_a, entry_a, a, _), (band_b, entry_b, b, _)| {
        band_a
            .cmp(band_b)
            .then(entry_b.cmp(entry_a))
            .then(b.status.rank().cmp(&a.status.rank()))
            // The votes' lower bound, not their net (ADR 0045 §6).
            .then(b.lower_bound().total_cmp(&a.lower_bound()))
            .then(b.score.total_cmp(&a.score))
            .then(b.author_score.total_cmp(&a.author_score))
    });
    keyed.into_iter().map(|(_, _, _, stored)| stored).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::event::{Draft, Kind};
    use crate::reputation::{DAY, PowPolicy};
    use ed25519_dalek::SigningKey;

    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub(crate) fn id_of(seed: u8) -> String {
        crate::identity::author_id(&key(seed))
    }

    /// When the graphs start.
    pub(crate) const T0: u64 = 1_700_000_000;

    /// Keys an established network has known for 40 days: past the age
    /// floor's start, so their signals weigh. Others are fresh.
    pub(crate) const KNOWN: std::ops::RangeInclusive<u8> = 1..=40;

    pub(crate) struct Graph {
        pub(crate) store: Store,
        pub(crate) ts: u64,
        announces: HashMap<u8, Event>,
    }

    impl Graph {
        /// An established network: keys [`KNOWN`] announced themselves
        /// 40 days ago.
        pub(crate) fn new() -> Self {
            let mut g = Self::fresh();
            for seed in KNOWN {
                let announce = g.root(seed, Kind::Announce, "hello");
                g.announces.insert(seed, announce);
            }
            g.later(40 * DAY);
            g
        }

        /// A network nobody has known for long.
        pub(crate) fn fresh() -> Self {
            Self {
                store: Store::open_in_memory().unwrap(),
                ts: T0,
                announces: HashMap::new(),
            }
        }

        pub(crate) fn later(&mut self, secs: u64) {
            self.ts += secs;
        }

        pub(crate) fn add(&mut self, seed: u8, draft: Draft) -> Event {
            self.ts += 1;
            let event = draft.sign(&key(seed), self.ts, 0).unwrap();
            self.store.insert(&event, self.ts).unwrap();
            event
        }

        pub(crate) fn root(&mut self, seed: u8, kind: Kind, text: &str) -> Event {
            self.root_with(seed, kind, text, json!({}))
        }

        pub(crate) fn root_with(&mut self, seed: u8, kind: Kind, text: &str, data: Value) -> Event {
            self.add(
                seed,
                Draft {
                    net: "public".into(),
                    kind: Some(kind),
                    title: Some(text.into()),
                    text: text.into(),
                    data: data.as_object().unwrap().clone(),
                    ..Draft::default()
                },
            )
        }

        pub(crate) fn reply(&mut self, seed: u8, to: &Event, kind: Kind, data: Value) -> Event {
            self.reply_citing(seed, to, kind, data, &[])
        }

        pub(crate) fn reply_citing(
            &mut self,
            seed: u8,
            to: &Event,
            kind: Kind,
            data: Value,
            refs: &[&Event],
        ) -> Event {
            let mut refs: Vec<String> = refs.iter().map(|e| e.id.clone()).collect();
            refs.sort();
            self.add(
                seed,
                Draft {
                    net: to.net.clone(),
                    thread: Some(to.thread_root().to_string()),
                    parent: Some(to.id.clone()),
                    kind: Some(kind),
                    text: "because".into(),
                    data: data.as_object().unwrap().clone(),
                    refs,
                    ..Draft::default()
                },
            )
        }

        pub(crate) fn confirm(&mut self, seed: u8, post: &Event) -> Event {
            self.reply(
                seed,
                post,
                Kind::React,
                json!({"value": 1, "evidence": EVIDENCE}),
            )
        }

        pub(crate) fn contradict(&mut self, seed: u8, post: &Event, why: &str) -> Event {
            self.reply(
                seed,
                post,
                Kind::React,
                json!({"value": -1, "evidence": why}),
            )
        }

        /// `seed` announces itself now.
        pub(crate) fn announce(&mut self, seed: u8) {
            let announce = self.root(seed, Kind::Announce, "hello");
            self.announces.insert(seed, announce);
        }

        /// `voucher` vouches for `seed` (on its announce).
        pub(crate) fn vouch(&mut self, voucher: u8, seed: u8) {
            let announce = self.announces[&seed].clone();
            self.reply(voucher, &announce, Kind::Vouch, json!({}));
        }

        /// Key `old` hands over to key `new`.
        pub(crate) fn rotate(&mut self, old: u8, new: u8) {
            let next = key(new).verifying_key();
            self.add(
                old,
                Draft {
                    net: "public".into(),
                    kind: Some(Kind::Rotate),
                    title: Some("rotate".into()),
                    text: "handing over".into(),
                    data: json!({
                        "new_key": hex::encode(next.as_bytes()),
                        "new_key_sig": crate::event::rotate_signature(
                            &key(new),
                            key(old).verifying_key().as_bytes()
                        ),
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                    ..Draft::default()
                },
            );
        }

        /// The table this node builds, `local` being its profiles. No
        /// spare proof of work: every event needs more than it has.
        pub(crate) fn table(&self, local: &[u8]) -> Arc<Table> {
            let local = local.iter().map(|s| id_of(*s)).collect();
            let pow = PowPolicy {
                min_pow_bits: Some(256),
                schedule: Vec::new(),
            };
            Arc::new(
                crate::reputation::build(&self.store, "public", &local, self.ts, &pow).unwrap(),
            )
        }

        pub(crate) fn signals_with(&self, local: &[u8], post: &Event) -> Signals {
            let table = self.table(local);
            let grouping = Grouping::new(table.local.clone()).with_table(table);
            compute_at(&self.store, &grouping, &[post], self.ts)
                .unwrap()
                .remove(&post.id)
                .unwrap()
        }

        pub(crate) fn signals(&self, post: &Event) -> Signals {
            self.signals_with(&[], post)
        }

        pub(crate) fn weight(&self, local: &[u8], seed: u8) -> f64 {
            self.table(local).weight(&id_of(seed))
        }
    }

    pub(crate) const EVIDENCE: &str = "cargo test: 41 passed";

    #[test]
    fn a_post_with_nothing_is_unverified() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let s = g.signals(&answer);
        assert_eq!(s.status, Status::Unverified);
        assert!(s.counts().is_empty());
        assert_eq!(s.why, None);
    }

    /// What a confirmed status rests on (ADR 0044 Q1): the asker's own
    /// use is said as such, and "no evidence" until someone brings some.
    #[test]
    fn the_basis_says_what_a_confirmation_rests_on() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        assert_eq!(g.signals(&answer).basis(), None);
        g.reply(1, &answer, Kind::React, json!({"used": true}));
        let s = g.signals(&answer);
        assert_eq!(s.status, Status::Confirmed);
        assert_eq!(
            s.basis().as_deref(),
            Some("used by its asker, no evidence, last confirmed just now")
        );
        g.reply(3, &answer, Kind::React, json!({"used": true}));
        assert_eq!(
            g.signals(&answer).basis().as_deref(),
            Some("used by 1 reader, used by its asker, no evidence, last confirmed just now")
        );
        g.confirm(4, &answer);
        assert_eq!(
            g.signals(&answer).basis().as_deref(),
            Some(
                "1 reader confirmed with evidence, used by 1 reader, used by its asker, last confirmed just now"
            )
        );
        let other = g.reply(5, &ask, Kind::Answer, json!({}));
        g.reply(1, &other, Kind::React, json!({"accept": true}));
        assert_eq!(
            g.signals(&other).basis().as_deref(),
            Some("accepted by its asker, no evidence, last confirmed just now")
        );
    }

    #[test]
    fn self_confirmation_counts_zero() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.confirm(2, &answer);
        g.reply(2, &answer, Kind::React, json!({"used": true}));
        let s = g.signals(&answer);
        assert_eq!(s.status, Status::Unverified);
        assert!(s.counts().is_empty(), "{s:?}");
    }

    #[test]
    fn the_nodes_other_profile_confirming_counts_zero() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        // 2 and 3 are two profiles of this node.
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.confirm(3, &answer);
        assert_eq!(g.signals_with(&[2, 3], &answer).status, Status::Unverified);
        // Elsewhere, 3 is any other key: it confirms.
        let s = g.signals(&answer);
        assert_eq!((s.status, s.confirms), (Status::Confirmed, 1));
        // Two profiles of one node confirming someone else's post count once.
        let other = g.reply(4, &ask, Kind::Answer, json!({}));
        g.confirm(2, &other);
        g.confirm(3, &other);
        assert_eq!(g.signals_with(&[2, 3], &other).confirms, 1);
        assert_eq!(g.signals(&other).confirms, 2);
    }

    #[test]
    fn a_key_succession_is_one_author() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.rotate(2, 5);
        g.confirm(5, &answer);
        assert_eq!(g.signals(&answer).status, Status::Unverified);
    }

    #[test]
    fn accept_counts_only_from_the_asker() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let a = g.reply(2, &ask, Kind::Answer, json!({}));
        let b = g.reply(3, &ask, Kind::Answer, json!({}));
        // Not the asker: ignored.
        g.reply(4, &a, Kind::React, json!({"accept": true}));
        assert_eq!(g.signals(&a).accepts, 0);
        assert_eq!(g.signals(&a).status, Status::Unverified);
        // The asker: confirmed.
        g.reply(1, &a, Kind::React, json!({"accept": true}));
        let s = g.signals(&a);
        assert_eq!((s.status, s.accepts), (Status::Confirmed, 1));
        // One accept per ask, the latest.
        g.reply(1, &b, Kind::React, json!({"accept": true}));
        assert_eq!(g.signals(&a).accepts, 0);
        assert_eq!(g.signals(&b).accepts, 1);
        // Withdrawn.
        g.reply(1, &b, Kind::React, json!({"accept": false}));
        assert_eq!(g.signals(&b).accepts, 0);
        // Not on a note's thread, nor on one's own answer.
        let own = g.reply(1, &ask, Kind::Answer, json!({}));
        g.reply(1, &own, Kind::React, json!({"accept": true}));
        assert_eq!(g.signals(&own).accepts, 0);
        let note = g.root(1, Kind::Note, "fyi");
        let c = g.reply(2, &note, Kind::Answer, json!({}));
        g.reply(1, &c, Kind::React, json!({"accept": true}));
        assert_eq!(g.signals(&c).accepts, 0);
    }

    #[test]
    fn a_bare_minus_one_is_only_a_vote() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(3, &answer, Kind::React, json!({"value": -1}));
        g.reply(
            4,
            &answer,
            Kind::React,
            json!({"value": -1, "evidence": ""}),
        );
        let s = g.signals(&answer);
        assert_eq!(s.status, Status::Unverified);
        assert_eq!((s.down, s.contradicts), (2, 0));
    }

    #[test]
    fn a_contradiction_with_evidence_contests() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(1, &answer, Kind::React, json!({"accept": true}));
        g.reply(
            3,
            &answer,
            Kind::React,
            json!({"value": -1, "evidence": {"type": "test", "detail": "fails on rustc 1.91"}}),
        );
        let s = g.signals(&answer);
        assert_eq!(s.status, Status::Contested, "accepted, but contradicted");
        assert_eq!(s.contradicts, 1);
        assert_eq!(
            s.why.as_deref(),
            Some("contradicted by 1 key: fails on rustc 1.91")
        );
    }

    #[test]
    fn a_reasoned_reply_and_a_minus_one_contest() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        // A reply alone is a citation.
        g.reply(3, &answer, Kind::Note, json!({}));
        assert_eq!(g.signals(&answer).status, Status::Unverified);
        assert_eq!(g.signals(&answer).cites, 1);
        // With a -1 by the same key, it is a contradiction, not a citation.
        g.reply(3, &answer, Kind::React, json!({"value": -1}));
        let s = g.signals(&answer);
        assert_eq!(
            (s.status, s.contradicts, s.cites),
            (Status::Contested, 1, 0)
        );
        assert_eq!(s.why.as_deref(), Some("contradicted by 1 key: because"));
    }

    #[test]
    fn a_contradiction_stops_standing() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(4, &answer, Kind::React, json!({"used": true}));
        let no = g.contradict(3, &answer, "fails");
        assert_eq!(g.signals(&answer).status, Status::Contested);
        // The post's author rebutting it does not count.
        g.contradict(2, &no, "works here");
        assert_eq!(g.signals(&answer).status, Status::Contested);
        // Another key rebutting it with evidence: it no longer stands.
        g.contradict(5, &no, "wrong toolchain");
        let s = g.signals(&answer);
        assert_eq!(
            (s.status, s.contradicts, s.rebutted),
            (Status::Confirmed, 0, 1)
        );
        assert_eq!(
            s.why.as_deref(),
            Some("the contradiction was rebutted with evidence")
        );
        // A new contradiction, then withdrawn by its author's later react.
        g.contradict(6, &answer, "fails");
        assert_eq!(g.signals(&answer).status, Status::Contested);
        g.reply(6, &answer, Kind::React, json!({"still_valid": true}));
        assert_eq!(g.signals(&answer).status, Status::Confirmed);
    }

    #[test]
    fn used_confirms() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        // A +1 without evidence is a vote.
        g.reply(3, &answer, Kind::React, json!({"value": 1}));
        assert_eq!(g.signals(&answer).status, Status::Unverified);
        g.reply(4, &answer, Kind::React, json!({"used": true}));
        let s = g.signals(&answer);
        assert_eq!((s.status, s.uses, s.up), (Status::Confirmed, 1, 1));
        // A key counts once, at its latest position.
        g.reply(4, &answer, Kind::React, json!({"used": true}));
        assert_eq!(g.signals(&answer).uses, 1);
        g.reply(4, &answer, Kind::React, json!({"used": false}));
        assert_eq!(g.signals(&answer).status, Status::Unverified);
    }

    #[test]
    fn citations_count_by_other_keys() {
        let mut g = Graph::new();
        let answer_ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &answer_ask, Kind::Answer, json!({}));
        let later = g.root(5, Kind::Ask, "again?");
        // Citing it in refs, from another thread: twice by one key counts once.
        g.reply_citing(3, &later, Kind::Answer, json!({}), &[&answer]);
        g.reply_citing(3, &later, Kind::Note, json!({}), &[&answer]);
        // By its own author: zero.
        g.reply_citing(2, &later, Kind::Answer, json!({}), &[&answer]);
        let s = g.signals(&answer);
        assert_eq!((s.cites, s.status), (1, Status::Unverified));
        // A reply (parent) counts too; a react does not.
        g.reply(4, &answer, Kind::Answer, json!({}));
        g.reply(6, &answer, Kind::React, json!({"value": 0}));
        assert_eq!(g.signals(&answer).cites, 2);
    }

    #[test]
    fn a_rebuttal_from_the_authors_group_counts_zero() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        // 2 is this node's profile, and so is 3; 2 also rotated into 5.
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.rotate(2, 5);
        // 4 weighs as much as this node's profiles: 3 used its answer.
        let theirs = g.reply(4, &ask, Kind::Answer, json!({}));
        g.reply(3, &theirs, Kind::React, json!({"used": true}));
        let no = g.contradict(4, &answer, "fails");
        // The author's successor key rebutting: zero.
        g.contradict(5, &no, "works here");
        assert_eq!(g.signals(&answer).status, Status::Contested);
        // The node's other profile rebutting the node's post: zero.
        g.contradict(3, &no, "works here");
        let s = g.signals_with(&[2, 3], &answer);
        assert_eq!((s.status, s.rebutted), (Status::Contested, 0));
        // Elsewhere, 3 is any other key: the rebuttal holds.
        assert_eq!(g.signals(&answer).rebutted, 1);
    }

    #[test]
    fn replies_to_a_root_ask_are_not_citations_of_it() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(3, &ask, Kind::Note, json!({}));
        assert_eq!(g.signals(&ask).cites, 0);
        // A reply to an answer cites the answer.
        g.reply(3, &answer, Kind::Answer, json!({}));
        assert_eq!(g.signals(&answer).cites, 1);
        // The ask cited in refs from another thread counts.
        let other = g.root(4, Kind::Ask, "related?");
        g.reply_citing(5, &other, Kind::Answer, json!({}), &[&ask]);
        assert_eq!(g.signals(&ask).cites, 1);
        // A reply to a note, or to an ask that is not a thread root, cites it.
        let note = g.root(1, Kind::Note, "fyi");
        g.reply(3, &note, Kind::Note, json!({}));
        assert_eq!(g.signals(&note).cites, 1);
        let follow_up = g.reply(1, &answer, Kind::Ask, json!({}));
        g.reply(4, &follow_up, Kind::Answer, json!({}));
        assert_eq!(g.signals(&follow_up).cites, 1);
    }

    #[test]
    fn citations_never_come_from_the_authors_group_a_contradiction_or_a_react() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let other = g.root(6, Kind::Ask, "again?");
        // The author's successor key, and the node's other profile.
        g.rotate(2, 5);
        g.reply_citing(5, &other, Kind::Answer, json!({}), &[&answer]);
        g.reply_citing(3, &other, Kind::Answer, json!({}), &[&answer]);
        assert_eq!(g.signals_with(&[2, 3], &answer).cites, 0);
        // A react, even with refs, never cites.
        g.reply_citing(7, &other, Kind::React, json!({"value": 1}), &[&answer]);
        g.reply(7, &answer, Kind::React, json!({"value": 0}));
        assert_eq!(g.signals_with(&[2, 3], &answer).cites, 0);
        // The reasoning of a contradiction is not a citation: only 3's
        // counts, where 3 is any other key.
        g.reply(4, &answer, Kind::Note, json!({}));
        g.reply(4, &answer, Kind::React, json!({"value": -1}));
        let s = g.signals(&answer);
        assert_eq!((s.contradicts, s.cites), (1, 1));
    }

    #[test]
    fn only_the_askers_key_or_its_successor_accepts() {
        let mut g = Graph::new();
        // 1 is the human's profile, 3 its node's agent profile.
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(3, &answer, Kind::React, json!({"accept": true}));
        let s = g.signals_with(&[1, 3], &answer);
        assert_eq!((s.accepts, s.status), (0, Status::Unverified));
        // The asker's successor key accepts for it.
        g.rotate(1, 5);
        g.reply(5, &answer, Kind::React, json!({"accept": true}));
        assert_eq!(g.signals_with(&[1, 3], &answer).accepts, 1);
        // The node's agent's own answer accepted by its human: zero.
        let agents = g.reply(3, &ask, Kind::Answer, json!({}));
        g.reply(1, &agents, Kind::React, json!({"accept": true}));
        assert_eq!(g.signals_with(&[1, 3], &agents).accepts, 0);
    }

    // v1b: reputation.

    /// Fresh keys, beyond [`KNOWN`].
    const SYBILS: [u8; 5] = [101, 102, 103, 104, 105];

    #[test]
    fn a_sybil_clique_gets_no_trust_cannot_confirm_nor_flip_a_status() {
        let mut g = Graph::new();
        // 1 is this node; it accepted 2's answer, so 2 is trusted.
        let ask = g.root(1, Kind::Ask, "how?");
        let trusted = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(1, &trusted, Kind::React, json!({"accept": true}));
        // A clique of fresh keys confirming and using each other, with
        // posts of its own and plenty of signals.
        let mut theirs = Vec::new();
        for &s in &SYBILS {
            theirs.push(g.root(s, Kind::Note, "we know"));
        }
        for &s in &SYBILS {
            for post in theirs.clone() {
                g.confirm(s, &post);
                g.reply(s, &post, Kind::React, json!({"used": true}));
            }
        }
        let table = g.table(&[1]);
        for &s in &SYBILS {
            assert_eq!(table.weight(&id_of(s)), 0.0, "sybil {s} weighs nothing");
            assert_eq!(table.authors[&id_of(s)].trust, 0.0);
        }
        for post in &theirs {
            let s = g.signals_with(&[1], post);
            assert_eq!(s.status, Status::Unverified, "{s:?}");
            assert!(s.counts().is_empty(), "weightless signals are not counted");
        }
        // They cannot make anyone's post confirmed either.
        let other = g.reply(3, &ask, Kind::Answer, json!({}));
        for &s in &SYBILS {
            g.confirm(s, &other);
        }
        assert_eq!(g.signals_with(&[1], &other).status, Status::Unverified);

        // Nor contest the trusted author's confirmed post.
        g.confirm(4, &trusted);
        assert_eq!(g.signals_with(&[1], &trusted).status, Status::Confirmed);
        for &s in &SYBILS {
            g.contradict(s, &trusted, "wrong");
        }
        let s = g.signals_with(&[1], &trusted);
        assert_eq!((s.status, s.contradicts), (Status::Confirmed, 0));
        // A contradiction with weight (5's answer was used by this node)
        // stands; the clique rebutting it changes nothing.
        let fives = g.reply(5, &ask, Kind::Answer, json!({}));
        g.reply(1, &fives, Kind::React, json!({"used": true}));
        let no = g.contradict(5, &trusted, "fails on musl");
        assert_eq!(g.signals_with(&[1], &trusted).status, Status::Contested);
        for &s in &SYBILS {
            g.contradict(s, &no, "works");
        }
        let s = g.signals_with(&[1], &trusted);
        assert_eq!((s.status, s.rebutted), (Status::Contested, 0));
    }

    #[test]
    fn on_a_fresh_network_the_asker_and_the_nodes_circle_confirm() {
        let mut g = Graph::fresh();
        let ask = g.root(51, Kind::Ask, "how?");
        let answer = g.reply(52, &ask, Kind::Answer, json!({}));
        // Another fresh key using it: no weight yet.
        g.reply(53, &answer, Kind::React, json!({"used": true}));
        assert_eq!(g.signals(&answer).status, Status::Unverified);
        // The asker using it is its own call, like an accept.
        g.reply(51, &answer, Kind::React, json!({"used": true}));
        let s = g.signals(&answer);
        assert_eq!((s.status, s.uses), (Status::Confirmed, 1));
        // This node's own profile confirms with its seed trust.
        let other = g.reply(54, &ask, Kind::Answer, json!({}));
        g.confirm(60, &other);
        assert_eq!(g.signals(&other).status, Status::Unverified);
        assert_eq!(g.signals_with(&[60], &other).status, Status::Confirmed);
    }

    #[test]
    fn implicit_trust_gives_weight_to_who_the_node_accepted() {
        let mut g = Graph::fresh();
        // 60 is this node. 61 and 62 are fresh.
        let ask = g.root(60, Kind::Ask, "how?");
        let answer = g.reply(61, &ask, Kind::Answer, json!({}));
        let note = g.root(63, Kind::Note, "a finding");
        g.confirm(61, &note);
        g.confirm(62, &note);
        assert_eq!(g.signals_with(&[60], &note).status, Status::Unverified);
        assert_eq!(g.weight(&[60], 61), 0.0);
        // The node accepts 61's answer: 61's confirmations now weigh.
        g.reply(60, &answer, Kind::React, json!({"accept": true}));
        assert!(g.weight(&[60], 61) > 0.5);
        assert_eq!(g.weight(&[60], 62), 0.0);
        let s = g.signals_with(&[60], &note);
        assert_eq!((s.status, s.confirms), (Status::Confirmed, 1));
        // Trust passes on, damped: 61 confirmed 63's note.
        let w = g.weight(&[60], 63);
        assert!(w > 0.0 && w < g.weight(&[60], 61), "{w}");
    }

    #[test]
    fn declared_owners_and_vouch_clusters_count_once() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        // 5 and 6 declare one owner.
        g.root_with(5, Kind::Announce, "me", json!({"owner": "Acme"}));
        g.root_with(6, Kind::Announce, "me too", json!({"owner": "acme "}));
        g.confirm(5, &answer);
        g.confirm(6, &answer);
        assert_eq!(g.signals(&answer).confirms, 1);
        // And confirming each other counts zero.
        let fives = g.reply(5, &ask, Kind::Answer, json!({}));
        g.confirm(6, &fives);
        assert_eq!(g.signals(&fives).status, Status::Unverified);
        // 1 is this node and used 7's answer. 7 alone vouches for the new
        // keys 108 and 109, which so weigh a little: one group with 7.
        let sevens = g.reply(7, &ask, Kind::Answer, json!({}));
        g.reply(1, &sevens, Kind::React, json!({"used": true}));
        g.announce(108);
        g.announce(109);
        g.vouch(7, 108);
        g.vouch(7, 109);
        assert!(g.weight(&[1], 108) > 0.0);
        let other = g.reply(3, &ask, Kind::Answer, json!({}));
        g.confirm(108, &other);
        g.confirm(109, &other);
        g.confirm(7, &other);
        assert_eq!(g.signals_with(&[1], &other).confirms, 1);
        g.confirm(108, &sevens);
        assert_eq!(
            g.signals_with(&[1], &sevens).confirms,
            0,
            "within the group"
        );
        // Vouched for by a second, independent key: on its own again.
        g.vouch(10, 109);
        assert_eq!(g.signals_with(&[1], &other).confirms, 2);
        // A stranger's vouch never pulls established keys into its group.
        g.vouch(110, 12);
        g.vouch(110, 13);
        let table = g.table(&[1]);
        assert_ne!(table.group_of(&id_of(12)), table.group_of(&id_of(13)));
        assert_ne!(table.group_of(&id_of(12)), table.group_of(&id_of(110)));
        // A declared owner never silences a contradiction of the owner's
        // posts: it only takes positive weight away.
        g.root_with(11, Kind::Announce, "me", json!({"owner": "acme"}));
        g.contradict(11, &fives, "wrong");
        assert_eq!(g.signals(&fives).status, Status::Contested);
    }

    /// A post by 2, whom this node (1) trusts, confirmed with evidence and
    /// cited by 3, whom this node trusts too.
    fn trusted_and_confirmed(g: &mut Graph) -> Event {
        let ask = g.root(1, Kind::Ask, "how?");
        let post = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(1, &post, Kind::React, json!({"used": true}));
        let threes = g.reply(3, &ask, Kind::Answer, json!({}));
        g.reply(1, &threes, Kind::React, json!({"used": true}));
        g.confirm(3, &post);
        g.reply_citing(3, &ask, Kind::Note, json!({}), &[&post]);
        post
    }

    /// 101, grouped with 3, votes -1 then contradicts 2's post: the group's
    /// latest react. 101's signals count at 101's own weight, below what
    /// contests 2; 3's confirmation and citation still count at 3's.
    fn assert_no_weight_borrowed(g: &mut Graph, post: &Event) {
        let table = g.table(&[1]);
        assert_eq!(
            table.group_of(&id_of(3)),
            table.group_of(&id_of(101)),
            "grouped"
        );
        assert!(table.weight(&id_of(101)) < ATTACK_FACTOR * table.weight(&id_of(2)));
        g.reply(101, post, Kind::React, json!({"value": -1}));
        g.contradict(101, post, "wrong");
        let s = g.signals_with(&[1], post);
        assert_eq!(
            (s.status, s.confirms, s.cites, s.contradicts),
            (Status::Confirmed, 1, 1, 0),
            "{s:?}"
        );
        assert!(s.score > 0.0);
    }

    #[test]
    fn a_declared_owner_lends_no_weight_to_a_fresh_key() {
        let mut g = Graph::new();
        g.root_with(3, Kind::Announce, "me", json!({"owner": "acme"}));
        let post = trusted_and_confirmed(&mut g);
        g.root_with(101, Kind::Announce, "me too", json!({"owner": "acme"}));
        assert_eq!(g.weight(&[1], 101), 0.0);
        assert_no_weight_borrowed(&mut g, &post);
        assert_eq!(g.signals_with(&[1], &post).down, 0, "a vote of no weight");
        // Nor does 101's confirmation borrow 3's weight elsewhere.
        let ask = g.root(4, Kind::Ask, "else?");
        let other = g.reply(5, &ask, Kind::Answer, json!({}));
        g.confirm(101, &other);
        assert_eq!(g.signals_with(&[1], &other).status, Status::Unverified);
        assert_eq!(g.table(&[1]).score(&id_of(101)), 0.0);
    }

    #[test]
    fn a_vouch_cluster_lends_no_weight_to_a_fresh_key() {
        let mut g = Graph::new();
        let post = trusted_and_confirmed(&mut g);
        // 3 alone vouches for the new key 101: one group, and 101 weighs
        // only the share of trust 3's vouch passes on.
        g.announce(101);
        g.vouch(3, 101);
        assert!(g.weight(&[1], 101) > 0.0);
        assert_no_weight_borrowed(&mut g, &post);
    }

    #[test]
    fn a_contradiction_cycle_is_undecided_and_contested() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.confirm(6, &answer);
        let b = g.contradict(3, &answer, "fails on musl");
        let c = g.contradict(4, &b, "your toolchain is wrong");
        assert_eq!(g.signals(&answer).status, Status::Confirmed);
        // 3 answers 4: they contradict each other, nobody wins.
        g.contradict(3, &c, "it is not");
        let s = g.signals(&answer);
        assert_eq!(
            (s.status, s.contradicts, s.undecided, s.rebutted),
            (Status::Contested, 0, 1, 0),
            "{s:?}"
        );
        assert_eq!(
            s.why.as_deref(),
            Some(
                "contradicted by 1 key: fails on musl; undecided: contradictions answer each \
                 other in a cycle"
            )
        );
    }

    #[test]
    fn contradiction_chains_are_grounded_three_levels_deep() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let a = g.reply(2, &ask, Kind::Answer, json!({}));
        g.confirm(7, &a);
        // A contradicted by B: contested.
        let b = g.contradict(3, &a, "fails");
        assert_eq!(g.signals(&a).status, Status::Contested);
        // B contradicted by C (depth 2): B is out, A confirmed again.
        let c = g.contradict(4, &b, "wrong flags");
        let s = g.signals(&a);
        assert_eq!((s.status, s.rebutted), (Status::Confirmed, 1));
        assert_eq!(
            s.why.as_deref(),
            Some("the contradiction was rebutted with evidence")
        );
        // C contradicted by D (depth 3): C is out, B stands again.
        let d = g.contradict(5, &c, "flags are right");
        let s = g.signals(&a);
        assert_eq!((s.status, s.contradicts), (Status::Contested, 1));
        assert_eq!(s.why.as_deref(), Some("contradicted by 1 key: fails"));
        // D contradicted by E (depth 4): ignored.
        g.contradict(6, &d, "no they are not");
        assert_eq!(g.signals(&a).status, Status::Contested);
        // With a second contradiction rebutted, the why says so.
        let b2 = g.contradict(8, &a, "slow");
        g.contradict(9, &b2, "not here");
        let s = g.signals(&a);
        assert_eq!((s.contradicts, s.rebutted), (1, 1));
        assert_eq!(
            s.why.as_deref(),
            Some("contradicted by 1 key: fails; 1 rebuttal stands")
        );
    }

    #[test]
    fn a_weak_attackers_contradiction_is_ignored_against_a_strong_author() {
        let mut g = Graph::new();
        // 1 is this node and used 2's answer: 2 weighs ~1, 3 only its floor.
        let ask = g.root(1, Kind::Ask, "how?");
        let strong = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(1, &strong, Kind::React, json!({"used": true}));
        assert!(g.weight(&[1], 3) < ATTACK_FACTOR * g.weight(&[1], 2));
        g.contradict(3, &strong, "wrong");
        let s = g.signals_with(&[1], &strong);
        assert_eq!((s.status, s.contradicts), (Status::Confirmed, 0));
        // Between keys of the same standing, it counts.
        assert_eq!(g.signals(&strong).status, Status::Contested);
    }

    #[test]
    fn confirmations_go_stale_unless_renewed() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.confirm(3, &answer);
        g.later(359 * DAY);
        assert_eq!(g.signals(&answer).status, Status::Confirmed);
        // Two half-lives without a renewal: stale.
        g.later(2 * DAY);
        let s = g.signals(&answer);
        assert_eq!(s.status, Status::Stale);
        assert_eq!(
            s.why.as_deref(),
            Some("every confirmation decayed: the latest is 361 days old")
        );
        // A renewal restarts 3's clock.
        g.reply(3, &answer, Kind::React, json!({"still_valid": true}));
        assert_eq!(g.signals(&answer).status, Status::Confirmed);
        // Renewals within 30 days of the last one do not stack: the clock
        // stays at the first.
        g.later(20 * DAY);
        g.reply(3, &answer, Kind::React, json!({"still_valid": true}));
        g.later(9 * DAY);
        g.reply(3, &answer, Kind::React, json!({"still_valid": true}));
        g.later(361 * DAY - 29 * DAY);
        assert_eq!(g.signals(&answer).status, Status::Stale);
        // An accept decays the same way, and the asker renews it.
        let accepted = g.reply(4, &ask, Kind::Answer, json!({}));
        g.reply(1, &accepted, Kind::React, json!({"accept": true}));
        g.later(361 * DAY);
        assert_eq!(g.signals(&accepted).status, Status::Stale);
        g.reply(1, &accepted, Kind::React, json!({"still_valid": true}));
        assert_eq!(g.signals(&accepted).status, Status::Confirmed);
    }

    #[test]
    fn renewals_count_at_most_every_thirty_days() {
        assert_eq!(renewed(0, &[]), 0);
        let day = DAY;
        assert_eq!(renewed(0, &[10 * day, 20 * day, 29 * day]), 0);
        assert_eq!(renewed(0, &[10 * day, 30 * day, 40 * day]), 30 * day);
        assert_eq!(
            renewed(0, &[30 * day, 45 * day, 60 * day, 61 * day]),
            60 * day
        );
    }

    #[test]
    /// Votes decay with a 180-day half-life since 0.4.15 (ADR 0045 §6),
    /// like uses and confirmations.
    fn votes_decay_with_a_half_year_half_life() {
        let mut g = Graph::new();
        let note = g.root(2, Kind::Note, "a finding");
        g.reply(3, &note, Kind::React, json!({"value": 1}));
        let fresh = g.signals(&note);
        assert_eq!(fresh.up, 1);
        g.later(180 * DAY);
        let later = g.signals(&note);
        assert!(
            (later.up_weight / fresh.up_weight - 0.5).abs() < 0.01,
            "{} then {}",
            fresh.up_weight,
            later.up_weight
        );
    }

    /// ADR 0045 §6: the lower bound ranks 40/45 above 2/2, and a
    /// still-valid answer keeps a floor while an outdated one fades.
    #[test]
    fn the_lower_bound_ranks_and_valid_for_keeps_a_floor() {
        assert_eq!(wilson(0.0, 0.0), 0.0);
        assert!(wilson(40.0, 45.0) > wilson(2.0, 2.0));
        assert!((wilson(2.0, 2.0) - 0.342).abs() < 0.001);
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let timeless = g.reply(2, &ask, Kind::Answer, json!({"valid_for": "kotlin 2"}));
        let plain = g.reply(2, &ask, Kind::Answer, json!({}));
        for voter in 3..6 {
            g.reply(voter, &timeless, Kind::React, json!({"value": 1}));
            g.reply(voter, &plain, Kind::React, json!({"value": 1}));
        }
        g.later(3 * 365 * DAY);
        let (t, p) = (g.signals(&timeless), g.signals(&plain));
        // The floor keeps a quarter of each vote; without it, about 1/64.
        assert!(
            t.up_weight > 10.0 * p.up_weight,
            "{} vs {}",
            t.up_weight,
            p.up_weight
        );
        assert!(t.lower_bound() > p.lower_bound());
        // Two readers mark it outdated: stale, and no floor any more.
        g.reply(6, &timeless, Kind::React, json!({"outdated": true}));
        g.reply(7, &timeless, Kind::React, json!({"outdated": true}));
        let t = g.signals(&timeless);
        assert_eq!(t.outdated, 2);
        assert_eq!(t.status, Status::Stale);
        assert_eq!(t.why.as_deref(), Some("marked outdated by 2 readers"));
        assert!((t.up_weight - p.up_weight).abs() < 1e-9, "{}", t.up_weight);
    }

    /// ADR 0045 §2: a bare −1 is a fifth of a vote; one with a reason, a
    /// whole one.
    #[test]
    fn a_minus_one_needs_a_reason() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let a = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(3, &a, Kind::React, json!({"value": -1}));
        let bare = g.signals(&a).down_weight;
        let b = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(
            3,
            &b,
            Kind::React,
            json!({"value": -1, "evidence": "fails on v3"}),
        );
        let reasoned = g.signals(&b).down_weight;
        assert!(reasoned > 0.0);
        assert!(
            (bare / reasoned - BARE_DOWN).abs() < 0.01,
            "{bare} / {reasoned}"
        );
    }

    /// ADR 0045 §1: two readers flag an ask as a duplicate, or its asker
    /// does; one reader is not enough.
    #[test]
    fn duplicates_need_two_readers_or_the_asker() {
        let mut g = Graph::new();
        let first = g.root(1, Kind::Ask, "how do I X?");
        let again = g.root(2, Kind::Ask, "how to X?");
        let flag = json!({"duplicate_of": first.id});
        g.reply(3, &again, Kind::React, flag.clone());
        assert_eq!(g.signals(&again).duplicate_of, None);
        g.reply(4, &again, Kind::React, flag.clone());
        assert_eq!(
            g.signals(&again).duplicate_of.as_deref(),
            Some(first.id.as_str())
        );
        let third = g.root(5, Kind::Ask, "X, how?");
        g.reply(5, &third, Kind::React, flag);
        assert_eq!(
            g.signals(&third).duplicate_of.as_deref(),
            Some(first.id.as_str())
        );
    }

    #[test]
    fn the_floor_starts_after_seven_days_with_pow_below_the_age_cap() {
        let mut g = Graph::fresh();
        // 70 mines 12 bits on each post, far beyond the 0 required here.
        for i in 0..20 {
            g.ts += 1;
            let event = Draft {
                net: "public".into(),
                kind: Some(Kind::Note),
                title: Some(format!("note {i}")),
                text: format!("note {i}"),
                ..Draft::default()
            }
            .sign(&key(70), g.ts, 12)
            .unwrap();
            g.store.insert(&event, g.ts).unwrap();
        }
        g.root(71, Kind::Note, "no work");
        let pow = PowPolicy {
            min_pow_bits: Some(0),
            schedule: Vec::new(),
        };
        let floor_of = |g: &Graph, seed: u8| {
            let local = HashSet::new();
            let table = crate::reputation::build(&g.store, "public", &local, g.ts, &pow).unwrap();
            table.authors[&id_of(seed)].floor
        };
        assert_eq!(floor_of(&g, 70), 0.0, "CPU buys a fresh key nothing");
        g.later(10 * DAY);
        let worker = floor_of(&g, 70);
        let idle = floor_of(&g, 71);
        assert!(idle > 0.0 && worker > idle, "{worker} {idle}");
        assert!(worker - idle < crate::reputation::POW_BONUS_CAP);
        g.later(100 * DAY);
        assert!(floor_of(&g, 70) <= crate::reputation::FLOOR_CAP);
        assert_eq!(floor_of(&g, 71), crate::reputation::AGE_FLOOR_CAP);
    }

    #[test]
    fn scores_sum_weighted_signals_and_subtract_standing_contradictions() {
        let mut g = Graph::new();
        // 1 is this node.
        let ask = g.root(1, Kind::Ask, "how?");
        let good = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(1, &good, Kind::React, json!({"accept": true}));
        let bad = g.reply(3, &ask, Kind::Answer, json!({}));
        g.contradict(1, &bad, "fails");
        let table = g.table(&[1]);
        let score = |seed: u8| table.score(&id_of(seed));
        assert!(score(2) > 0.0, "accepted by a seed");
        assert!(score(3) < 0.0, "contradicted by a seed, standing");
        assert!(score(1) > 0.0, "its contradiction stands");
        assert_eq!(table.authors[&id_of(3)].counts.contradicted, 1);
        assert_eq!(table.authors[&id_of(1)].counts.stands, 1);
        assert_eq!(table.authors[&id_of(2)].counts.accepts, 1);
        // Grouping::is_low: below the threshold, never the node's own.
        let grouping = Grouping::new(table.local.clone()).with_table(table.clone());
        assert!(grouping.is_low("public", &id_of(3), 0.0));
        assert!(!grouping.is_low("public", &id_of(2), 0.0));
        assert!(!grouping.is_low("public", &id_of(4), 0.0));
        assert!(!grouping.is_low("public", &id_of(1), 1e9));
    }

    #[test]
    fn search_ranks_status_then_score_within_comparable_relevance() {
        let store = Store::open_in_memory().unwrap();
        // Other posts, so that both words are rare enough to weigh.
        for i in 0..8 {
            let filler = Draft {
                net: "public".into(),
                kind: Some(Kind::Note),
                title: Some(format!("python pip {i}")),
                text: format!("python pip {i}"),
                ..Draft::default()
            }
            .sign(&key(100 + i), 5, 0)
            .unwrap();
            store.insert(&filler, 5).unwrap();
        }
        let mut events = Vec::new();
        for (i, text) in ["rust musl", "rust musl", "rust musl", "rust", "rust musl"]
            .iter()
            .enumerate()
        {
            let event = Draft {
                net: "public".into(),
                kind: Some(Kind::Note),
                title: Some(text.to_string()),
                text: text.to_string(),
                ..Draft::default()
            }
            .sign(&key(i as u8 + 1), 10 + i as u64, 0)
            .unwrap();
            store.insert(&event, 10).unwrap();
            events.push(event);
        }
        let found = store
            .search_scored(Some("rust musl"), &Default::default(), 10)
            .unwrap();
        let order: Vec<&str> = found.iter().map(|(s, _)| s.event.id.as_str()).collect();
        // BM25, then newest first.
        assert_eq!(
            order,
            [
                &events[4].id,
                &events[2].id,
                &events[1].id,
                &events[0].id,
                &events[3].id
            ]
        );
        let with = |id: &str, status: Status, score: f64| {
            (
                id.to_string(),
                Signals {
                    status,
                    score,
                    ..Signals::default()
                },
            )
        };
        let signals: HashMap<String, Signals> = [
            with(&events[0].id, Status::Confirmed, 1.0),
            with(&events[1].id, Status::Confirmed, 3.0),
            with(&events[2].id, Status::Contested, 9.0),
            with(&events[3].id, Status::Confirmed, 9.0),
        ]
        .into_iter()
        .collect();
        let ranked: Vec<String> = rank(found, &signals)
            .into_iter()
            .map(|s| s.event.id.clone())
            .collect();
        // Confirmed rises (the better scored first) and contested sinks
        // within its band; the less relevant post stays below, confirmed
        // or not.
        assert_eq!(
            ranked,
            [
                events[1].id.clone(),
                events[0].id.clone(),
                events[4].id.clone(),
                events[2].id.clone(),
                events[3].id.clone()
            ]
        );
        // Without search text, the store's order stands.
        let newest = store.search_scored(None, &Default::default(), 10).unwrap();
        let before: Vec<String> = newest.iter().map(|(s, _)| s.event.id.clone()).collect();
        let after: Vec<String> = rank(newest, &signals)
            .into_iter()
            .map(|s| s.event.id.clone())
            .collect();
        assert_eq!(before, after);
    }

    /// How long a reputation table takes to build, and a search's
    /// signals take with the table cached, on ~50k events. A bench, not
    /// a check: `cargo test --release -p wats --lib -- --ignored
    /// reputation_at_50k --nocapture`.
    #[test]
    #[ignore]
    fn reputation_at_50k_events() {
        let mut g = Graph::new();
        let mut asks = Vec::new();
        let mut answers = Vec::new();
        let seed = |i: usize| (i % 200) as u8 + 1;
        for i in 0..7_000 {
            let ask = g.root(seed(i), Kind::Ask, &format!("rust musl {i}"));
            for j in 0..3 {
                let answer = g.reply(seed(i + j + 1), &ask, Kind::Answer, json!({}));
                answers.push(answer);
            }
            asks.push(ask);
        }
        for (i, answer) in answers.clone().iter().enumerate() {
            match i % 6 {
                0 => {
                    g.confirm(seed(i + 7), answer);
                }
                1 => {
                    g.reply(seed(i + 9), answer, Kind::React, json!({"used": true}));
                }
                2 => {
                    g.contradict(seed(i + 11), answer, "fails");
                }
                3 => {
                    g.reply(seed(i + 13), answer, Kind::React, json!({"value": 1}));
                }
                _ => {}
            }
            if i % 3 == 0 {
                let asker = asks[i / 3].author.clone();
                let asker = (1..=255u8).find(|s| id_of(*s) == asker).unwrap();
                g.reply(asker, answer, Kind::React, json!({"accept": true}));
            }
        }
        let events = g.store.count().unwrap();
        // How long each query of a build holds the store's lock (inserts
        // wait meanwhile).
        let timed = |name: &str, run: &dyn Fn()| {
            let start = std::time::Instant::now();
            run();
            eprintln!("{name}: {:?}", start.elapsed());
        };
        timed("author_events", &|| drop(g.store.author_events("public")));
        timed("net_signal_rows", &|| {
            drop(g.store.net_signal_rows("public", 200_000))
        });
        timed("accept_reacts_in", &|| {
            drop(g.store.accept_reacts_in("public", 200_000))
        });
        timed("vouches", &|| drop(g.store.vouches("public")));
        let start = std::time::Instant::now();
        let table = g.table(&[1, 2]);
        let built = start.elapsed();
        let grouping = Grouping::new(table.local.clone()).with_table(table);
        let found = g
            .store
            .search_scored(Some("rust musl"), &Default::default(), 60)
            .unwrap();
        let shown: Vec<&Event> = answers.iter().rev().take(60).collect();
        let start = std::time::Instant::now();
        let runs = 20;
        for _ in 0..runs {
            compute_at(&g.store, &grouping, &shown, g.ts).unwrap();
        }
        let per_search = start.elapsed() / runs;
        eprintln!(
            "{events} events, {} found: table built in {built:?}, signals of 60 posts in \
             {per_search:?} per search",
            found.len()
        );
    }

    #[test]
    fn statuses_parse_and_order() {
        for status in Status::ALL {
            assert_eq!(status.as_str().parse::<Status>(), Ok(status));
            assert_eq!(json!(status), json!(status.as_str()));
        }
        assert!("great".parse::<Status>().is_err());
        assert!(Status::Confirmed.rank() > Status::Stale.rank());
        assert!(Status::Stale.rank() > Status::Unverified.rank());
        assert!(Status::Unverified.rank() > Status::Contested.rank());
    }
}
