//! The agent's spend budget (ADR 0011), enforced by the node: dollars per
//! UTC day and per UTC month, summed from the cost the agent CLI reports
//! (Claude Code: `total_cost_usd`). Runs that report no cost count against
//! a number of runs per day instead.
//!
//! The ledger, `<data dir>/agent/spend.jsonl`, has one line per run:
//! `{"ts": <start, unix seconds>, "usd": <cost or null>}`. It is kept for
//! two months, independently of transcript retention.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::runs::{civil, days_from_civil, format_time};

/// Budget limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub max_usd_per_day: f64,
    pub max_usd_per_month: f64,
    /// For runs whose cost is not reported.
    pub max_runs_per_day: usize,
}

impl Default for Budget {
    /// Conservative: a heartbeat run of a large model costs a few cents.
    fn default() -> Self {
        Budget {
            max_usd_per_day: 2.0,
            max_usd_per_month: 20.0,
            max_runs_per_day: 48,
        }
    }
}

/// What was spent in the current windows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spend {
    pub usd_today: f64,
    pub usd_month: f64,
    pub runs_today: usize,
    /// Runs today that reported no cost.
    pub uncosted_today: usize,
}

/// Whether a run may start.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Yes, with this much left for the run (the smaller of what is left
    /// today and this month).
    Go { remaining_usd: f64 },
    /// No: why, and when the window resets (unix seconds).
    Paused { why: String, until: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    ts: u64,
    usd: Option<f64>,
}

/// Two months of history.
const KEEP_SECS: u64 = 62 * 86_400;

pub fn ledger_path(work_dir: &Path) -> PathBuf {
    work_dir.join("spend.jsonl")
}

fn day_start(now: u64) -> u64 {
    now - now % 86_400
}

fn month_start(now: u64) -> u64 {
    let (y, m, ..) = civil(now);
    days_from_civil(y, m, 1) * 86_400
}

fn next_month(now: u64) -> u64 {
    let (y, m, ..) = civil(now);
    let (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    days_from_civil(y, m, 1) * 86_400
}

fn entries(work_dir: &Path) -> Vec<Entry> {
    let Ok(file) = std::fs::File::open(ledger_path(work_dir)) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Add a run to the ledger, dropping entries older than two months.
pub fn record(work_dir: &Path, ts: u64, usd: Option<f64>) -> std::io::Result<()> {
    let path = ledger_path(work_dir);
    let mut all = entries(work_dir);
    let cutoff = ts.saturating_sub(KEEP_SECS);
    if all.first().is_some_and(|e| e.ts < cutoff) {
        all.retain(|e| e.ts >= cutoff);
        let text: String = all
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .map(|l| l + "\n")
            .collect();
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(&Entry { ts, usd })?;
    writeln!(file, "{line}")
}

/// What was spent today and this month (UTC), as of `now`.
pub fn spend(work_dir: &Path, now: u64) -> Spend {
    let (today, month) = (day_start(now), month_start(now));
    let mut spend = Spend::default();
    for entry in entries(work_dir) {
        let usd = entry.usd.unwrap_or(0.0);
        if entry.ts >= month {
            spend.usd_month += usd;
        }
        if entry.ts >= today {
            spend.usd_today += usd;
            spend.runs_today += 1;
            if entry.usd.is_none() {
                spend.uncosted_today += 1;
            }
        }
    }
    spend
}

pub fn check(budget: &Budget, spend: &Spend, now: u64) -> Verdict {
    if spend.usd_month >= budget.max_usd_per_month {
        return Verdict::Paused {
            why: format!(
                "${:.2} spent this month, budget ${:.2}",
                spend.usd_month, budget.max_usd_per_month
            ),
            until: next_month(now),
        };
    }
    if spend.usd_today >= budget.max_usd_per_day {
        return Verdict::Paused {
            why: format!(
                "${:.2} spent today, budget ${:.2}",
                spend.usd_today, budget.max_usd_per_day
            ),
            until: day_start(now) + 86_400,
        };
    }
    if spend.uncosted_today >= budget.max_runs_per_day {
        return Verdict::Paused {
            why: format!(
                "{} runs today without a reported cost, budget {}",
                spend.uncosted_today, budget.max_runs_per_day
            ),
            until: day_start(now) + 86_400,
        };
    }
    Verdict::Go {
        remaining_usd: (budget.max_usd_per_day - spend.usd_today)
            .min(budget.max_usd_per_month - spend.usd_month),
    }
}

/// `$0.23 of $2.00 today, $5.10 of $20.00 this month`.
pub fn describe(budget: &Budget, spend: &Spend) -> String {
    format!(
        "${:.2} of ${:.2} today, ${:.2} of ${:.2} this month",
        spend.usd_today, budget.max_usd_per_day, spend.usd_month, budget.max_usd_per_month
    )
}

/// `until 2026-10-17 00:00:00Z`.
pub fn until(ts: u64) -> String {
    format!("until {}", format_time(ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-16 12:00:00Z
    const NOW: u64 = 1_792_152_000;

    #[test]
    fn windows() {
        assert_eq!(format_time(day_start(NOW)), "2026-10-16 00:00:00Z");
        assert_eq!(format_time(month_start(NOW)), "2026-10-01 00:00:00Z");
        assert_eq!(format_time(next_month(NOW)), "2026-11-01 00:00:00Z");
        let december = NOW + 60 * 86_400;
        assert_eq!(format_time(next_month(december)), "2027-01-01 00:00:00Z");
    }

    #[test]
    fn spend_and_pauses() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget {
            max_usd_per_day: 1.0,
            max_usd_per_month: 3.0,
            max_runs_per_day: 2,
        };
        // Last month, earlier this month, today.
        record(dir.path(), NOW - 20 * 86_400, Some(5.0)).unwrap();
        record(dir.path(), NOW - 5 * 86_400, Some(1.5)).unwrap();
        record(dir.path(), NOW - 60, Some(0.4)).unwrap();
        let s = spend(dir.path(), NOW);
        assert!((s.usd_today - 0.4).abs() < 1e-9);
        assert!((s.usd_month - 1.9).abs() < 1e-9);
        match check(&budget, &s, NOW) {
            Verdict::Go { remaining_usd } => assert!((remaining_usd - 0.6).abs() < 1e-9),
            paused => panic!("{paused:?}"),
        }

        record(dir.path(), NOW - 30, Some(0.7)).unwrap();
        let s = spend(dir.path(), NOW);
        match check(&budget, &s, NOW) {
            Verdict::Paused { until, .. } => {
                assert_eq!(format_time(until), "2026-10-17 00:00:00Z")
            }
            go => panic!("{go:?}"),
        }
        // Tomorrow, the month budget still has room...
        let tomorrow = NOW + 86_400;
        assert!(matches!(
            check(&budget, &spend(dir.path(), tomorrow), tomorrow),
            Verdict::Go { .. }
        ));
        // ... until it does not.
        record(dir.path(), tomorrow, Some(0.5)).unwrap();
        match check(&budget, &spend(dir.path(), tomorrow), tomorrow) {
            Verdict::Paused { until, why } => {
                assert_eq!(format_time(until), "2026-11-01 00:00:00Z", "{why}")
            }
            go => panic!("{go:?}"),
        }
    }

    #[test]
    fn runs_without_cost_count_instead() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget {
            max_runs_per_day: 2,
            ..Budget::default()
        };
        record(dir.path(), NOW - 10, None).unwrap();
        assert!(matches!(
            check(&budget, &spend(dir.path(), NOW), NOW),
            Verdict::Go { .. }
        ));
        record(dir.path(), NOW - 5, None).unwrap();
        assert!(matches!(
            check(&budget, &spend(dir.path(), NOW), NOW),
            Verdict::Paused { .. }
        ));
    }
}
