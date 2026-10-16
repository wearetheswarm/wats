//! Wake limits of the agent runner, on top of the spend budget
//! ([`crate::budget`]) and the hourly cap on runs:
//!
//! - **per author:** at most `agent.wakes_per_author_per_hour` runs
//!   triggered by events of the same author key in any hour (0: unlimited);
//! - **per thread:** after a run triggered in a thread, the next wake from
//!   that thread waits `agent.thread_backoff`, doubled after each further
//!   run (1 min, 2, 4, …) up to `agent.thread_backoff_max`. A thread that
//!   stays quiet for `agent.thread_backoff_max` once its wait is over
//!   starts over.
//!
//! An event held by these limits is not dropped: it stays queued and wakes
//! the agent at the first run its author and thread allow.
//!
//! The state is rebuilt at start from the run index ([`crate::runs`]), so a
//! restart does not reset the limits. Time is unix seconds, given by the
//! caller.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

/// The window of the per-author limit.
const HOUR: u64 = 3600;

/// Wake limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeLimits {
    /// Runs one author can trigger per hour; 0: unlimited.
    pub per_author_per_hour: usize,
    /// Wait before a thread can wake the agent again after a run; doubles
    /// with each further run. Zero: no backoff.
    pub thread_backoff: Duration,
    /// The longest wait, and the quiet period (after the wait) after which
    /// a thread's backoff starts over.
    pub thread_backoff_max: Duration,
}

impl Default for WakeLimits {
    fn default() -> Self {
        WakeLimits {
            per_author_per_hour: 3,
            thread_backoff: Duration::from_secs(60),
            thread_backoff_max: Duration::from_secs(HOUR),
        }
    }
}

impl WakeLimits {
    /// No limit at all (tests).
    pub fn unlimited() -> Self {
        WakeLimits {
            per_author_per_hour: 0,
            thread_backoff: Duration::ZERO,
            thread_backoff_max: Duration::ZERO,
        }
    }

    /// The wait after the `streak`-th run in a row in a thread (1 for the
    /// first): `thread_backoff × 2^(streak-1)`, at most
    /// `thread_backoff_max`.
    pub fn backoff(&self, streak: u32) -> u64 {
        let base = self.thread_backoff.as_secs();
        let max = self.thread_backoff_max.as_secs();
        let doubling = streak.saturating_sub(1).min(63);
        base.saturating_mul(1u64 << doubling).min(max)
    }

    /// Whether a thread whose last run was the `streak`-th in a row, at
    /// `last`, has been quiet long enough at `now` to start over: no wake
    /// for `thread_backoff_max` after its wait ended.
    fn quiet(&self, last: u64, streak: u32, now: u64) -> bool {
        let quiet = self
            .backoff(streak)
            .saturating_add(self.thread_backoff_max.as_secs());
        now.saturating_sub(last) >= quiet
    }
}

/// Why an event cannot wake the agent yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// Its author triggered `runs` runs in the last hour.
    Author { runs: usize, until: u64 },
    /// Its thread woke the agent less than `wait` seconds ago.
    Thread { wait: u64, until: u64 },
}

impl Hold {
    /// Unix seconds when the event may wake the agent (unless held again).
    pub fn until(&self) -> u64 {
        match self {
            Hold::Author { until, .. } | Hold::Thread { until, .. } => *until,
        }
    }

    /// For the log.
    pub fn describe(&self, limits: &WakeLimits) -> String {
        match self {
            Hold::Author { runs, .. } => format!(
                "its author triggered {runs} runs in the last hour \
                 (agent.wakes_per_author_per_hour = {})",
                limits.per_author_per_hour
            ),
            Hold::Thread { wait, .. } => format!(
                "its thread woke the agent recently, backoff {} \
                 (agent.thread_backoff, up to agent.thread_backoff_max)",
                seconds(*wait)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThreadState {
    /// When the thread last woke the agent.
    last: u64,
    /// Runs in a row in this thread, each within the quiet period of the
    /// previous one.
    streak: u32,
}

/// Who and which threads woke the agent recently.
#[derive(Debug, Clone, Default)]
pub struct WakeState {
    /// Start times of the runs each author triggered, oldest first.
    authors: HashMap<String, VecDeque<u64>>,
    threads: HashMap<String, ThreadState>,
}

impl WakeState {
    /// Whether an event by `author` in `thread` may wake the agent at
    /// `now`, or why not.
    pub fn check(&self, limits: &WakeLimits, author: &str, thread: &str, now: u64) -> Option<Hold> {
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
        if let Some(state) = self.threads.get(thread) {
            let wait = limits.backoff(state.streak);
            let until = state.last.saturating_add(wait);
            if now < until {
                return Some(Hold::Thread { wait, until });
            }
        }
        None
    }

    /// A run triggered at `now` by events of these authors in these
    /// threads (each counted once per run).
    pub fn record<'a>(
        &mut self,
        limits: &WakeLimits,
        authors: impl IntoIterator<Item = &'a str>,
        threads: impl IntoIterator<Item = &'a str>,
        now: u64,
    ) {
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
        seen.clear();
        for thread in threads {
            if seen.contains(&thread) {
                continue;
            }
            seen.push(thread);
            let state = self
                .threads
                .entry(thread.to_string())
                .or_insert(ThreadState {
                    last: now,
                    streak: 0,
                });
            state.streak = if state.streak == 0 || limits.quiet(state.last, state.streak, now) {
                1
            } else {
                state.streak.saturating_add(1)
            };
            state.last = now;
        }
        self.prune(limits, now);
    }

    /// Forget what no longer limits anything at `now`.
    pub fn prune(&mut self, limits: &WakeLimits, now: u64) {
        self.authors.retain(|_, runs| {
            while runs.front().is_some_and(|t| t.saturating_add(HOUR) <= now) {
                runs.pop_front();
            }
            !runs.is_empty()
        });
        self.threads
            .retain(|_, state| !limits.quiet(state.last, state.streak, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_792_152_000;

    fn limits() -> WakeLimits {
        WakeLimits::default()
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let l = limits();
        let waits: Vec<u64> = (1..=9).map(|s| l.backoff(s)).collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        assert_eq!(l.backoff(u32::MAX), 3600, "no overflow");
        assert_eq!(WakeLimits::unlimited().backoff(5), 0);
    }

    #[test]
    fn three_runs_per_author_per_hour() {
        let l = limits();
        let mut s = WakeState::default();
        // Three runs, each in its own thread.
        for (n, thread) in ["t1", "t2", "t3"].into_iter().enumerate() {
            let now = T0 + 600 * n as u64;
            assert_eq!(s.check(&l, "alice", thread, now), None);
            s.record(&l, ["alice"], [thread], now);
        }
        // The fourth waits until the first leaves the window.
        let now = T0 + 1800;
        assert_eq!(
            s.check(&l, "alice", "t4", now),
            Some(Hold::Author {
                runs: 3,
                until: T0 + HOUR
            })
        );
        assert_eq!(s.check(&l, "bob", "t4", now), None, "per author");
        assert_eq!(s.check(&l, "alice", "t4", T0 + HOUR), None);
        // A run out of the window (not pruned yet) does not count.
        let mut old = WakeState::default();
        old.authors
            .insert("alice".into(), [T0 - HOUR, T0, T0 + 60, T0 + 120].into());
        assert_eq!(
            old.check(&l, "alice", "t", T0 + 600),
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
        assert_eq!(s.check(&off, "alice", "t4", now), None);
    }

    #[test]
    fn one_run_counts_once_per_author_and_thread() {
        let l = limits();
        let mut s = WakeState::default();
        s.record(&l, ["alice", "alice", "bob"], ["t1", "t1"], T0);
        assert_eq!(s.authors["alice"].len(), 1);
        assert_eq!(s.threads["t1"].streak, 1);
    }

    #[test]
    fn thread_backoff_grows_then_resets_after_quiet() {
        let l = WakeLimits {
            per_author_per_hour: 0,
            ..limits()
        };
        let mut s = WakeState::default();
        let mut now = T0;
        // Each run as soon as the thread allows it: 1, 2, 4… minutes.
        for expected in [60, 120, 240, 480, 960, 1920, 3600, 3600] {
            s.record(&l, ["alice"], ["t"], now);
            match s.check(&l, "alice", "t", now + 1) {
                Some(Hold::Thread { wait, until }) => {
                    assert_eq!(wait, expected);
                    assert_eq!(until, now + expected);
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(s.check(&l, "alice", "t", now + expected), None);
            assert_eq!(s.check(&l, "alice", "other", now + 1), None, "per thread");
            now += expected;
        }
        // Still the cap one second before an hour of quiet after the wait...
        s.record(&l, ["alice"], ["t"], now);
        now += 3600 + HOUR - 1;
        s.record(&l, ["alice"], ["t"], now);
        assert!(matches!(
            s.check(&l, "alice", "t", now + 1),
            Some(Hold::Thread { wait: 3600, .. })
        ));
        // ... and back to 1 minute after an hour of quiet.
        now += 3600 + HOUR;
        s.record(&l, ["alice"], ["t"], now);
        assert_eq!(
            s.check(&l, "alice", "t", now + 1),
            Some(Hold::Thread {
                wait: 60,
                until: now + 60
            })
        );
    }

    #[test]
    fn prune_forgets_what_no_longer_limits() {
        let l = limits();
        let mut s = WakeState::default();
        s.record(&l, ["alice"], ["t"], T0);
        s.prune(&l, T0 + HOUR - 1);
        assert_eq!((s.authors.len(), s.threads.len()), (1, 1));
        s.prune(&l, T0 + HOUR);
        assert_eq!((s.authors.len(), s.threads.len()), (0, 1));
        // The thread is kept for its wait (1 min) and an hour of quiet.
        s.prune(&l, T0 + 60 + HOUR - 1);
        assert_eq!(s.threads.len(), 1);
        s.prune(&l, T0 + 60 + HOUR);
        assert!(s.threads.is_empty());
    }

    #[test]
    fn unlimited_never_holds() {
        let l = WakeLimits::unlimited();
        let mut s = WakeState::default();
        for _ in 0..10 {
            s.record(&l, ["alice"], ["t"], T0);
        }
        assert_eq!(s.check(&l, "alice", "t", T0), None);
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(seconds(45), "45s");
        assert_eq!(seconds(120), "2m");
        assert_eq!(seconds(3600), "1h");
        assert_eq!(seconds(90), "90s");
    }
}
