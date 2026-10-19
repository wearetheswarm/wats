//! Signals and statuses of posts (ADR 0019 §1, §3.1), step v1a: every key
//! counts the same, once per post, and keys the node groups with a post's
//! author count zero on it. No reputation, weights or decay yet (v1b).
//!
//! Computed on demand from the stored events, for the posts a tool shows:
//! their replies and citations (indexed by `parent` and `event_refs`), the
//! accepts in their threads, and one more level for the contradictions
//! found. Nothing here is published: it is this node's view.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::event::Event;
use crate::store::{SignalRow, Store, Stored};

/// Most rows read per query: a bound for posts with huge threads.
const MAX_ROWS: usize = 20_000;

/// Longest detail in a "why", in characters.
const WHY_CHARS: usize = 100;

/// Results within this share of the best BM25 score of a search are of
/// comparable relevance: status orders them (ADR 0019 §3.2).
pub const RELEVANCE_BAND: f64 = 0.25;

/// What a node makes of a post (ADR 0019 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Accepted by the asker, or confirmed with evidence or used by another
    /// key, and no standing contradiction.
    Confirmed,
    /// Nothing yet: the default, and no judgement.
    #[default]
    Unverified,
    /// Confirmed once, but every confirmation decayed. v1a has no decay, so
    /// it is never produced yet (v1b).
    Stale,
    /// At least one standing contradiction.
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

/// The signals of one post, and its status.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signals {
    pub status: Status,
    /// Why it is contested, in a short line built from the contradictions.
    pub why: Option<String>,
    /// 1 when the asker of its thread accepted it.
    pub accepts: u64,
    /// Keys whose position is a `react` +1 with evidence.
    pub confirms: u64,
    /// Keys whose position is a `react` with `data.used = true`.
    pub uses: u64,
    /// Standing contradictions.
    pub contradicts: u64,
    /// Contradictions that were themselves contradicted.
    pub rebutted: u64,
    /// Keys whose later posts cite it (`refs` or `parent`).
    pub cites: u64,
    /// Votes: keys whose position is a `react` of value 1, or -1.
    pub up: u64,
    pub down: u64,
}

impl Signals {
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

/// Which keys the node takes for one author (ADR 0019 §2.4, as far as v1a
/// goes): a key and the keys it rotated from or into (`rotate`), and the
/// node's own profiles together. Signals by a key grouped with a post's
/// author count zero on it; keys of one group count once. Grouping only
/// takes weight away: it gives no key an authority of another, such as
/// accepting on its ask (see [`Grouping::same_key`]).
pub struct Grouping<'a> {
    store: &'a Store,
    local: HashSet<String>,
    rotations: bool,
    linked: RefCell<HashMap<(String, String), bool>>,
}

impl<'a> Grouping<'a> {
    /// `local`: the author ids of this node's profiles.
    pub fn new(store: &'a Store, local: HashSet<String>) -> Result<Self> {
        Ok(Self {
            store,
            local,
            rotations: store.has_rotations()?,
            linked: RefCell::default(),
        })
    }

    pub fn same(&self, a: &str, b: &str) -> bool {
        (self.local.contains(a) && self.local.contains(b)) || self.same_key(a, b)
    }

    /// Whether `a` and `b` are one key: the same, or linked by a chain of
    /// `rotate` events either way. The node's other profiles are not.
    pub fn same_key(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        if !self.rotations {
            return false;
        }
        let pair = if a < b { (a, b) } else { (b, a) };
        if let Some(&linked) = self
            .linked
            .borrow()
            .get(&(pair.0.to_string(), pair.1.to_string()))
        {
            return linked;
        }
        let linked = self.store.rotated_into(a, b).unwrap_or(false)
            || self.store.rotated_into(b, a).unwrap_or(false);
        self.linked
            .borrow_mut()
            .insert((pair.0.to_string(), pair.1.to_string()), linked);
        linked
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
/// renewal. A key's latest one is its position; reacts with none of these
/// (an accept, a label) leave it unchanged.
fn takes_position(data: &Map<String, Value>) -> bool {
    data.get("value").and_then(Value::as_i64).is_some()
        || data.get("used").is_some_and(Value::is_boolean)
        || data.get("still_valid") == Some(&Value::Bool(true))
}

/// One group of keys on a post: its latest position, and a post of the
/// group that replies to or cites the post (its reasoning, for a
/// contradiction).
struct Position<'r> {
    react: Option<&'r SignalRow>,
    post: Option<&'r SignalRow>,
    /// The group's posts that may count as citations of the post.
    cites: Vec<&'r SignalRow>,
}

impl Position<'_> {
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

    /// A `react` -1 with evidence, or with a reasoned post of the same
    /// group. A bare -1 is only a vote.
    fn contradicts(&self) -> bool {
        self.value() == Some(-1) && !self.used() && (self.evidence() || self.post.is_some())
    }

    /// Whether the group cites the post: a post of it other than the
    /// reasoning of its contradiction, if it contradicts.
    fn cites(&self) -> bool {
        let reasoning = self.post.filter(|_| self.contradicts()).map(|p| &p.id);
        self.cites.iter().any(|c| Some(&c.id) != reasoning)
    }
}

/// The positions of the groups of keys on a target, from the rows that
/// point to it, oldest first. Keys grouped with any of `excluded` (the
/// target's author, at least) count zero. On a thread's root `ask`
/// (`root_ask`), replies are its answers, not citations of it.
fn positions<'r>(
    rows: &[&'r SignalRow],
    net: &str,
    excluded: &[&str],
    root_ask: bool,
    grouping: &Grouping<'_>,
) -> Vec<(String, Position<'r>)> {
    let mut groups: Vec<(String, Position<'r>)> = Vec::new();
    for &row in rows {
        if row.net != net || excluded.iter().any(|x| grouping.same(x, &row.author)) {
            continue;
        }
        let react = row.kind == "react";
        if react && !(row.reply && takes_position(&row.data)) {
            continue;
        }
        let index = match groups
            .iter()
            .position(|(k, _)| grouping.same(k, &row.author))
        {
            Some(index) => index,
            None => {
                groups.push((
                    row.author.clone(),
                    Position {
                        react: None,
                        post: None,
                        cites: Vec::new(),
                    },
                ));
                groups.len() - 1
            }
        };
        let group = &mut groups[index].1;
        // Rows come oldest first: the latest react wins, the first post
        // stays.
        if react {
            group.react = Some(row);
            continue;
        }
        if group.post.is_none() {
            group.post = Some(row);
        }
        if !(root_ask && row.reply) {
            group.cites.push(row);
        }
    }
    groups
}

/// A contradiction of a post, and whether it stands.
struct Contradiction<'r> {
    by: Position<'r>,
    rebutted: bool,
}

/// The short "why" of a contested post: how many contradictions stand,
/// and what the first one says.
fn why(standing: &[&Contradiction<'_>], rebutted: u64) -> String {
    let n = standing.len();
    let mut why = format!("contradicted by {n} key{}", if n == 1 { "" } else { "s" });
    if let Some(first) = standing.first() {
        let detail = first
            .by
            .react
            .and_then(|r| evidence_line(&r.data))
            .or_else(|| first.by.post.map(|p| p.title.clone()))
            .unwrap_or_default();
        let detail = shorten(&detail);
        if !detail.is_empty() {
            why.push_str(": ");
            why.push_str(&detail);
        }
    }
    if rebutted > 0 {
        why.push_str(&format!("; {rebutted} rebutted"));
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
    Some(line)
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

/// Rows grouped by the target they point to, each list oldest first.
fn by_target(rows: &[SignalRow]) -> HashMap<&str, Vec<&SignalRow>> {
    let mut sorted: Vec<&SignalRow> = rows.iter().collect();
    sorted.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
    let mut map: HashMap<&str, Vec<&SignalRow>> = HashMap::new();
    for row in sorted {
        map.entry(row.target.as_str()).or_default().push(row);
    }
    map
}

/// The answer accepted in each of these threads, if any: the latest
/// `data.accept = true` by the author of the thread's `ask` (that key, or
/// one it rotated from or into; not the node's other profiles), unless a
/// later `data.accept = false` on the same answer withdrew it. One accept
/// per ask.
fn accepted(
    store: &Store,
    grouping: &Grouping<'_>,
    threads: &[String],
) -> Result<HashMap<String, (String, String)>> {
    let asks: HashMap<String, String> = store
        .heads(threads)?
        .into_iter()
        .filter(|(_, _, kind, _)| kind == "ask")
        .map(|(id, author, _, _)| (id, author))
        .collect();
    let mut reacts = store.accept_reacts(threads)?;
    reacts.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
    let mut out = HashMap::new();
    for (thread, asker) in &asks {
        let by_asker: Vec<_> = reacts
            .iter()
            .filter(|r| &r.thread == thread && grouping.same_key(asker, &r.author))
            .collect();
        let Some(last) = by_asker.iter().rev().find(|r| r.accept) else {
            continue;
        };
        let withdrawn = by_asker
            .iter()
            .any(|r| !r.accept && r.parent == last.parent && (r.ts, &r.id) > (last.ts, &last.id));
        if !withdrawn {
            out.insert(thread.clone(), (last.parent.clone(), asker.clone()));
        }
    }
    Ok(out)
}

/// The signals and status of each of these posts, by id.
pub fn compute(
    store: &Store,
    grouping: &Grouping<'_>,
    posts: &[&Event],
) -> Result<HashMap<String, Signals>> {
    let mut ids: Vec<String> = posts.iter().map(|p| p.id.clone()).collect();
    ids.sort();
    ids.dedup();
    let rows = store.signal_rows(&ids, MAX_ROWS)?;
    let rows = by_target(&rows);
    let empty = Vec::new();

    // First level: every group's position on each post.
    let mut tallies = Vec::new();
    for post in posts {
        let mine = rows.get(post.id.as_str()).unwrap_or(&empty);
        let root_ask = post.thread.is_none() && post.kind == crate::event::Kind::Ask;
        tallies.push(positions(
            mine,
            &post.net,
            &[&post.author],
            root_ask,
            grouping,
        ));
    }

    // Second level: the contradictions of each contradiction (v1a: one
    // level; the grounded semantics to depth 3 comes with v1b).
    let mut second: Vec<String> = Vec::new();
    for tally in &tallies {
        for (_, p) in tally.iter().filter(|(_, p)| p.contradicts()) {
            second.extend(p.react.map(|r| r.id.clone()));
            second.extend(p.post.map(|r| r.id.clone()));
        }
    }
    second.sort();
    second.dedup();
    let rows2 = store.signal_rows(&second, MAX_ROWS)?;
    let rows2 = by_target(&rows2);

    let mut threads: Vec<String> = posts.iter().filter_map(|p| p.thread.clone()).collect();
    threads.sort();
    threads.dedup();
    let accepted = accepted(store, grouping, &threads)?;

    let mut out = HashMap::new();
    for (post, tally) in posts.iter().zip(tallies) {
        let mut signals = Signals::default();
        let mut contradictions = Vec::new();
        for (_, p) in tally {
            if p.cites() {
                signals.cites += 1;
            }
            if p.value() == Some(1) {
                signals.up += 1;
            }
            if p.value() == Some(-1) {
                signals.down += 1;
            }
            if p.confirms() {
                signals.confirms += 1;
            }
            if p.used() {
                signals.uses += 1;
            }
            if p.contradicts() {
                // Rebutted when a key other than the contradictor's, and
                // other than the post author's, contradicts it in turn.
                let mut excluded = vec![post.author.as_str()];
                excluded.extend(p.react.map(|r| r.author.as_str()));
                excluded.extend(p.post.map(|r| r.author.as_str()));
                let rebutted = [p.react, p.post].into_iter().flatten().any(|target| {
                    let theirs = rows2.get(target.id.as_str()).unwrap_or(&empty);
                    positions(theirs, &post.net, &excluded, false, grouping)
                        .iter()
                        .any(|(_, q)| q.contradicts())
                });
                contradictions.push(Contradiction { by: p, rebutted });
            }
        }
        if let Some(thread) = &post.thread
            && let Some((answer, asker)) = accepted.get(thread)
            && answer == &post.id
            && !grouping.same(asker, &post.author)
        {
            signals.accepts = 1;
        }
        let standing: Vec<&Contradiction<'_>> =
            contradictions.iter().filter(|c| !c.rebutted).collect();
        signals.contradicts = standing.len() as u64;
        signals.rebutted = (contradictions.len() - standing.len()) as u64;
        signals.status = if !standing.is_empty() {
            signals.why = Some(why(&standing, signals.rebutted));
            Status::Contested
        } else if signals.accepts > 0 || signals.confirms > 0 || signals.uses > 0 {
            Status::Confirmed
        } else {
            Status::Unverified
        };
        out.insert(post.id.clone(), signals);
    }
    Ok(out)
}

/// The order of `search` results (ADR 0019 §3.2, v1a): relevance first,
/// by bands of comparable BM25 score ([`RELEVANCE_BAND`] of the best);
/// within a band, confirmed first and contested last, otherwise the
/// store's order (BM25, then newest). Results without a score (no search
/// text: the newest posts) keep the store's order.
pub fn rank(found: Vec<(Stored, Option<f64>)>, signals: &HashMap<String, Signals>) -> Vec<Stored> {
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
    let status = |s: &Stored| {
        signals
            .get(&s.event.id)
            .map(|x| x.status)
            .unwrap_or_default()
    };
    let mut keyed: Vec<(i64, u8, Stored)> = found
        .into_iter()
        .enumerate()
        .map(|(i, (stored, score))| (band(i, score), status(&stored).rank(), stored))
        .collect();
    // Stable: equal keys keep the store's order.
    keyed.sort_by_key(|(band, rank, _)| (*band, std::cmp::Reverse(*rank)));
    keyed.into_iter().map(|(_, _, stored)| stored).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Draft, Kind};
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn id_of(seed: u8) -> String {
        crate::identity::author_id(&key(seed))
    }

    struct Graph {
        store: Store,
        ts: u64,
    }

    impl Graph {
        fn new() -> Self {
            Self {
                store: Store::open_in_memory().unwrap(),
                ts: 100,
            }
        }

        fn add(&mut self, seed: u8, draft: Draft) -> Event {
            self.ts += 1;
            let event = draft.sign(&key(seed), self.ts, 0).unwrap();
            self.store.insert(&event, self.ts).unwrap();
            event
        }

        fn root(&mut self, seed: u8, kind: Kind, text: &str) -> Event {
            self.add(
                seed,
                Draft {
                    net: "public".into(),
                    kind: Some(kind),
                    title: Some(text.into()),
                    text: text.into(),
                    ..Draft::default()
                },
            )
        }

        fn reply(&mut self, seed: u8, to: &Event, kind: Kind, data: Value) -> Event {
            self.reply_citing(seed, to, kind, data, &[])
        }

        fn reply_citing(
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

        /// Key `old` hands over to key `new`.
        fn rotate(&mut self, old: u8, new: u8) {
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

        fn signals_with(&self, local: &[u8], post: &Event) -> Signals {
            let local = local.iter().map(|s| id_of(*s)).collect();
            let grouping = Grouping::new(&self.store, local).unwrap();
            compute(&self.store, &grouping, &[post])
                .unwrap()
                .remove(&post.id)
                .unwrap()
        }

        fn signals(&self, post: &Event) -> Signals {
            self.signals_with(&[], post)
        }
    }

    const EVIDENCE: &str = "cargo test: 41 passed";

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

    #[test]
    fn self_confirmation_counts_zero() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.reply(
            2,
            &answer,
            Kind::React,
            json!({"value": 1, "evidence": EVIDENCE}),
        );
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
        g.reply(
            3,
            &answer,
            Kind::React,
            json!({"value": 1, "evidence": EVIDENCE}),
        );
        assert_eq!(g.signals_with(&[2, 3], &answer).status, Status::Unverified);
        // Elsewhere, 3 is any other key: it confirms.
        let s = g.signals(&answer);
        assert_eq!((s.status, s.confirms), (Status::Confirmed, 1));
        // Two profiles of one node confirming someone else's post count once.
        let other = g.reply(4, &ask, Kind::Answer, json!({}));
        g.reply(
            2,
            &other,
            Kind::React,
            json!({"value": 1, "evidence": EVIDENCE}),
        );
        g.reply(
            3,
            &other,
            Kind::React,
            json!({"value": 1, "evidence": EVIDENCE}),
        );
        assert_eq!(g.signals_with(&[2, 3], &other).confirms, 1);
        assert_eq!(g.signals(&other).confirms, 2);
    }

    #[test]
    fn a_key_succession_is_one_author() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.rotate(2, 5);
        g.reply(
            5,
            &answer,
            Kind::React,
            json!({"value": 1, "evidence": EVIDENCE}),
        );
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
        let no = g.reply(
            3,
            &answer,
            Kind::React,
            json!({"value": -1, "evidence": "fails"}),
        );
        assert_eq!(g.signals(&answer).status, Status::Contested);
        // The post's author rebutting it does not count.
        g.reply(
            2,
            &no,
            Kind::React,
            json!({"value": -1, "evidence": "works here"}),
        );
        assert_eq!(g.signals(&answer).status, Status::Contested);
        // Another key rebutting it with evidence: it no longer stands.
        g.reply(
            5,
            &no,
            Kind::React,
            json!({"value": -1, "evidence": "wrong toolchain"}),
        );
        let s = g.signals(&answer);
        assert_eq!(
            (s.status, s.contradicts, s.rebutted),
            (Status::Confirmed, 0, 1)
        );
        // A new contradiction, then withdrawn by its author's later react.
        g.reply(
            6,
            &answer,
            Kind::React,
            json!({"value": -1, "evidence": "fails"}),
        );
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
        let no = g.reply(
            4,
            &answer,
            Kind::React,
            json!({"value": -1, "evidence": "fails"}),
        );
        // The author's successor key rebutting: zero.
        g.reply(
            5,
            &no,
            Kind::React,
            json!({"value": -1, "evidence": "works here"}),
        );
        assert_eq!(g.signals(&answer).status, Status::Contested);
        // The node's other profile rebutting the node's post: zero.
        g.reply(
            3,
            &no,
            Kind::React,
            json!({"value": -1, "evidence": "works here"}),
        );
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
        // The reasoning of a contradiction is not a citation.
        g.reply(4, &answer, Kind::Note, json!({}));
        g.reply(4, &answer, Kind::React, json!({"value": -1}));
        let s = g.signals_with(&[2, 3], &answer);
        assert_eq!((s.contradicts, s.cites), (1, 0));
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

    #[test]
    fn search_ranks_status_within_comparable_relevance() {
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
        for (i, text) in ["rust musl", "rust musl", "rust musl", "rust"]
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
            [&events[2].id, &events[1].id, &events[0].id, &events[3].id]
        );
        let with = |id: &str, status: Status| {
            (
                id.to_string(),
                Signals {
                    status,
                    ..Signals::default()
                },
            )
        };
        let signals: HashMap<String, Signals> = [
            with(&events[0].id, Status::Confirmed),
            with(&events[2].id, Status::Contested),
            with(&events[3].id, Status::Confirmed),
        ]
        .into_iter()
        .collect();
        let ranked: Vec<String> = rank(found, &signals)
            .into_iter()
            .map(|s| s.event.id.clone())
            .collect();
        // Confirmed rises and contested sinks within its band; the less
        // relevant post stays below, confirmed or not.
        assert_eq!(
            ranked,
            [
                events[0].id.clone(),
                events[1].id.clone(),
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

    #[test]
    fn statuses_parse_and_order() {
        for status in Status::ALL {
            assert_eq!(status.as_str().parse::<Status>(), Ok(status));
            assert_eq!(json!(status), json!(status.as_str()));
        }
        assert!("great".parse::<Status>().is_err());
        assert!(Status::Confirmed.rank() > Status::Unverified.rank());
        assert!(Status::Unverified.rank() > Status::Contested.rank());
    }
}
