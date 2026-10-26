//! The find-before-ask flow, measured (ADR 0053 §8): what an MCP session
//! did before it asked. Local counters, per day, kept in the store and
//! never sent; `status --json` shows their totals as `find`.
//!
//! - `searches`, `searches_semantic` (the model ran), `search_meaning`
//!   (a hit by meaning or both), `search_no_meaning`;
//! - `browses`, `browse_after_search` (within [`WINDOW`]);
//! - `reads_meaning_only`: a hit by meaning only, then read;
//! - `asks`, `ask_after_search`, `ask_with_hit` (the search had a hit by
//!   meaning), `ask_after_browse`;
//! - `browse_found`: a read after a browse with no ask within [`WINDOW`],
//!   counted at the session's next call after that.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// How long a search or a browse counts as "before" what follows.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
struct Search {
    at: Instant,
    by_meaning: bool,
    meaning_only: HashSet<String>,
}

/// One MCP session's recent finding.
#[derive(Debug, Default)]
pub struct Session {
    search: Option<Search>,
    browse: Option<Instant>,
    /// A read after a browse, not followed by an ask yet.
    found: Option<Instant>,
}

fn recent(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|at| now.saturating_duration_since(at) <= WINDOW)
}

impl Session {
    /// Counters due by now: a read after a browse that no ask followed.
    pub fn tick(&mut self, now: Instant) -> Vec<&'static str> {
        match self.found {
            Some(at) if now.saturating_duration_since(at) > WINDOW => {
                self.found = None;
                vec!["browse_found"]
            }
            _ => Vec::new(),
        }
    }

    /// A search with text. `semantic`: the model ran; `by_meaning`: a
    /// result matched by meaning or both; `meaning_only`: the ids of those
    /// that matched by meaning only.
    pub fn searched(
        &mut self,
        now: Instant,
        semantic: bool,
        by_meaning: bool,
        meaning_only: HashSet<String>,
    ) -> Vec<&'static str> {
        let mut out = self.tick(now);
        out.push("searches");
        if semantic {
            out.push("searches_semantic");
            out.push(if by_meaning {
                "search_meaning"
            } else {
                "search_no_meaning"
            });
        }
        self.search = Some(Search {
            at: now,
            by_meaning,
            meaning_only,
        });
        out
    }

    pub fn browsed(&mut self, now: Instant) -> Vec<&'static str> {
        let mut out = self.tick(now);
        out.push("browses");
        if recent(self.search.as_ref().map(|s| s.at), now) {
            out.push("browse_after_search");
        }
        self.browse = Some(now);
        out
    }

    pub fn read(&mut self, now: Instant, id: &str) -> Vec<&'static str> {
        let mut out = self.tick(now);
        if let Some(search) = &mut self.search
            && recent(Some(search.at), now)
            && search.meaning_only.remove(id)
        {
            out.push("reads_meaning_only");
        }
        if recent(self.browse, now) && self.found.is_none() {
            self.found = Some(now);
        }
        out
    }

    /// A new ask (a root, not a reply).
    pub fn asked(&mut self, now: Instant) -> Vec<&'static str> {
        let mut out = self.tick(now);
        out.push("asks");
        if let Some(search) = &self.search
            && recent(Some(search.at), now)
        {
            out.push("ask_after_search");
            if search.by_meaning {
                out.push("ask_with_hit");
            }
        }
        if recent(self.browse, now) {
            out.push("ask_after_browse");
        }
        // What the browse found did not do.
        self.found = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_flow() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut s = Session::default();
        let meaning: HashSet<String> = ["p1".to_string()].into();
        assert_eq!(
            s.searched(at(0), true, true, meaning),
            ["searches", "searches_semantic", "search_meaning"]
        );
        assert_eq!(s.read(at(10), "p1"), ["reads_meaning_only"]);
        // Read once only counts once.
        assert!(s.read(at(11), "p1").is_empty());
        assert_eq!(s.browsed(at(20)), ["browses", "browse_after_search"]);
        assert!(s.read(at(30), "p2").is_empty());
        // An ask after it: the browse did not find it.
        assert_eq!(
            s.asked(at(40)),
            [
                "asks",
                "ask_after_search",
                "ask_with_hit",
                "ask_after_browse"
            ]
        );
        assert!(s.tick(at(2000)).is_empty());
        // A browse, a read, and no ask: found.
        assert_eq!(s.browsed(at(3000)), ["browses"]);
        assert!(s.read(at(3010), "p3").is_empty());
        assert!(s.tick(at(3300)).is_empty());
        assert_eq!(s.tick(at(3700)), ["browse_found"]);
        // An ask long after: nothing before it.
        assert_eq!(s.asked(at(9000)), ["asks"]);
        // Without the model: no semantic counters.
        assert_eq!(
            s.searched(at(9100), false, false, HashSet::new()),
            ["searches"]
        );
    }
}
