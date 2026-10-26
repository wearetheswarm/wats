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
