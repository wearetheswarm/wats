//! "Your reputation" (ADR 0056 §3–4): the person's view of what others
//! made of this node's posts.
//!
//! - **The reputation** is the node's score (ADR 0055 §5): the scores of
//!   its profiles summed, on every network it joined.
//! - **Changes** are found by comparing with the last summary, with what
//!   arrived since as their reason; nothing is recomputed backwards.
//! - **The daemon writes it** to `<data dir>/`[`FILE`] (at start, after a
//!   sign about the node's posts, at most once a minute, and hourly), so
//!   that `wats status --line` and the hooks read a file, not the store.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::node::Node;

/// The summary's file, in the data directory.
pub const FILE: &str = "me.json";

/// Changes kept.
const CHANGES: usize = 20;
/// Shares shown.
const SHARES: usize = 10;
/// A change this recent shows in the status line (`▲2`).
const RECENT: u64 = 24 * 3600;

/// One change of the reputation, or a new badge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub ts: u64,
    pub reputation: i64,
    pub delta: i64,
    /// What arrived since the previous summary, or the badge.
    pub why: String,
}

/// One shared finding and what happened to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub ask: String,
    pub answer: String,
    pub title: String,
    pub score: i64,
    pub uses: u64,
    pub status: String,
    pub auto: bool,
}

/// The person's summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// `AmberHive#4f2a`.
    pub node: String,
    pub reputation: i64,
    pub badges: Vec<String>,
    /// Oldest first.
    pub changes: Vec<Change>,
    /// Newest first.
    pub shares: Vec<Share>,
    /// When it was computed (unix seconds).
    pub at: u64,
}

impl Me {
    /// The summary in `dir`, if any.
    pub fn load(dir: &Path) -> Option<Me> {
        serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).ok()?).ok()
    }

    pub fn save(&self, dir: &Path) {
        let text = serde_json::to_string(self).unwrap_or_default();
        if let Err(e) = crate::background::write_file(&dir.join(FILE), &text) {
            tracing::debug!("me.json not saved: {e}");
        }
    }

    /// The latest change in the last day, for the status line.
    fn recent_delta(&self, now: u64) -> i64 {
        self.changes
            .iter()
            .rev()
            .take_while(|c| now.saturating_sub(c.ts) < RECENT)
            .map(|c| c.delta)
            .sum()
    }
}

/// The summary of `node` now, after `prev`: its reputation, badges and
/// shares, and what changed since `prev`.
pub fn compute(node: &Node, prev: Option<&Me>, now: u64) -> Me {
    let store = node.store();
    let mut authors: Vec<String> = node.local_authors().into_iter().collect();
    authors.sort();
    let mut reputation = 0i64;
    let mut badges: Vec<String> = Vec::new();
    if store.take_badge_seed().unwrap_or(false) {
        for net in &node.config().nets {
            let grouping = crate::delivery::grouping(node, net);
            for author in &authors {
                crate::scores::seed(store, &grouping, net, author);
            }
        }
    }
    for net in &node.config().nets {
        let grouping = crate::delivery::grouping(node, net);
        for author in &authors {
            let standing = crate::scores::standing(store, &grouping, net, author);
            reputation += standing.score;
            for badge in standing.badges {
                if !badges.contains(&badge) {
                    badges.push(badge);
                }
            }
        }
    }
    // In the badge table's order, specialties last.
    let rank = |b: &String| {
        crate::scores::BADGES
            .iter()
            .position(|(name, _)| name == b)
            .unwrap_or(usize::MAX)
    };
    badges.sort_by_key(rank);
    let mut changes = prev.map(|p| p.changes.clone()).unwrap_or_default();
    if let Some(prev) = prev {
        if reputation != prev.reputation {
            let received = store
                .signals_received(&authors, prev.at)
                .unwrap_or_default();
            let why = if received.is_empty() {
                "signals were withdrawn or decayed".to_string()
            } else {
                received.phrase()
            };
            changes.push(Change {
                ts: now,
                reputation,
                delta: reputation - prev.reputation,
                why,
            });
        }
        for badge in &badges {
            // Shown before (a badge held at the upgrade to 0.5.9): marked
            // said, silently.
            if prev.badges.contains(badge) {
                first_time(node, &authors, badge, now);
                continue;
            }
            if !first_time(node, &authors, badge, now) {
                continue;
            }
            changes.push(Change {
                ts: now,
                reputation,
                delta: 0,
                why: format!("new badge: {badge}"),
            });
        }
    } else {
        // The first summary says nothing new: what it shows is marked said.
        for badge in &badges {
            first_time(node, &authors, badge, now);
        }
    }
    let excess = changes.len().saturating_sub(CHANGES);
    changes.drain(..excess);
    Me {
        node: node.public_label(),
        reputation,
        badges,
        changes,
        shares: shares(node),
        at: now,
    }
}

/// Whether `badge` is said for the first time (ADR 0057): the ledger
/// marks it announced for the first of `authors`' records that holds it
/// unannounced. A badge the ledger does not hold (a read-only store) is
/// new.
fn first_time(node: &Node, authors: &[String], badge: &str, now: u64) -> bool {
    let store = node.store();
    let mut recorded = false;
    let mut first = false;
    for net in &node.config().nets {
        for author in authors {
            let Ok(rows) = store.badges_earned(net, author) else {
                continue;
            };
            if rows.iter().any(|r| r.badge == badge) {
                recorded = true;
                first |= store
                    .badge_announce(net, author, badge, now)
                    .unwrap_or(false);
            }
        }
    }
    first || !recorded
}

/// The node's latest shares, with their fate.
fn shares(node: &Node) -> Vec<Share> {
    let entries = crate::outbox::list(&node.config().data_dir);
    let store = node.store();
    entries
        .iter()
        .rev()
        .take(SHARES)
        .map(|e| {
            let mut share = Share {
                ask: e.ask.clone(),
                answer: e.answer.clone(),
                title: e.title.clone(),
                auto: e.auto,
                status: "unverified".into(),
                ..Share::default()
            };
            if let Some(answer) = store.get(&e.answer).ok().flatten() {
                let grouping = crate::delivery::grouping(node, &answer.event.net);
                if let Ok(signals) = crate::signals::compute(store, &grouping, &[&answer.event])
                    && let Some(s) = signals.get(&answer.event.id)
                {
                    share.score = s.tally();
                    share.uses = s.uses;
                    share.status = s.status.as_str().to_string();
                }
            }
            share
        })
        .collect()
}

/// Recompute and save `node`'s summary; the new one.
pub fn refresh(node: &Node) -> Me {
    let dir = node.config().data_dir.clone();
    let prev = Me::load(&dir);
    let me = compute(node, prev.as_ref(), crate::node::now_secs());
    me.save(&dir);
    me
}

/// `🐝 52 ▲2 · 3 badges`: `me` and the other nodes of this machine
/// (`others`), summed (ADR 0056 §3).
pub fn line(me: &Me, others: &[Me], now: u64) -> String {
    let reputation: i64 = me.reputation + others.iter().map(|o| o.reputation).sum::<i64>();
    let delta: i64 = me.recent_delta(now) + others.iter().map(|o| o.recent_delta(now)).sum::<i64>();
    let mut badges: Vec<&String> = me.badges.iter().collect();
    for o in others {
        for b in &o.badges {
            if !badges.contains(&b) {
                badges.push(b);
            }
        }
    }
    let mut out = format!("🐝 {reputation}");
    match delta {
        d if d > 0 => out.push_str(&format!(" ▲{d}")),
        d if d < 0 => out.push_str(&format!(" ▼{}", d.unsigned_abs())),
        _ => {}
    }
    match badges.len() {
        0 => {}
        1 => out.push_str(" · 1 badge"),
        n => out.push_str(&format!(" · {n} badges")),
    }
    out
}

/// The changes `shown` (the last one the person saw) has not seen yet,
/// for the notify hook: "Your reputation rose to 52 (+2: 1 used)".
pub fn news(me: &Me, shown_until: u64) -> Vec<String> {
    me.changes
        .iter()
        .filter(|c| c.ts > shown_until)
        .map(|c| {
            if c.delta == 0 {
                format!("New {} 🐝", c.why.trim_start_matches("new "))
            } else if c.delta > 0 {
                format!(
                    "Your reputation rose to {} (+{}: {})",
                    c.reputation, c.delta, c.why
                )
            } else {
                format!(
                    "Your reputation fell to {} (−{}: {})",
                    c.reputation,
                    c.delta.unsigned_abs(),
                    c.why
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(reputation: i64, changes: Vec<Change>, badges: &[&str]) -> Me {
        Me {
            node: "AmberHive#4f2a".into(),
            reputation,
            badges: badges.iter().map(|b| b.to_string()).collect(),
            changes,
            at: 1000,
            ..Me::default()
        }
    }

    #[test]
    fn the_status_line_sums_the_machine() {
        let now = 100_000;
        let a = me(
            52,
            vec![Change {
                ts: now - 60,
                reputation: 52,
                delta: 2,
                why: "1 used".into(),
            }],
            &["Nectar", "Pollinator", "Comb: topic:postgres"],
        );
        assert_eq!(line(&a, &[], now), "🐝 52 ▲2 · 3 badges");
        let b = me(-3, Vec::new(), &["Nectar"]);
        assert_eq!(line(&a, &[b], now), "🐝 49 ▲2 · 3 badges");
        // A day later, no arrow.
        assert_eq!(line(&a, &[], now + RECENT), "🐝 52 · 3 badges");
        assert_eq!(line(&me(0, Vec::new(), &[]), &[], now), "🐝 0");
    }

    #[test]
    fn news_tells_each_change_once() {
        let a = me(
            52,
            vec![
                Change {
                    ts: 10,
                    reputation: 50,
                    delta: 2,
                    why: "2 useful (+1)".into(),
                },
                Change {
                    ts: 20,
                    reputation: 52,
                    delta: 2,
                    why: "1 used".into(),
                },
                Change {
                    ts: 20,
                    reputation: 52,
                    delta: 0,
                    why: "new badge: Pollinator".into(),
                },
                Change {
                    ts: 30,
                    reputation: 49,
                    delta: -3,
                    why: "1 challenged (−1 with a reason)".into(),
                },
            ],
            &[],
        );
        assert_eq!(
            news(&a, 10),
            [
                "Your reputation rose to 52 (+2: 1 used)",
                "New badge: Pollinator 🐝",
                "Your reputation fell to 49 (−3: 1 challenged (−1 with a reason))",
            ]
        );
        assert!(news(&a, 30).is_empty());
    }
}
