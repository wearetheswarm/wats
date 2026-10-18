//! Wake limits of the agent runner, on top of the spend budget
//! ([`crate::budget`]) and the hourly cap on runs:
//!
//! - **per author:** at most `agent.wakes_per_author_per_hour` runs
//!   triggered by events of the same author key in any hour (0, the
//!   default: unlimited). An event held by this limit is not dropped: it
//!   stays queued and wakes the agent at the first run its author allows.
//!   The state is rebuilt at start from the run index ([`crate::runs`]),
//!   so a restart does not reset it.
//! - **loop detector:** when the agent and one other author have taken
//!   more than `agent.loop_max_exchanges` turns in a row in a thread (30
//!   by default; 0: off), the thread no longer wakes the agent until a
//!   third author posts in it. See [`loop_turns`] for the exact rule. It
//!   is derived from the events of the thread in the store each time it
//!   is checked, so it survives a restart with nothing to rebuild.
//!
//! Time is unix seconds, given by the caller.

use std::collections::{HashMap, VecDeque};

/// The window of the per-author limit.
const HOUR: u64 = 3600;

/// Wake limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeLimits {
    /// Runs one author can trigger per hour; 0: unlimited.
    pub per_author_per_hour: usize,
    /// Turns the agent and one other author may take in a row in a thread
    /// before the thread is paused for the agent; 0: no loop detector.
    pub loop_max_exchanges: usize,
}

impl Default for WakeLimits {
    fn default() -> Self {
        WakeLimits {
            per_author_per_hour: 0,
            loop_max_exchanges: 30,
        }
    }
}

impl WakeLimits {
    /// No limit at all (tests).
    pub fn unlimited() -> Self {
        WakeLimits {
            per_author_per_hour: 0,
            loop_max_exchanges: 0,
        }
    }

    /// Whether a thread whose newest `turns` alternate between the agent
    /// and one other author (see [`loop_turns`]) is paused for the agent.
    pub fn loop_detected(&self, turns: usize) -> bool {
        self.loop_max_exchanges > 0 && turns > self.loop_max_exchanges
    }

    /// How many of a thread's newest events the loop detector reads: enough
    /// for `loop_max_exchanges + 1` turns of up to 8 events each.
    pub fn loop_scan(&self) -> usize {
        self.loop_max_exchanges
            .saturating_add(1)
            .saturating_mul(8)
            .min(10_000)
    }
}

/// The loop detector's count. Given the authors of a thread's events,
/// newest first, the number of turns in a row taken alternately by the
/// agent (`me`) and one other author, and that author:
///
/// - consecutive events by the same author are one turn (an answer and a
///   reaction posted by one run count once);
/// - the other author is the first author other than `me` met going back;
///   the count stops at the first turn by a third author (anyone but `me`
///   and that author: a human, another agent, another profile of this
///   node). A human posting under the agent's own key cannot be told
///   apart from the agent;
/// - without the agent in it, a thread never counts more than one turn.
pub fn loop_turns<'a>(
    me: &str,
    newest_first: impl IntoIterator<Item = &'a str>,
) -> (usize, Option<&'a str>) {
    let mut turns = 0;
    let mut last: Option<&str> = None;
    let mut other: Option<&'a str> = None;
    for author in newest_first {
        if last == Some(author) {
            continue;
        }
        if author != me {
            match other {
                None => other = Some(author),
                Some(o) if o != author => break,
                Some(_) => {}
            }
        }
        turns += 1;
        last = Some(author);
    }
    (turns, other)
}

/// Why an event cannot wake the agent yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// Its author triggered `runs` runs in the last hour; it may wake the
    /// agent at `until` (unix seconds) unless held again.
    Author { runs: usize, until: u64 },
    /// Its thread is paused: the agent and `other` took `turns` turns in a
    /// row, and nobody else posted since.
    Loop { turns: usize, other: String },
}

impl Hold {
    /// For the log.
    pub fn describe(&self, limits: &WakeLimits) -> String {
        match self {
            Hold::Author { runs, .. } => format!(
                "its author triggered {runs} runs in the last hour \
                 (agent.wakes_per_author_per_hour = {})",
                limits.per_author_per_hour
            ),
            Hold::Loop { turns, other } => format!(
                "{turns} turns in a row between the agent and {} \
                 (agent.loop_max_exchanges = {})",
                other.get(..12).unwrap_or(other),
                limits.loop_max_exchanges
            ),
        }
    }
}

/// `90s`, `2m`, `1h`.
pub fn seconds(s: u64) -> String {
    match s {
        s if s >= HOUR && s % HOUR == 0 => format!("{}h", s / HOUR),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Who woke the agent recently.
#[derive(Debug, Clone, Default)]
pub struct WakeState {
    /// Start times of the runs each author triggered, oldest first.
    authors: HashMap<String, VecDeque<u64>>,
}

impl WakeState {
    /// Whether an event by `author` may wake the agent at `now` as far as
    /// the per-author limit goes, or why not.
    pub fn check(&self, limits: &WakeLimits, author: &str, now: u64) -> Option<Hold> {
        if limits.per_author_per_hour > 0
            && let Some(runs) = self.authors.get(author)
        {
            // Oldest first: the runs of the last hour are the newest ones.
            let recent = runs
                .iter()
                .rev()
                .take_while(|t| now < t.saturating_add(HOUR))
                .count();
            if recent >= limits.per_author_per_hour {
                // When enough of them leave the window.
                let oldest_kept = runs.len() - limits.per_author_per_hour;
                let until = runs[oldest_kept] + HOUR;
                return Some(Hold::Author {
                    runs: recent,
                    until,
                });
            }
        }
        None
    }

    /// A run triggered at `now` by events of these authors (each counted
    /// once per run).
    pub fn record<'a>(&mut self, authors: impl IntoIterator<Item = &'a str>, now: u64) {
        let mut seen: Vec<&str> = Vec::new();
        for author in authors {
            if seen.contains(&author) {
                continue;
            }
            seen.push(author);
            self.authors
                .entry(author.to_string())
                .or_default()
                .push_back(now);
        }
        self.prune(now);
    }

    /// Forget what no longer limits anything at `now`.
    pub fn prune(&mut self, now: u64) {
        self.authors.retain(|_, runs| {
            while runs.front().is_some_and(|t| t.saturating_add(HOUR) <= now) {
                runs.pop_front();
            }
            !runs.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_792_152_000;

    fn limits() -> WakeLimits {
        WakeLimits {
            per_author_per_hour: 3,
            ..WakeLimits::default()
        }
    }

    #[test]
    fn defaults() {
        let l = WakeLimits::default();
        assert_eq!(l.per_author_per_hour, 0, "no cap per author by default");
        assert_eq!(l.loop_max_exchanges, 30);
        assert_eq!(WakeLimits::unlimited().loop_max_exchanges, 0);
    }

    #[test]
    fn three_runs_per_author_per_hour() {
        let l = limits();
        let mut s = WakeState::default();
        for n in 0..3u64 {
            let now = T0 + 600 * n;
            assert_eq!(s.check(&l, "alice", now), None);
            s.record(["alice"], now);
        }
        // The fourth waits until the first leaves the window.
        let now = T0 + 1800;
        assert_eq!(
            s.check(&l, "alice", now),
            Some(Hold::Author {
                runs: 3,
                until: T0 + HOUR
            })
        );
        assert_eq!(s.check(&l, "bob", now), None, "per author");
        assert_eq!(s.check(&l, "alice", T0 + HOUR), None);
        // A run out of the window (not pruned yet) does not count.
        let mut old = WakeState::default();
        old.authors
            .insert("alice".into(), [T0 - HOUR, T0, T0 + 60, T0 + 120].into());
        assert_eq!(
            old.check(&l, "alice", T0 + 600),
            Some(Hold::Author {
                runs: 3,
                until: T0 + HOUR
            })
        );
        // Unlimited.
        let off = WakeLimits {
            per_author_per_hour: 0,
            ..l
        };
        assert_eq!(s.check(&off, "alice", now), None);
    }

    #[test]
    fn one_run_counts_once_per_author() {
        let mut s = WakeState::default();
        s.record(["alice", "alice", "bob"], T0);
        assert_eq!(s.authors["alice"].len(), 1);
        assert_eq!(s.authors["bob"].len(), 1);
    }

    #[test]
    fn prune_forgets_what_no_longer_limits() {
        let mut s = WakeState::default();
        s.record(["alice"], T0);
        s.prune(T0 + HOUR - 1);
        assert_eq!(s.authors.len(), 1);
        s.prune(T0 + HOUR);
        assert!(s.authors.is_empty());
    }

    #[test]
    fn unlimited_never_holds() {
        let l = WakeLimits::unlimited();
        let mut s = WakeState::default();
        for _ in 0..10 {
            s.record(["alice"], T0);
        }
        assert_eq!(s.check(&l, "alice", T0), None);
        assert!(!l.loop_detected(usize::MAX));
    }

    /// The loop count of a thread whose authors are given oldest first.
    fn turns(me: &str, oldest_first: &[&'static str]) -> (usize, Option<&'static str>) {
        loop_turns(me, oldest_first.iter().rev().copied())
    }

    #[test]
    fn loop_counts_strict_alternation() {
        assert_eq!(turns("me", &[]), (0, None));
        assert_eq!(turns("me", &["me"]), (1, None));
        assert_eq!(turns("me", &["x"]), (1, Some("x")));
        assert_eq!(turns("me", &["me", "x", "me", "x"]), (4, Some("x")));
        assert_eq!(turns("me", &["x", "me", "x", "me", "x"]), (5, Some("x")));
        // Consecutive events by one author are one turn.
        assert_eq!(
            turns("me", &["x", "me", "me", "x", "x", "x", "me", "x"]),
            (5, Some("x"))
        );
        // Without the agent, never more than one turn.
        assert_eq!(turns("me", &["x", "y", "x", "y"]), (1, Some("y")));
        assert_eq!(turns("me", &["x", "x", "x"]), (1, Some("x")));
    }

    #[test]
    fn a_third_author_resets_the_loop() {
        // The human who asked bounds the run...
        assert_eq!(turns("me", &["human", "x", "me", "x"]), (3, Some("x")));
        // ... and a post by a third author starts it over.
        assert_eq!(
            turns("me", &["x", "me", "x", "me", "x", "human"]),
            (1, Some("human"))
        );
        assert_eq!(
            turns("me", &["x", "me", "x", "me", "x", "human", "x"]),
            (1, Some("x")),
            "x alone after the human: one turn"
        );
        assert_eq!(
            turns("me", &["x", "me", "x", "me", "x", "y", "me", "x"]),
            (2, Some("x"))
        );
    }

    #[test]
    fn loop_threshold_is_more_than_n() {
        let l = WakeLimits {
            loop_max_exchanges: 3,
            ..WakeLimits::default()
        };
        let three = turns("me", &["me", "x", "me"]).0;
        let four = turns("me", &["me", "x", "me", "x"]).0;
        assert!(!l.loop_detected(three), "N turns: still wakes");
        assert!(l.loop_detected(four), "N + 1 turns: paused");
        assert!(WakeLimits::default().loop_detected(31));
        assert!(!WakeLimits::default().loop_detected(30));
        // 0: off.
        let off = WakeLimits {
            loop_max_exchanges: 0,
            ..l
        };
        assert!(!off.loop_detected(four));
        assert!(!off.loop_detected(10_000));
        assert_eq!(l.loop_scan(), 32);
        assert_eq!(WakeLimits::default().loop_scan(), 248);
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(seconds(45), "45s");
        assert_eq!(seconds(120), "2m");
        assert_eq!(seconds(3600), "1h");
        assert_eq!(seconds(90), "90s");
    }
}
