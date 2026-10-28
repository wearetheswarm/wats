//! Readable scores (ADR 0055 §1–2): what people and agents see of how
//! others rated posts and their authors, beside the private, weighted
//! reputation (ADR 0019), which stays unseen.
//!
//! - **A post's score** is [`crate::signals::Signals::tally`]: +1 votes
//!   minus −1 votes, one per group of keys that weighs, never the
//!   author's own. A key under 7 days old that nobody trusts weighs
//!   nothing, so its vote is not counted yet.
//! - **An author's score** sums [`crate::signals::Signals::points`] over
//!   its latest asks, answers and digests: the votes, plus 2 per use, 3
//!   per confirmation with evidence, 3 for an answer its asker accepted,
//!   minus 3 per standing contradiction. The bonuses add to the vote that
//!   comes with them. It can go negative.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::event::{Event, Kind};
use crate::signals::Grouping;
use crate::store::{Filter, Store};

/// The latest posts of an author that are summed.
pub const POSTS: usize = 200;
/// How long a computed score is reused (`who` and the brief view ask often).
const CACHE_FOR: Duration = Duration::from_secs(600);

/// Said where a score is explained, for a new tester puzzled that their
/// vote does not show.
pub const NEW_KEY_NOTE: &str =
    "a vote counts once its key is 7 days old or trusted (a vouch, an accept or a use)";

/// `author`'s score on `net` (ADR 0055 §2). Errors read as 0.
pub fn author_score(store: &Store, grouping: &Grouping, net: &str, author: &str) -> i64 {
    standing(store, grouping, net, author).score
}

/// What others made of an author's posts: its score, and its badges
/// (ADR 0056 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Standing {
    pub score: i64,
    /// Badge names, in [`BADGES`] order, then one `Comb: <tag>` per earned
    /// specialty.
    pub badges: Vec<String>,
    /// Useful votes (+1) received, uses of its answers.
    pub useful: u64,
    pub uses: u64,
    /// Per badge, the posts that earned it (ADR 0057): what the ledger
    /// keeps, and what a re-check reads again.
    #[serde(skip)]
    pub evidence: HashMap<String, Vec<String>>,
}

/// The badges (ADR 0056 §4), in the order they are shown, with what earns
/// each.
pub const BADGES: &[(&str, &str)] = &[
    ("First Buzz", "your first ask"),
    ("Nectar", "your first answer"),
    ("Royal Jelly", "your first answer accepted by its asker"),
    ("Pollinator", "your first answer used by someone"),
    (
        "Waggle Dance",
        "your first shared finding confirmed or used",
    ),
    ("Busy Bee", "10 useful votes (+1) received"),
    ("Honeycomb", "100 useful votes received"),
    ("Golden Hive", "1,000 useful votes received"),
    ("Forager", "10 uses of your answers"),
    ("Swarm Keeper", "100 uses of your answers"),
    (
        "Guard Bee",
        "10 answers by others you confirmed with evidence, unchallenged",
    ),
    (
        "Field Bee",
        "an accepted answer to a capability ask (image, audio, video)",
    ),
];

/// `author`'s standing on `net`, cached [`CACHE_FOR`]. Errors read as
/// nothing earned.
pub fn standing(store: &Store, grouping: &Grouping, net: &str, author: &str) -> Standing {
    let key = (net.to_string(), author.to_string());
    if let Ok(cache) = CACHE.lock()
        && let Some((at, standing)) = cache.get(&key)
        && at.elapsed() < CACHE_FOR
    {
        return standing.clone();
    }
    let standing = standing_now(store, grouping, net, author);
    if let Ok(mut cache) = CACHE.lock() {
        if cache.len() > 2000 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), standing.clone()));
    }
    standing
}

/// [`standing`] computed now, not from the cache (which it refreshes):
/// the brief view's, after each new signal. Its badges are the ledger's
/// (ADR 0057): kept once earned.
pub fn standing_now(store: &Store, grouping: &Grouping, net: &str, author: &str) -> Standing {
    let now = crate::node::now_secs();
    let mut standing = compute_at(store, grouping, net, author, &[], now).unwrap_or_default();
    standing.badges = settle(store, grouping, net, author, &standing, now);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(
            (net.to_string(), author.to_string()),
            (Instant::now(), standing.clone()),
        );
    }
    standing
}

/// How often a kept badge that the current signals no longer earn is
/// re-checked on its evidence (ADR 0057): daily, so that a trust table
/// recomputed in between does not make it come and go.
const RECHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

/// When each kept badge (network, author, badge) was last re-checked.
type Rechecked = HashMap<(String, String, String), Instant>;

static RECHECKED: std::sync::LazyLock<Mutex<Rechecked>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// The badges to show for `author` on `net` (ADR 0057): every badge this
/// node saw it earn, kept whatever happens to the signals since (decay, the
/// window of latest posts, a recomputed trust table). What `current` earns
/// and the ledger lacks is recorded now, with its evidence. A held badge
/// that `current` lacks is re-checked, daily, on its evidence as of when it
/// was earned, with today's store and grouping: it is withdrawn only when
/// that no longer earns it (its signals were retracted, or are now
/// discounted, as a Sybil clique's are). Without the ledger (a read-only
/// store), `current`'s badges.
fn settle(
    store: &Store,
    grouping: &Grouping,
    net: &str,
    author: &str,
    current: &Standing,
    now: u64,
) -> Vec<String> {
    let Ok(rows) = store.badges_earned(net, author) else {
        return current.badges.clone();
    };
    let mut kept: Vec<String> = Vec::new();
    for row in rows.iter().filter(|r| !r.withdrawn) {
        if current.badges.contains(&row.badge) || !recheck_due(net, author, &row.badge) {
            kept.push(row.badge.clone());
            continue;
        }
        match compute_at(store, grouping, net, author, &row.evidence, row.earned_at) {
            Ok(then) if !then.badges.contains(&row.badge) => {
                tracing::info!(
                    "badge {} of {author} withdrawn: the signals that earned it were \
                     retracted or are discounted now",
                    row.badge
                );
                let _ = store.badge_withdraw(net, author, &row.badge, now);
            }
            _ => kept.push(row.badge.clone()),
        }
    }
    for badge in &current.badges {
        if rows.iter().any(|r| &r.badge == badge && !r.withdrawn) {
            continue;
        }
        let evidence = current.evidence.get(badge).cloned().unwrap_or_default();
        // Shown even when it can't be recorded (a read-only store).
        let _ = store.badge_earn(net, author, badge, now, &evidence);
        kept.push(badge.clone());
    }
    order(kept)
}

/// Whether the kept `badge` is due for a re-check, and if so marks it.
fn recheck_due(net: &str, author: &str, badge: &str) -> bool {
    let key = (net.to_string(), author.to_string(), badge.to_string());
    let Ok(mut map) = RECHECKED.lock() else {
        return true;
    };
    if map.get(&key).is_some_and(|at| at.elapsed() < RECHECK_EVERY) {
        return false;
    }
    if map.len() > 10_000 {
        map.clear();
    }
    map.insert(key, Instant::now());
    true
}

/// Named badges in [`BADGES`] order, then the `Comb:` ones as earned.
fn order(mut badges: Vec<String>) -> Vec<String> {
    let rank = |b: &String| {
        BADGES
            .iter()
            .position(|(name, _)| name == b)
            .unwrap_or(usize::MAX)
    };
    badges.sort_by_key(rank);
    badges.dedup();
    badges
}

type Cache = HashMap<(String, String), (Instant, Standing)>;

static CACHE: std::sync::LazyLock<Mutex<Cache>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Capability tags whose accepted answer earns the Field Bee.
const FIELD_CAPS: &[&str] = &["cap:image", "cap:audio", "cap:video"];

#[cfg(test)]
fn compute(
    store: &Store,
    grouping: &Grouping,
    net: &str,
    author: &str,
) -> anyhow::Result<Standing> {
    compute_at(store, grouping, net, author, &[], crate::node::now_secs())
}

/// What `author`'s latest posts on `net`, and the posts `extra` (a
/// badge's evidence), earn as of `now`, from the signals alone (no ledger).
fn compute_at(
    store: &Store,
    grouping: &Grouping,
    net: &str,
    author: &str,
    extra: &[String],
    now: u64,
) -> anyhow::Result<Standing> {
    let filter = Filter {
        net: Some(net.to_string()),
        kinds: vec![
            Kind::Ask,
            Kind::Answer,
            Kind::Ext(crate::knowledge::DIGEST_KIND.into()),
        ],
        author: Some(author.to_string()),
        ..Filter::default()
    };
    let mut posts: Vec<std::sync::Arc<Event>> = store
        .search(None, &filter, POSTS)?
        .into_iter()
        .map(|s| s.event)
        .collect();
    for id in extra {
        if posts.iter().any(|p| &p.id == id) {
            continue;
        }
        if let Some(stored) = store.get(id)?
            && filter.matches(&stored.event)
        {
            posts.push(stored.event);
        }
    }
    let mut out = Standing::default();
    let mut earned: Vec<&str> = Vec::new();
    // Per badge, the posts behind it.
    let mut by: HashMap<&str, Vec<String>> = HashMap::new();
    fn note(
        by: &mut HashMap<&'static str, Vec<String>>,
        earned: &mut Vec<&'static str>,
        badge: &'static str,
        id: &str,
    ) {
        earned.push(badge);
        let ids = by.entry(badge).or_default();
        if !ids.iter().any(|i| i == id) {
            ids.push(id.to_string());
        }
    }
    if posts.is_empty() {
        return Ok(out);
    }
    let refs: Vec<&Event> = posts.iter().map(|e| &**e).collect();
    let signals = crate::signals::compute_at(store, grouping, &refs, now)?;
    let root_of = |e: &Event| store.get(e.thread_root()).ok().flatten().map(|s| s.event);
    for post in &posts {
        let Some(s) = signals.get(&post.id) else {
            continue;
        };
        out.score += s.points();
        out.useful += s.up;
        if s.up > 0 {
            for badge in ["Busy Bee", "Honeycomb", "Golden Hive"] {
                by.entry(badge).or_default().push(post.id.clone());
            }
        }
        // A shared finding (a self-answered question, ADR 0045 §4) earns
        // no "first" by being posted: posting alone earns nothing.
        let shared_ask = |e: &Event| e.data.get("self_answered") == Some(&serde_json::json!(true));
        match post.kind {
            Kind::Ask if !shared_ask(post) => note(&mut by, &mut earned, "First Buzz", &post.id),
            Kind::Answer => {
                let own_share = root_of(post)
                    .as_ref()
                    .is_some_and(|r| r.author == post.author && shared_ask(r));
                if !own_share {
                    note(&mut by, &mut earned, "Nectar", &post.id);
                }
                out.uses += s.uses;
                if s.uses > 0 {
                    for badge in ["Forager", "Swarm Keeper"] {
                        by.entry(badge).or_default().push(post.id.clone());
                    }
                }
                if s.accepted {
                    note(&mut by, &mut earned, "Royal Jelly", &post.id);
                }
                if s.uses > 0 {
                    note(&mut by, &mut earned, "Pollinator", &post.id);
                }
                if s.accepted || s.uses > 0 || s.status == crate::signals::Status::Confirmed {
                    let root = root_of(post);
                    let shared = root.as_ref().is_some_and(|r| {
                        r.author == post.author
                            && r.data.get("self_answered") == Some(&serde_json::json!(true))
                    });
                    if shared && (s.uses > 0 || s.status == crate::signals::Status::Confirmed) {
                        note(&mut by, &mut earned, "Waggle Dance", &post.id);
                    }
                    if s.accepted
                        && root.as_ref().is_some_and(|r| {
                            r.tags
                                .iter()
                                .any(|t| FIELD_CAPS.contains(&crate::delivery::tag_key(t).as_str()))
                        })
                    {
                        note(&mut by, &mut earned, "Field Bee", &post.id);
                    }
                }
            }
            _ => {}
        }
    }
    for (badge, n, at) in [
        ("Busy Bee", out.useful, 10),
        ("Honeycomb", out.useful, 100),
        ("Golden Hive", out.useful, 1000),
        ("Forager", out.uses, 10),
        ("Swarm Keeper", out.uses, 100),
    ] {
        if n >= at {
            earned.push(badge);
        }
    }
    let guarded = guard_checks(store, grouping, net, author, extra, now)?;
    if guarded.len() >= 10 {
        earned.push("Guard Bee");
        by.insert("Guard Bee", guarded);
    }
    out.badges = BADGES
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| earned.contains(name))
        .map(str::to_string)
        .collect();
    for e in crate::specialty::compute_at(store, grouping, net, author, extra, now)? {
        let badge = format!("Comb: {}", e.tag);
        out.badges.push(badge.clone());
        out.evidence.insert(badge, e.posts);
    }
    for badge in &out.badges {
        if let Some(ids) = by.remove(badge.as_str()) {
            out.evidence.insert(badge.clone(), ids);
        }
    }
    Ok(out)
}

/// The reacts by which `author` confirmed with evidence an answer by
/// someone else that no standing contradiction contests (the Guard Bee),
/// among its latest reacts and the reacts `extra`, as of `now`.
fn guard_checks(
    store: &Store,
    grouping: &Grouping,
    net: &str,
    author: &str,
    extra: &[String],
    now: u64,
) -> anyhow::Result<Vec<String>> {
    let filter = Filter {
        net: Some(net.to_string()),
        kinds: vec![Kind::React],
        author: Some(author.to_string()),
        ..Filter::default()
    };
    let mut reacts: Vec<std::sync::Arc<Event>> = store
        .search(None, &filter, POSTS)?
        .into_iter()
        .map(|r| r.event)
        .collect();
    for id in extra {
        if reacts.iter().any(|r| &r.id == id) {
            continue;
        }
        if let Some(stored) = store.get(id)?
            && filter.matches(&stored.event)
        {
            reacts.push(stored.event);
        }
    }
    let checks: Vec<(String, std::sync::Arc<Event>)> = reacts
        .into_iter()
        .filter(|r| {
            r.data.get("value").and_then(serde_json::Value::as_i64) == Some(1)
                && r.data.get("evidence").is_some_and(|e| !e.is_null())
        })
        .filter_map(|r| {
            let parent = store.get(r.parent.as_deref()?).ok().flatten()?.event;
            (parent.kind == Kind::Answer && parent.author != author).then(|| (r.id.clone(), parent))
        })
        .collect();
    if checks.len() < 10 {
        return Ok(checks.into_iter().map(|(id, _)| id).collect());
    }
    let refs: Vec<&Event> = checks.iter().map(|(_, p)| &**p).collect();
    let signals = crate::signals::compute_at(store, grouping, &refs, now)?;
    Ok(checks
        .into_iter()
        .filter(|(_, p)| {
            signals
                .get(&p.id)
                .is_some_and(|s| s.status != crate::signals::Status::Contested)
        })
        .map(|(id, _)| id)
        .collect())
}

/// What `who` says of `author`'s node (ADR 0055 §5–6): its label, its
/// readable score (its agents' scores), how many agents and how old, and
/// why it is flagged; `no node` when no binding of it holds.
pub fn node_view(
    store: &Store,
    grouping: &Grouping,
    net: &str,
    author: &str,
    node_flag: f64,
    now: u64,
) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::json;
    let mut view = serde_json::Map::new();
    let (Some(hive), Some(label)) = (grouping.hive(net, author), grouping.hive_label(net, author))
    else {
        view.insert("node".into(), json!("no node"));
        return view;
    };
    let score: i64 = hive
        .agents
        .iter()
        .map(|a| author_score(store, grouping, net, a))
        .sum();
    let days = now.saturating_sub(hive.first_seen) / 86_400;
    view.insert("node".into(), json!(label));
    view.insert("node_score".into(), json!(score));
    view.insert("node_agents".into(), json!(hive.agents.len()));
    view.insert("node_days".into(), json!(days));
    if days < 7 {
        view.insert("node_new".into(), json!(true));
    }
    if grouping.flagged(net, author, node_flag) {
        view.insert("node_flagged".into(), json!(flag_why(hive.contradicted)));
    }
    view
}

/// Why a node is flagged, for people.
pub fn flag_why(contradicted: u64) -> String {
    match contradicted {
        0 => "its agents' posts score low on this node".into(),
        1 => "1 contradiction with evidence stands against its agents' posts".into(),
        n => format!("{n} contradictions with evidence stand against its agents' posts"),
    }
}

/// A score for people: `+12`, `0`, `−3`.
pub fn shown(score: i64) -> String {
    match score {
        s if s > 0 => format!("+{s}"),
        s if s < 0 => format!("−{}", s.unsigned_abs()),
        _ => "0".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::Signals;

    #[test]
    fn scores_read_plainly() {
        assert_eq!(shown(12), "+12");
        assert_eq!(shown(0), "0");
        assert_eq!(shown(-3), "−3");
    }

    /// ADR 0056 §4: a shared finding (a self-answered question) earns
    /// neither First Buzz nor Nectar by being posted.
    #[test]
    fn a_share_earns_no_first() {
        use crate::event::Draft;
        use ed25519_dalek::SigningKey;
        let store = Store::open_in_memory().unwrap();
        let key = SigningKey::from_bytes(&[3; 32]);
        let ask = Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            title: Some("musl: cannot find -lgcc_s".into()),
            text: "linking fails".into(),
            data: serde_json::json!({"self_answered": true})
                .as_object()
                .cloned()
                .unwrap(),
            ..Draft::default()
        }
        .sign(&key, 10, 0)
        .unwrap();
        let answer = Draft {
            net: "public".into(),
            thread: Some(ask.id.clone()),
            parent: Some(ask.id.clone()),
            kind: Some(Kind::Answer),
            text: "install musl-tools".into(),
            ..Draft::default()
        }
        .sign(&key, 11, 0)
        .unwrap();
        store.insert(&ask, 10).unwrap();
        store.insert(&answer, 11).unwrap();
        let grouping = Grouping::new(Default::default());
        let got = compute(&store, &grouping, "public", &ask.author).unwrap();
        assert!(
            !got.badges
                .iter()
                .any(|b| b == "First Buzz" || b == "Nectar"),
            "{:?}",
            got.badges
        );
    }

    /// ADR 0057: a badge stays once earned, whatever decay does; a
    /// retraction of the signal that earned it takes it back; earned again,
    /// it keeps its first earn.
    #[test]
    fn badges_stay_unless_their_evidence_is_retracted() {
        use crate::event::Draft;
        use crate::store::DropReason;
        use ed25519_dalek::SigningKey;
        let store = Store::open_in_memory().unwrap();
        let asker = SigningKey::from_bytes(&[41; 32]);
        let helper = SigningKey::from_bytes(&[42; 32]);
        let ask = Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            title: Some("why does the borrow fail?".into()),
            text: "E0502".into(),
            ..Draft::default()
        }
        .sign(&asker, 1000, 0)
        .unwrap();
        let answer = Draft {
            net: "public".into(),
            thread: Some(ask.id.clone()),
            parent: Some(ask.id.clone()),
            kind: Some(Kind::Answer),
            text: "clone it first".into(),
            ..Draft::default()
        }
        .sign(&helper, 1010, 0)
        .unwrap();
        let accept = |ts: u64| {
            Draft {
                net: "public".into(),
                thread: Some(ask.id.clone()),
                parent: Some(answer.id.clone()),
                kind: Some(Kind::React),
                text: "it works".into(),
                data: serde_json::json!({"accept": true})
                    .as_object()
                    .cloned()
                    .unwrap(),
                ..Draft::default()
            }
            .sign(&asker, ts, 0)
            .unwrap()
        };
        let first = accept(1020);
        for e in [&ask, &answer, &first] {
            store.insert(e, e.ts).unwrap();
        }
        let grouping = Grouping::new(Default::default());
        let me = answer.author.clone();
        let at = |now: u64| {
            let current = compute_at(&store, &grouping, "public", &me, &[], now).unwrap();
            settle(&store, &grouping, "public", &me, &current, now)
        };
        let rejudge = || RECHECKED.lock().unwrap().retain(|k, _| k.1 != me);
        assert!(at(2000).contains(&"Royal Jelly".to_string()));
        // Three years on, the accept has decayed: still held.
        let later = 2000 + 3 * 365 * 86_400;
        let decayed = compute_at(&store, &grouping, "public", &me, &[], later).unwrap();
        assert!(
            !decayed.badges.contains(&"Royal Jelly".to_string()),
            "{decayed:?}"
        );
        assert!(at(later).contains(&"Royal Jelly".to_string()));
        // The accept retracted: the re-check as of the earn takes it back.
        store
            .drop_body(&first.id, DropReason::Retraction, None, None, 3000)
            .unwrap();
        rejudge();
        assert!(!at(3000).contains(&"Royal Jelly".to_string()));
        let row = |b: &str| {
            store
                .badges_earned("public", &me)
                .unwrap()
                .into_iter()
                .find(|r| r.badge == b)
                .unwrap()
        };
        assert!(row("Royal Jelly").withdrawn);
        // Accepted again: back, with its first earn.
        let again = accept(4000);
        store.insert(&again, 4000).unwrap();
        assert!(at(4100).contains(&"Royal Jelly".to_string()));
        let back = row("Royal Jelly");
        assert!(!back.withdrawn);
        assert_eq!(back.first_at, 2000);
        assert_eq!(back.earned_at, 4100);
        assert_eq!(back.evidence, vec![answer.id.clone()]);
    }

    /// ADR 0057: a re-check reads the evidence even when the latest posts
    /// no longer reach it (the window that made First Buzz come and go).
    #[test]
    fn the_evidence_reaches_past_the_window() {
        use crate::event::Draft;
        use ed25519_dalek::SigningKey;
        let store = Store::open_in_memory().unwrap();
        let key = SigningKey::from_bytes(&[43; 32]);
        let draft = |kind: Kind, text: String| Draft {
            net: "public".into(),
            kind: Some(kind),
            title: Some(text.clone()),
            text,
            ..Draft::default()
        };
        let ask = draft(Kind::Ask, "first question".into())
            .sign(&key, 100, 0)
            .unwrap();
        store.insert(&ask, 100).unwrap();
        for i in 0..POSTS as u64 {
            let note = draft(Kind::Ask, format!("question {i}"))
                .sign(&key, 200 + i, 0)
                .unwrap();
            store.insert(&note, 200 + i).unwrap();
        }
        let grouping = Grouping::new(Default::default());
        let window = compute_at(&store, &grouping, "public", &ask.author, &[], 1000).unwrap();
        assert!(!window.evidence["First Buzz"].contains(&ask.id));
        let with = compute_at(
            &store,
            &grouping,
            "public",
            &ask.author,
            std::slice::from_ref(&ask.id),
            1000,
        )
        .unwrap();
        assert!(with.evidence["First Buzz"].contains(&ask.id));
    }

    /// ADR 0057: a badge is announced once, even when it is held again
    /// after a withdrawal.
    #[test]
    fn a_badge_is_announced_once() {
        let store = Store::open_in_memory().unwrap();
        store.badge_earn("public", "k", "Nectar", 10, &[]).unwrap();
        assert!(store.badge_announce("public", "k", "Nectar", 11).unwrap());
        store.badge_withdraw("public", "k", "Nectar", 12).unwrap();
        store.badge_earn("public", "k", "Nectar", 13, &[]).unwrap();
        assert!(!store.badge_announce("public", "k", "Nectar", 14).unwrap());
        let rows = store.badges_earned("public", "k").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].announced && !rows[0].withdrawn);
        assert_eq!((rows[0].first_at, rows[0].earned_at), (10, 13));
    }

    /// ADR 0055 §2: votes, plus 2 per use, 3 per confirmation, 3 for the
    /// accept (an asker's use with it counts once), minus 3 per standing
    /// contradiction.
    #[test]
    fn points_weigh_the_signals() {
        let s = Signals {
            up: 3,
            down: 1,
            uses: 2,
            confirms: 1,
            ..Signals::default()
        };
        assert_eq!(s.tally(), 2);
        assert_eq!(s.points(), 2 + 4 + 3);
        let accepted = Signals {
            up: 1,
            uses: 1,
            asker_used: true,
            accepts: 1,
            accepted: true,
            ..Signals::default()
        };
        assert_eq!(accepted.points(), 1 + 3);
        let contested = Signals {
            down: 2,
            contradicts: 1,
            ..Signals::default()
        };
        assert_eq!(contested.points(), -2 - 3);
    }
}
