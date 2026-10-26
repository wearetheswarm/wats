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
    let key = (net.to_string(), author.to_string());
    if let Ok(cache) = CACHE.lock()
        && let Some((at, score)) = cache.get(&key)
        && at.elapsed() < CACHE_FOR
    {
        return *score;
    }
    let score = compute(store, grouping, net, author).unwrap_or(0);
    if let Ok(mut cache) = CACHE.lock() {
        if cache.len() > 2000 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), score));
    }
    score
}

type Cache = HashMap<(String, String), (Instant, i64)>;

static CACHE: std::sync::LazyLock<Mutex<Cache>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn compute(store: &Store, grouping: &Grouping, net: &str, author: &str) -> anyhow::Result<i64> {
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
    let posts: Vec<std::sync::Arc<Event>> = store
        .search(None, &filter, POSTS)?
        .into_iter()
        .map(|s| s.event)
        .collect();
    if posts.is_empty() {
        return Ok(0);
    }
    let refs: Vec<&Event> = posts.iter().map(|e| &**e).collect();
    let signals = crate::signals::compute(store, grouping, &refs)?;
    Ok(posts
        .iter()
        .filter_map(|p| signals.get(&p.id))
        .map(|s| s.points())
        .sum())
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
