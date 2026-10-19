//! Knowledge entries (ADR 0019 §5), step 3: digests, and threads as the
//! fallback.
//!
//! A digest is an ordinary signed v1 event of the extension kind
//! [`DIGEST_KIND`] (ADR 0010 A7): consolidated knowledge, its sources in
//! `refs`, optionally `data.supersedes` (the id of an older digest it
//! replaces). Older nodes relay it as an opaque extension kind and show it
//! as a note. Its signals are those of any post ([`crate::signals`]).
//!
//! A **knowledge entry** is this node's view, computed on demand and never
//! published (node policy, written down in ADR 0019, "Implementation notes
//! (digests)"):
//!
//! - a digest with at least one ref, `confirmed`, and either backed by two
//!   independent groups of keys that weigh (a fresh confirmation with
//!   evidence or use each), or accepted by the asker of the ask it replies
//!   in; unless a newer digest that is promoted too, by the same key (or
//!   one it rotated from or into), supersedes it;
//! - otherwise, for a thread no such digest covers, its canonical answer:
//!   the best-scored `answer` of the thread that is `confirmed` and either
//!   accepted by the asker plus backed by one group independent of the
//!   asker, or backed with evidence by two groups.
//!
//! A digest covers the thread it sits in and the threads of the posts in
//! its `refs` (in its network). Nothing here is published: the node never
//! writes a digest of its own.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::Value;

use crate::event::{Event, Kind};
use crate::signals::{Grouping, Signals, Status};
use crate::store::{Store, Stored};

/// The extension kind of a digest.
pub const DIGEST_KIND: &str = "x.wearetheswarm.digest";

/// Most candidates (answers and digests) read for the threads asked for.
const MAX_CANDIDATES: usize = 2_000;
/// Most digests read that supersede the candidates.
const MAX_SUPERSEDING: usize = 500;

pub fn is_digest(kind: &Kind) -> bool {
    kind.as_str() == DIGEST_KIND
}

/// The older digest a digest replaces: its `data.supersedes`, when a
/// string.
pub fn supersedes(event: &Event) -> Option<&str> {
    event.data.get("supersedes").and_then(Value::as_str)
}

/// Whether a digest qualifies as an entry (§5.2), supersession aside: it
/// names its sources, and it is `confirmed`, backed by two independent
/// groups or accepted by the asker.
pub fn promotes_digest(event: &Event, s: &Signals) -> bool {
    !event.refs.is_empty() && s.status == Status::Confirmed && (s.backers.len() >= 2 || s.accepted)
}

/// Whether an answer makes its thread an entry (§5.2): `confirmed`, and
/// accepted by the asker plus backed by a group independent of the asker,
/// or backed with evidence by two groups.
pub fn promotes_answer(s: &Signals) -> bool {
    s.status == Status::Confirmed
        && ((s.accepted && s.backers.iter().any(|b| !b.asker))
            || s.backers.iter().filter(|b| b.evidence).count() >= 2)
}

/// One knowledge entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The digest, or the thread's canonical answer.
    pub post: Stored,
    /// What it rests on: a digest's `refs`; for a canonical answer, the
    /// root of its thread.
    pub sources: Vec<String>,
}

impl Entry {
    pub fn id(&self) -> &str {
        &self.post.event.id
    }
}

/// The knowledge entries of some threads, as this node sees them.
#[derive(Debug, Default)]
pub struct Knowledge {
    /// The entry of each thread that has one, by thread root.
    pub threads: HashMap<String, Entry>,
    /// The digests that are entries, by id.
    pub digests: HashMap<String, Entry>,
    /// The signals of every post looked at.
    pub signals: HashMap<String, Signals>,
}

impl Knowledge {
    /// The entry a post leads to: itself when it is a digest that is an
    /// entry, otherwise its thread's.
    pub fn of(&self, event: &Event) -> Option<&Entry> {
        self.digests
            .get(&event.id)
            .or_else(|| self.threads.get(event.thread_root()))
    }

    /// The entry this post is, if it is one itself.
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.digests
            .get(id)
            .or_else(|| self.threads.values().find(|e| e.id() == id))
    }

    /// The ids of every entry.
    pub fn ids(&self) -> HashSet<String> {
        self.digests
            .keys()
            .cloned()
            .chain(self.threads.values().map(|e| e.id().to_string()))
            .collect()
    }
}

/// The better of two entries for a thread: the higher score, then the
/// newer, then the smaller id.
fn better(a: &Stored, sa: &Signals, b: &Stored, sb: &Signals) -> bool {
    sa.score
        .total_cmp(&sb.score)
        .then(a.event.ts.cmp(&b.event.ts))
        .then(b.event.id.cmp(&a.event.id))
        .is_gt()
}

/// The knowledge entries of these threads (their roots). `known`: signals
/// already computed, reused.
pub fn compute(
    store: &Store,
    grouping: &Grouping,
    threads: &[String],
    known: &HashMap<String, Signals>,
) -> Result<Knowledge> {
    compute_at(store, grouping, threads, known, crate::node::now_secs())
}

/// [`compute`] at time `now`.
pub fn compute_at(
    store: &Store,
    grouping: &Grouping,
    threads: &[String],
    known: &HashMap<String, Signals>,
    now: u64,
) -> Result<Knowledge> {
    let mut threads: Vec<String> = threads.to_vec();
    threads.sort();
    threads.dedup();
    let candidates = store.knowledge_candidates(&threads, MAX_CANDIDATES)?;
    let digest_ids: Vec<String> = candidates
        .iter()
        .filter(|c| is_digest(&c.event.kind))
        .map(|c| c.event.id.clone())
        .collect();
    let mut posts: HashMap<String, Stored> = candidates
        .into_iter()
        .map(|c| (c.event.id.clone(), c))
        .collect();
    for newer in store.superseding(&digest_ids, MAX_SUPERSEDING)? {
        posts.entry(newer.event.id.clone()).or_insert(newer);
    }

    // Signals, those already known reused.
    let missing: Vec<&Event> = posts
        .values()
        .filter(|p| !known.contains_key(&p.event.id))
        .map(|p| &*p.event)
        .collect();
    let mut signals = crate::signals::compute_at(store, grouping, &missing, now)?;
    for id in posts.keys() {
        if let Some(s) = known.get(id) {
            signals.insert(id.clone(), s.clone());
        }
    }
    let none = Signals::default();
    let signals_of = |id: &str| signals.get(id).unwrap_or(&none);

    // Promoted digests, then those a newer promoted one supersedes.
    let promoted: Vec<&Stored> = posts
        .values()
        .filter(|p| is_digest(&p.event.kind) && promotes_digest(&p.event, signals_of(&p.event.id)))
        .collect();
    let superseded: HashSet<&str> = promoted
        .iter()
        .filter_map(|newer| {
            let older = posts.get(supersedes(&newer.event)?)?;
            let (n, o) = (&newer.event, &older.event);
            let replaces = is_digest(&o.kind)
                && n.net == o.net
                && (n.ts, &n.id) > (o.ts, &o.id)
                && grouping.same_key(&n.net, &n.author, &o.author);
            replaces.then_some(o.id.as_str())
        })
        .collect();
    let mut out = Knowledge::default();
    for digest in promoted {
        if !superseded.contains(digest.event.id.as_str()) {
            let entry = Entry {
                post: digest.clone(),
                sources: digest.event.refs.clone(),
            };
            out.digests.insert(digest.event.id.clone(), entry);
        }
    }

    // What each digest entry covers: its own thread, and the threads of
    // its refs in its network.
    let refs: Vec<String> = out
        .digests
        .values()
        .flat_map(|e| e.post.event.refs.iter().cloned())
        .collect();
    let roots: HashMap<String, (String, String)> = store
        .roots(&refs)?
        .into_iter()
        .map(|(id, root, net)| (id, (root, net)))
        .collect();
    let wanted: HashSet<&str> = threads.iter().map(String::as_str).collect();
    let mut by_thread: HashMap<String, &Entry> = HashMap::new();
    for entry in out.digests.values() {
        let event = &entry.post.event;
        let mut covered = vec![event.thread_root().to_string()];
        for r in &event.refs {
            if let Some((root, net)) = roots.get(r)
                && *net == event.net
            {
                covered.push(root.clone());
            }
        }
        for thread in covered {
            if !wanted.contains(thread.as_str()) {
                continue;
            }
            let replace = by_thread.get(&thread).is_none_or(|best| {
                better(
                    &entry.post,
                    signals_of(entry.id()),
                    &best.post,
                    signals_of(best.id()),
                )
            });
            if replace {
                by_thread.insert(thread, entry);
            }
        }
    }
    let mut threads_out: HashMap<String, Entry> = by_thread
        .into_iter()
        .map(|(thread, entry)| (thread, entry.clone()))
        .collect();

    // The fallback: the canonical answer of each thread no digest covers.
    let mut answers: HashMap<&str, &Stored> = HashMap::new();
    for post in posts.values() {
        let event = &post.event;
        let Some(thread) = event.thread.as_deref() else {
            continue;
        };
        if event.kind != Kind::Answer
            || !wanted.contains(thread)
            || threads_out.contains_key(thread)
            || !promotes_answer(signals_of(&event.id))
        {
            continue;
        }
        let replace = answers.get(thread).is_none_or(|best| {
            better(
                post,
                signals_of(&event.id),
                best,
                signals_of(&best.event.id),
            )
        });
        if replace {
            answers.insert(thread, post);
        }
    }
    for (thread, answer) in answers {
        threads_out.insert(
            thread.to_string(),
            Entry {
                post: answer.clone(),
                sources: vec![thread.to_string()],
            },
        );
    }
    out.threads = threads_out;
    out.signals = signals;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use crate::reputation::DAY;
    use crate::signals::tests::Graph;
    use serde_json::json;

    impl Graph {
        /// A digest by `seed` citing `refs`: a reply to `to`, or a thread
        /// of its own.
        fn digest(&mut self, seed: u8, to: Option<&Event>, refs: &[&Event], data: Value) -> Event {
            let mut refs: Vec<String> = refs.iter().map(|e| e.id.clone()).collect();
            refs.sort();
            self.add(
                seed,
                Draft {
                    net: "public".into(),
                    thread: to.map(|t| t.thread_root().to_string()),
                    parent: to.map(|t| t.id.clone()),
                    kind: Some(Kind::Ext(DIGEST_KIND.into())),
                    title: Some("Rust: linking on musl needs musl-tools".into()),
                    text: "install musl-tools".into(),
                    data: data.as_object().unwrap().clone(),
                    refs,
                    ..Draft::default()
                },
            )
        }

        fn used(&mut self, seed: u8, post: &Event) {
            self.reply(seed, post, Kind::React, json!({"used": true}));
        }

        fn accept(&mut self, seed: u8, post: &Event) {
            self.reply(seed, post, Kind::React, json!({"accept": true}));
        }

        /// The knowledge of the threads of these posts, `local` being this
        /// node's profiles.
        fn knowledge_with(&self, local: &[u8], posts: &[&Event]) -> Knowledge {
            let table = self.table(local);
            let grouping = Grouping::new(table.local.clone()).with_table(table);
            let threads: Vec<String> = posts.iter().map(|e| e.thread_root().to_string()).collect();
            compute_at(&self.store, &grouping, &threads, &HashMap::new(), self.ts).unwrap()
        }

        fn knowledge(&self, posts: &[&Event]) -> Knowledge {
            self.knowledge_with(&[], posts)
        }

        /// The id of the entry of `post`'s thread.
        fn entry_of(&self, local: &[u8], post: &Event) -> Option<String> {
            self.knowledge_with(local, &[post])
                .threads
                .get(post.thread_root())
                .map(|e| e.id().to_string())
        }
    }

    #[test]
    fn a_digest_backed_by_two_independent_keys_is_an_entry() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let digest = g.digest(5, Some(&ask), &[&answer], json!({}));
        assert_eq!(g.entry_of(&[], &ask), None, "unverified");
        // One key: confirmed, not yet an entry.
        g.confirm(3, &digest);
        assert_eq!(g.signals(&digest).status, Status::Confirmed);
        assert_eq!(g.entry_of(&[], &ask), None, "one key");
        // A second, independent key that used it: an entry.
        g.used(4, &digest);
        let k = g.knowledge(&[&ask]);
        let entry = &k.threads[&ask.id];
        assert_eq!(entry.id(), digest.id);
        assert_eq!(entry.sources, vec![answer.id.clone()]);
        assert!(k.digests.contains_key(&digest.id));
        assert_eq!(k.of(&answer).map(Entry::id), Some(digest.id.as_str()));
        assert!(k.entry(&digest.id).is_some() && k.entry(&answer.id).is_none());
        // Keys that weigh nothing back nothing: fresh keys 101 and 102.
        let other = g.root(1, Kind::Ask, "else?");
        let fresh = g.digest(5, Some(&other), &[&answer], json!({}));
        g.confirm(101, &fresh);
        g.used(102, &fresh);
        assert_eq!(g.entry_of(&[], &other), None);
    }

    #[test]
    fn two_keys_of_one_group_back_a_digest_once() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let entry = |g: &mut Graph, local: &[u8], backers: &dyn Fn(&mut Graph, &Event)| {
            let digest = g.digest(5, Some(&ask), &[&answer], json!({}));
            backers(g, &digest);
            g.knowledge_with(local, &[&ask])
                .digests
                .contains_key(&digest.id)
        };
        // A key and the key it rotated into.
        g.rotate(3, 6);
        assert!(!entry(&mut g, &[], &|g, d| {
            g.confirm(3, d);
            g.confirm(6, d);
        }));
        // Two profiles of this node.
        assert!(!entry(&mut g, &[7, 8], &|g, d| {
            g.confirm(7, d);
            g.used(8, d);
        }));
        // Elsewhere, 7 and 8 are two keys: an entry.
        assert!(entry(&mut g, &[], &|g, d| {
            g.confirm(7, d);
            g.used(8, d);
        }));
        // A declared owner.
        g.root_with(9, Kind::Announce, "me", json!({"owner": "Acme"}));
        g.root_with(10, Kind::Announce, "me too", json!({"owner": "acme"}));
        assert!(!entry(&mut g, &[], &|g, d| {
            g.confirm(9, d);
            g.confirm(10, d);
        }));
        // A vouch cluster: 1 is this node and used 11's answer; 11 alone
        // vouches for the new keys 108 and 109, which so weigh a little.
        let elevens = g.reply(11, &ask, Kind::Answer, json!({}));
        g.used(1, &elevens);
        g.announce(108);
        g.announce(109);
        g.vouch(11, 108);
        g.vouch(11, 109);
        assert!(g.weight(&[1], 108) > 0.0);
        assert!(!entry(&mut g, &[1], &|g, d| {
            g.confirm(108, d);
            g.confirm(109, d);
        }));
        assert!(entry(&mut g, &[1], &|g, d| {
            g.confirm(108, d);
            g.confirm(12, d);
        }));
    }

    #[test]
    fn the_askers_accept_promotes_a_digest_replying_to_its_ask() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let digest = g.digest(5, Some(&ask), &[&answer], json!({}));
        // Someone else's accept is nothing.
        g.accept(3, &digest);
        assert_eq!(g.entry_of(&[], &ask), None);
        g.accept(1, &digest);
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(digest.id.as_str()));
        // A digest opening its own thread has no asker to accept it.
        let own = g.digest(5, None, &[&answer], json!({}));
        g.accept(1, &own);
        assert!(!g.knowledge(&[&own]).digests.contains_key(&own.id));
    }

    #[test]
    fn a_received_digest_without_refs_is_valid_and_never_promoted() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let bare = g.digest(5, Some(&ask), &[], json!({}));
        assert!(bare.refs.is_empty());
        g.confirm(3, &bare);
        g.confirm(4, &bare);
        g.accept(1, &bare);
        assert_eq!(g.signals(&bare).status, Status::Confirmed);
        let k = g.knowledge(&[&ask]);
        assert!(k.threads.is_empty() && k.digests.is_empty(), "{k:?}");
    }

    #[test]
    fn a_contested_digest_hands_over_to_the_next_best() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        g.confirm(3, &answer);
        g.confirm(4, &answer);
        // The thread's fallback: its canonical answer.
        let k = g.knowledge(&[&ask]);
        assert_eq!(k.threads[&ask.id].id(), answer.id);
        assert_eq!(k.threads[&ask.id].sources, vec![ask.id.clone()]);
        // A digest covering it, once promoted, is preferred.
        let first = g.digest(5, Some(&ask), &[&answer], json!({}));
        g.confirm(6, &first);
        g.confirm(7, &first);
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(first.id.as_str()));
        // A newer digest by another key, promoted too and better backed.
        let second = g.digest(8, Some(&ask), &[&answer], json!({}));
        g.confirm(9, &second);
        g.confirm(10, &second);
        g.used(11, &second);
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(second.id.as_str()));
        // Contested, it is no entry: the other digest takes over.
        g.contradict(12, &second, "fails on alpine");
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(first.id.as_str()));
        // Both contested: back to the thread's canonical answer.
        g.contradict(13, &first, "fails too");
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(answer.id.as_str()));
    }

    #[test]
    fn a_stale_entry_loses_it() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let digest = g.digest(5, Some(&ask), &[&answer], json!({}));
        g.confirm(3, &digest);
        g.confirm(4, &digest);
        g.later(300 * DAY);
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(digest.id.as_str()));
        g.later(61 * DAY);
        assert_eq!(g.signals(&digest).status, Status::Stale);
        assert_eq!(g.entry_of(&[], &ask), None);
        // One renewal is not two keys.
        g.reply(3, &digest, Kind::React, json!({"still_valid": true}));
        assert_eq!(g.signals(&digest).status, Status::Confirmed);
        assert_eq!(g.entry_of(&[], &ask), None);
        g.reply(4, &digest, Kind::React, json!({"still_valid": true}));
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(digest.id.as_str()));
    }

    #[test]
    fn a_thread_falls_back_on_its_best_confirmed_answer() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let a = g.reply(2, &ask, Kind::Answer, json!({}));
        // Accepted alone: confirmed, no entry.
        g.accept(1, &a);
        assert_eq!(g.signals(&a).status, Status::Confirmed);
        assert_eq!(g.entry_of(&[], &ask), None);
        // The asker's own confirmation is not independent of the asker.
        g.confirm(1, &a);
        assert_eq!(g.entry_of(&[], &ask), None);
        // Nor is a key the asker rotated into.
        g.rotate(1, 14);
        g.used(14, &a);
        assert_eq!(g.entry_of(&[], &ask), None);
        // Accepted plus one independent key: an entry.
        g.used(3, &a);
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(a.id.as_str()));

        // Without an accept: two uses are not enough, two confirmations
        // with evidence are.
        let other = g.root(1, Kind::Ask, "else?");
        let b = g.reply(2, &other, Kind::Answer, json!({}));
        g.used(3, &b);
        g.used(4, &b);
        assert_eq!(g.entry_of(&[], &other), None);
        g.confirm(5, &b);
        assert_eq!(g.entry_of(&[], &other), None);
        g.confirm(6, &b);
        assert_eq!(g.entry_of(&[], &other).as_deref(), Some(b.id.as_str()));
        // The best-scored qualifying answer is the canonical one.
        let c = g.reply(7, &other, Kind::Answer, json!({}));
        for seed in [8, 9, 10, 11, 12] {
            g.confirm(seed, &c);
        }
        assert_eq!(g.entry_of(&[], &other).as_deref(), Some(c.id.as_str()));
        // Contested, the next confirmed answer takes over.
        g.contradict(13, &c, "wrong");
        assert_eq!(g.entry_of(&[], &other).as_deref(), Some(b.id.as_str()));
        // A note is no answer, however confirmed.
        let note = g.reply(2, &other, Kind::Note, json!({}));
        for seed in [15, 16, 17, 18, 19, 20] {
            g.confirm(seed, &note);
        }
        assert_eq!(g.entry_of(&[], &other).as_deref(), Some(b.id.as_str()));
    }

    #[test]
    fn a_digest_covers_the_threads_of_its_refs() {
        let mut g = Graph::new();
        let one = g.root(1, Kind::Ask, "how?");
        let a = g.reply(2, &one, Kind::Answer, json!({}));
        let two = g.root(3, Kind::Ask, "and on alpine?");
        let b = g.reply(4, &two, Kind::Answer, json!({}));
        let three = g.root(3, Kind::Ask, "unrelated");
        // Its own thread, spanning both.
        let digest = g.digest(5, None, &[&a, &b], json!({}));
        g.confirm(6, &digest);
        g.confirm(7, &digest);
        let k = g.knowledge(&[&one, &two, &three, &digest]);
        for thread in [&one, &two, &digest] {
            assert_eq!(k.threads[&thread.id].id(), digest.id);
        }
        assert!(!k.threads.contains_key(&three.id));
        assert_eq!(k.of(&b).map(Entry::id), Some(digest.id.as_str()));
        // A thread asked for alone finds it too.
        assert_eq!(g.entry_of(&[], &b).as_deref(), Some(digest.id.as_str()));
    }

    #[test]
    fn a_newer_digest_by_the_same_key_supersedes_the_older() {
        let mut g = Graph::new();
        let ask = g.root(1, Kind::Ask, "how?");
        let answer = g.reply(2, &ask, Kind::Answer, json!({}));
        let older = g.digest(5, Some(&ask), &[&answer], json!({}));
        for seed in [3, 4, 6, 7, 8] {
            g.confirm(seed, &older);
        }
        let newer = g.digest(5, Some(&ask), &[&answer], json!({"supersedes": older.id}));
        // Not promoted yet: the older one stays the entry.
        assert_eq!(g.entry_of(&[], &ask).as_deref(), Some(older.id.as_str()));
        g.confirm(9, &newer);
        g.confirm(10, &newer);
        // Promoted, it hides the older one, better backed as it is; the
        // older one stays readable, and confirmed.
        let k = g.knowledge(&[&ask]);
        assert_eq!(k.threads[&ask.id].id(), newer.id);
        assert!(!k.digests.contains_key(&older.id));
        assert_eq!(k.signals[&older.id].status, Status::Confirmed);
        // A key's successor supersedes it too.
        g.rotate(5, 21);
        let third = g.digest(21, Some(&ask), &[&answer], json!({"supersedes": newer.id}));
        g.confirm(11, &third);
        g.confirm(12, &third);
        let k = g.knowledge(&[&ask]);
        assert_eq!(k.threads[&ask.id].id(), third.id);
        assert!(!k.digests.contains_key(&newer.id));
        // Another key's `supersedes` hides nothing: both are entries, and
        // the better-backed one is the thread's.
        let other = g.digest(22, Some(&ask), &[&answer], json!({"supersedes": third.id}));
        g.confirm(13, &other);
        g.confirm(14, &other);
        g.confirm(15, &third);
        let k = g.knowledge(&[&ask]);
        assert!(k.digests.contains_key(&third.id) && k.digests.contains_key(&other.id));
        assert_eq!(k.threads[&ask.id].id(), third.id);
    }
}
