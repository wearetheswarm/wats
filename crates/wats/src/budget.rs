//! The agent's budget (ADR 0011, ADR 0032), enforced by the node before
//! each run, per UTC day and per UTC month:
//!
//! - **dollars**, summed from the cost the agent CLI reports (Claude Code:
//!   `total_cost_usd`);
//! - **minutes**, the wall time of the runs, measured by the node (every
//!   runtime);
//! - **LLM tokens**, as the agent CLI reports them (Claude Code and Codex;
//!   [`reports_tokens`]).
//!
//! Each monthly limit is 30 × its daily one ([`MONTH_DAYS`]) unless
//! config.toml sets it, and 0 is no limit, for the day and the month alike.
//! A run in progress keeps its own timeout: a budget stops the next runs.
//! Runs that report no cost also count against a number of runs per day.
//!
//! The ledger, `<data dir>/agent/spend.jsonl`, has one line per run:
//! `{"ts": <start, unix seconds>, "usd": <cost or null>, "secs": <wall
//! time>, "tokens": <LLM tokens>}` (`secs` and `tokens` since 0.4.4: an
//! older line counts as 0 of each). It is kept for two months,
//! independently of transcript retention.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::runs::{civil, days_from_civil, format_time};
use crate::settings::AgentSettings;

/// A monthly limit not set is this many times the daily one.
pub const MONTH_DAYS: u64 = 30;

/// `agent.max_usd_per_day` when unset: conservative, a heartbeat run of a
/// large model costs a few cents.
pub const DEFAULT_USD_PER_DAY: f64 = 2.0;

/// `agent.max_runs_per_day` when unset.
pub const DEFAULT_RUNS_PER_DAY: usize = 48;

/// Budget limits, `None` for no limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub max_usd_per_day: Option<f64>,
    pub max_usd_per_month: Option<f64>,
    pub max_minutes_per_day: Option<u64>,
    pub max_minutes_per_month: Option<u64>,
    pub max_tokens_per_day: Option<u64>,
    pub max_tokens_per_month: Option<u64>,
    /// For runs whose cost is not reported.
    pub max_runs_per_day: usize,
}

impl Default for Budget {
    /// The defaults of config.toml: $2 a day ($60 a month), no time nor
    /// LLM token limit, 48 runs a day without a reported cost.
    fn default() -> Self {
        Budget::of(&AgentSettings::default())
    }
}

/// 0 (or less, or not a number) is no limit.
fn limit_f(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

fn limit_n(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

impl Budget {
    /// The budget of an agent's table in config.toml: each key, else its
    /// default; each month, 30 × the day unless set.
    pub fn of(file: &AgentSettings) -> Budget {
        let usd = file.max_usd_per_day.unwrap_or(DEFAULT_USD_PER_DAY);
        let minutes = file.max_minutes_per_day.unwrap_or(0);
        let tokens = file.max_tokens_per_day.unwrap_or(0);
        Budget {
            max_usd_per_day: limit_f(usd),
            max_usd_per_month: limit_f(file.max_usd_per_month.unwrap_or(usd * MONTH_DAYS as f64)),
            max_minutes_per_day: limit_n(minutes),
            max_minutes_per_month: limit_n(
                file.max_minutes_per_month
                    .unwrap_or(minutes.saturating_mul(MONTH_DAYS)),
            ),
            max_tokens_per_day: limit_n(tokens),
            max_tokens_per_month: limit_n(
                file.max_tokens_per_month
                    .unwrap_or(tokens.saturating_mul(MONTH_DAYS)),
            ),
            max_runs_per_day: file.max_runs_per_day.unwrap_or(DEFAULT_RUNS_PER_DAY),
        }
    }

    /// Without the limits `runtime` cannot be held to: dollars for one
    /// that reports no cost, LLM tokens for one that reports none.
    pub fn for_runtime(mut self, runtime: &str) -> Budget {
        if !reports_cost(runtime) {
            self.max_usd_per_day = None;
            self.max_usd_per_month = None;
        }
        if !reports_tokens(runtime) {
            self.max_tokens_per_day = None;
            self.max_tokens_per_month = None;
        }
        self
    }
}

/// Whether the agent CLI of `runtime` reports a run's cost: Claude Code
/// (`total_cost_usd`). Codex and OpenClaw do not.
pub fn reports_cost(runtime: &str) -> bool {
    !matches!(runtime, "codex" | "openclaw")
}

/// Whether the agent CLI of `runtime` reports a run's LLM tokens: Claude
/// Code (the `usage` of its `result`) and Codex (the `usage` of each
/// `turn.completed`). OpenClaw's gateway returns none.
pub fn reports_tokens(runtime: &str) -> bool {
    runtime != "openclaw"
}

/// What a run used, for the ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// The cost the agent CLI reported, if any.
    pub usd: Option<f64>,
    /// Its wall time.
    pub secs: Option<f64>,
    /// The LLM tokens the agent CLI reported, if any.
    pub tokens: Option<u64>,
}

/// What was spent in the current windows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spend {
    pub usd_today: f64,
    pub usd_month: f64,
    /// Wall time of the runs.
    pub secs_today: f64,
    pub secs_month: f64,
    pub tokens_today: u64,
    pub tokens_month: u64,
    pub runs_today: usize,
    /// Runs today that reported no cost.
    pub uncosted_today: usize,
}

impl Spend {
    pub fn minutes_today(&self) -> f64 {
        self.secs_today / 60.0
    }

    pub fn minutes_month(&self) -> f64 {
        self.secs_month / 60.0
    }
}

/// Whether a run may start.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Yes, with this much left for the run (the smaller of what is left
    /// today and this month), `None` without a dollar limit.
    Go { remaining_usd: Option<f64> },
    /// No: why, and when the window resets (unix seconds).
    Paused { why: String, until: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    ts: u64,
    usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<u64>,
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
pub fn record(work_dir: &Path, ts: u64, usage: Usage) -> std::io::Result<()> {
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
    let line = serde_json::to_string(&Entry {
        ts,
        usd: usage.usd,
        secs: usage.secs.filter(|s| s.is_finite() && *s >= 0.0),
        tokens: usage.tokens,
    })?;
    writeln!(file, "{line}")
}

/// What was spent today and this month (UTC), as of `now`.
pub fn spend(work_dir: &Path, now: u64) -> Spend {
    let (today, month) = (day_start(now), month_start(now));
    let mut spend = Spend::default();
    for entry in entries(work_dir) {
        let usd = entry.usd.unwrap_or(0.0);
        let secs = entry.secs.unwrap_or(0.0);
        let tokens = entry.tokens.unwrap_or(0);
        if entry.ts >= month {
            spend.usd_month += usd;
            spend.secs_month += secs;
            spend.tokens_month = spend.tokens_month.saturating_add(tokens);
        }
        if entry.ts >= today {
            spend.usd_today += usd;
            spend.secs_today += secs;
            spend.tokens_today = spend.tokens_today.saturating_add(tokens);
            spend.runs_today += 1;
            if entry.usd.is_none() {
                spend.uncosted_today += 1;
            }
        }
    }
    spend
}

pub fn check(budget: &Budget, spend: &Spend, now: u64) -> Verdict {
    let tomorrow = day_start(now) + 86_400;
    let month_end = next_month(now);
    let paused = |why: String, until: u64| Verdict::Paused { why, until };
    if let Some(max) = budget.max_usd_per_month
        && spend.usd_month >= max
    {
        return paused(
            format!("${:.2} spent this month, budget ${max:.2}", spend.usd_month),
            month_end,
        );
    }
    if let Some(max) = budget.max_usd_per_day
        && spend.usd_today >= max
    {
        return paused(
            format!("${:.2} spent today, budget ${max:.2}", spend.usd_today),
            tomorrow,
        );
    }
    if let Some(max) = budget.max_minutes_per_month
        && spend.minutes_month() >= max as f64
    {
        return paused(
            format!(
                "{} min of agent runs this month, budget {max} min",
                minutes(spend.minutes_month())
            ),
            month_end,
        );
    }
    if let Some(max) = budget.max_minutes_per_day
        && spend.minutes_today() >= max as f64
    {
        return paused(
            format!(
                "{} min of agent runs today, budget {max} min",
                minutes(spend.minutes_today())
            ),
            tomorrow,
        );
    }
    if let Some(max) = budget.max_tokens_per_month
        && spend.tokens_month >= max
    {
        return paused(
            format!(
                "{} LLM tokens this month, budget {}",
                tokens(spend.tokens_month),
                tokens(max)
            ),
            month_end,
        );
    }
    if let Some(max) = budget.max_tokens_per_day
        && spend.tokens_today >= max
    {
        return paused(
            format!(
                "{} LLM tokens today, budget {}",
                tokens(spend.tokens_today),
                tokens(max)
            ),
            tomorrow,
        );
    }
    if spend.uncosted_today >= budget.max_runs_per_day {
        return paused(
            format!(
                "{} runs today without a reported cost, budget {}",
                spend.uncosted_today, budget.max_runs_per_day
            ),
            tomorrow,
        );
    }
    let left = [
        budget.max_usd_per_day.map(|max| max - spend.usd_today),
        budget.max_usd_per_month.map(|max| max - spend.usd_month),
    ];
    Verdict::Go {
        remaining_usd: left.into_iter().flatten().reduce(f64::min),
    }
}

/// Whole minutes, rounded down: `12`.
fn minutes(m: f64) -> String {
    format!("{}", m.max(0.0).floor() as u64)
}

/// An LLM token count, short: `950`, `12.3k`, `1.5M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => trim_zero(format!("{:.1}", n as f64 / 1e3)) + "k",
        _ => trim_zero(format!("{:.1}", n as f64 / 1e6)) + "M",
    }
}

fn trim_zero(s: String) -> String {
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// One metric: `12 of 120 min today, 300 of 3600 this month`, `12 min
/// today` without a limit.
fn metric(
    today: String,
    day: Option<String>,
    month_used: String,
    month: Option<String>,
    unit: &str,
) -> String {
    let mut text = match &day {
        Some(day) => format!("{today} of {day}{unit} today"),
        None => format!("{today}{unit} today"),
    };
    match month {
        Some(month) => text.push_str(&format!(", {month_used} of {month} this month")),
        None if day.is_none() => text.push_str(" (no limit)"),
        None => {}
    }
    text
}

/// The budget of an agent of `runtime` and what it spent: `$0.23 of $2.00
/// today, $5.10 of $60.00 this month · 12 min today (no limit) · 34.5k
/// LLM tokens today (no limit)`. Runtimes without a cost show their runs
/// instead of dollars, and those without tokens no tokens.
pub fn describe(budget: &Budget, spend: &Spend, runtime: &str) -> String {
    let mut parts = Vec::new();
    match runtime {
        "openclaw" => parts.push(format!(
            "{} ({} of {} runs today)",
            crate::openclaw::BUDGET,
            spend.uncosted_today,
            budget.max_runs_per_day
        )),
        _ if !reports_cost(runtime) => parts.push(format!(
            "{} of {} runs today ({} reports no cost)",
            spend.uncosted_today,
            budget.max_runs_per_day,
            runtime_name(runtime)
        )),
        _ => parts.push(metric(
            format!("${:.2}", spend.usd_today),
            budget.max_usd_per_day.map(|m| format!("${m:.2}")),
            format!("${:.2}", spend.usd_month),
            budget.max_usd_per_month.map(|m| format!("${m:.2}")),
            "",
        )),
    }
    parts.push(metric(
        minutes(spend.minutes_today()),
        budget.max_minutes_per_day.map(|m| m.to_string()),
        minutes(spend.minutes_month()),
        budget.max_minutes_per_month.map(|m| m.to_string()),
        " min",
    ));
    if reports_tokens(runtime) {
        parts.push(metric(
            tokens(spend.tokens_today),
            budget.max_tokens_per_day.map(tokens),
            tokens(spend.tokens_month),
            budget.max_tokens_per_month.map(tokens),
            " LLM tokens",
        ));
    }
    parts.join(" · ")
}

/// For `status` (CLI and MCP): what an agent of `runtime` used today and
/// this month, and its limits (`null`: no limit, or not reported by the
/// runtime).
pub fn status_json(budget: &Budget, spend: &Spend, runtime: &str) -> Value {
    let cost = reports_cost(runtime);
    let toks = reports_tokens(runtime);
    let round = |x: f64| (x * 100.0).round() / 100.0;
    json!({
        "today": {
            "usd": cost.then(|| round(spend.usd_today)),
            "minutes": round(spend.minutes_today()),
            "tokens": toks.then_some(spend.tokens_today),
            "runs": spend.runs_today,
            "runs_without_cost": spend.uncosted_today,
        },
        "month": {
            "usd": cost.then(|| round(spend.usd_month)),
            "minutes": round(spend.minutes_month()),
            "tokens": toks.then_some(spend.tokens_month),
        },
        "limits": {
            "usd_per_day": budget.max_usd_per_day,
            "usd_per_month": budget.max_usd_per_month,
            "minutes_per_day": budget.max_minutes_per_day,
            "minutes_per_month": budget.max_minutes_per_month,
            "tokens_per_day": budget.max_tokens_per_day,
            "tokens_per_month": budget.max_tokens_per_month,
            "runs_without_cost_per_day": budget.max_runs_per_day,
        },
    })
}

fn runtime_name(runtime: &str) -> &str {
    match runtime {
        "codex" => "Codex",
        "claude" => "Claude Code",
        "openclaw" => "OpenClaw",
        other => other,
    }
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

    fn usd(x: f64) -> Usage {
        Usage {
            usd: Some(x),
            ..Usage::default()
        }
    }

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
            max_usd_per_day: Some(1.0),
            max_usd_per_month: Some(3.0),
            max_runs_per_day: 2,
            ..Budget::default()
        };
        // Last month, earlier this month, today.
        record(dir.path(), NOW - 20 * 86_400, usd(5.0)).unwrap();
        record(dir.path(), NOW - 5 * 86_400, usd(1.5)).unwrap();
        record(dir.path(), NOW - 60, usd(0.4)).unwrap();
        let s = spend(dir.path(), NOW);
        assert!((s.usd_today - 0.4).abs() < 1e-9);
        assert!((s.usd_month - 1.9).abs() < 1e-9);
        match check(&budget, &s, NOW) {
            Verdict::Go {
                remaining_usd: Some(remaining),
            } => assert!((remaining - 0.6).abs() < 1e-9),
            paused => panic!("{paused:?}"),
        }

        record(dir.path(), NOW - 30, usd(0.7)).unwrap();
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
        record(dir.path(), tomorrow, usd(0.5)).unwrap();
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
        record(dir.path(), NOW - 10, Usage::default()).unwrap();
        assert!(matches!(
            check(&budget, &spend(dir.path(), NOW), NOW),
            Verdict::Go { .. }
        ));
        record(dir.path(), NOW - 5, Usage::default()).unwrap();
        assert!(matches!(
            check(&budget, &spend(dir.path(), NOW), NOW),
            Verdict::Paused { .. }
        ));
    }

    /// Each month is 30 × its day unless config.toml sets it; 0 is no
    /// limit, for the day and the month.
    #[test]
    fn the_month_is_thirty_days_unless_set() {
        let file = AgentSettings {
            max_usd_per_day: Some(5.0),
            max_minutes_per_day: Some(120),
            max_tokens_per_day: Some(1_000_000),
            ..AgentSettings::default()
        };
        let b = Budget::of(&file);
        assert_eq!(b.max_usd_per_month, Some(150.0));
        assert_eq!(b.max_minutes_per_month, Some(3600));
        assert_eq!(b.max_tokens_per_month, Some(30_000_000));
        // The defaults: $2 a day, so $60 a month; no time nor token limit.
        let d = Budget::default();
        assert_eq!(d.max_usd_per_day, Some(2.0));
        assert_eq!(d.max_usd_per_month, Some(60.0));
        assert_eq!(d.max_minutes_per_day, None);
        assert_eq!(d.max_minutes_per_month, None);
        assert_eq!(d.max_tokens_per_day, None);
        assert_eq!(d.max_runs_per_day, 48);
        // An explicit month overrides, the old `max_usd_per_month` too.
        let set = AgentSettings {
            max_usd_per_month: Some(20.0),
            max_minutes_per_day: Some(60),
            max_minutes_per_month: Some(600),
            ..AgentSettings::default()
        };
        let b = Budget::of(&set);
        assert_eq!(b.max_usd_per_day, Some(2.0));
        assert_eq!(b.max_usd_per_month, Some(20.0));
        assert_eq!(b.max_minutes_per_month, Some(600));
        // 0 is no limit: a day of 0 makes the derived month 0 too, an
        // explicit month still applies, and a month of 0 is no limit.
        let zero = AgentSettings {
            max_usd_per_day: Some(0.0),
            max_minutes_per_day: Some(0),
            max_minutes_per_month: Some(900),
            max_tokens_per_day: Some(1000),
            max_tokens_per_month: Some(0),
            ..AgentSettings::default()
        };
        let b = Budget::of(&zero);
        assert_eq!((b.max_usd_per_day, b.max_usd_per_month), (None, None));
        assert_eq!(
            (b.max_minutes_per_day, b.max_minutes_per_month),
            (None, Some(900))
        );
        assert_eq!(
            (b.max_tokens_per_day, b.max_tokens_per_month),
            (Some(1000), None)
        );
    }

    /// No limit at all: every run goes, with no dollar cap to pass on,
    /// whatever was spent.
    #[test]
    fn zero_is_no_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = AgentSettings {
            max_usd_per_day: Some(0.0),
            ..AgentSettings::default()
        };
        let budget = Budget::of(&file);
        for _ in 0..5 {
            record(
                dir.path(),
                NOW - 10,
                Usage {
                    usd: Some(100.0),
                    secs: Some(3600.0),
                    tokens: Some(50_000_000),
                },
            )
            .unwrap();
        }
        assert_eq!(
            check(&budget, &spend(dir.path(), NOW), NOW),
            Verdict::Go {
                remaining_usd: None
            }
        );
        let said = describe(&budget, &spend(dir.path(), NOW), "claude");
        assert_eq!(
            said,
            "$500.00 today (no limit) · 300 min today (no limit) · 250M LLM tokens today \
             (no limit)"
        );
    }

    /// Minutes: the wall time of the runs; once the day's are used, no new
    /// run until tomorrow, then the month's.
    #[test]
    fn minutes_stop_new_runs() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget::of(&AgentSettings {
            max_usd_per_day: Some(0.0),
            max_minutes_per_day: Some(30),
            max_minutes_per_month: Some(50),
            ..AgentSettings::default()
        });
        let run = |secs: f64| Usage {
            usd: Some(0.01),
            secs: Some(secs),
            tokens: None,
        };
        record(dir.path(), NOW - 3600, run(20.0 * 60.0)).unwrap();
        let s = spend(dir.path(), NOW);
        assert!((s.minutes_today() - 20.0).abs() < 1e-9);
        assert_eq!(
            check(&budget, &s, NOW),
            Verdict::Go {
                remaining_usd: None
            }
        );
        assert_eq!(
            describe(&budget, &s, "claude"),
            "$0.01 today (no limit) · 20 of 30 min today, 20 of 50 this month · 0 LLM tokens \
             today (no limit)"
        );
        record(dir.path(), NOW - 60, run(10.5 * 60.0)).unwrap();
        match check(&budget, &spend(dir.path(), NOW), NOW) {
            Verdict::Paused { why, until } => {
                assert_eq!(why, "30 min of agent runs today, budget 30 min");
                assert_eq!(format_time(until), "2026-10-17 00:00:00Z");
            }
            go => panic!("{go:?}"),
        }
        // Tomorrow: 30.5 of the month's 50 minutes, so it goes; 20 more
        // and the month is used.
        let tomorrow = NOW + 86_400;
        assert!(matches!(
            check(&budget, &spend(dir.path(), tomorrow), tomorrow),
            Verdict::Go { .. }
        ));
        record(dir.path(), tomorrow, run(20.0 * 60.0)).unwrap();
        match check(&budget, &spend(dir.path(), tomorrow), tomorrow) {
            Verdict::Paused { why, until } => {
                assert_eq!(why, "50 min of agent runs this month, budget 50 min");
                assert_eq!(format_time(until), "2026-11-01 00:00:00Z");
            }
            go => panic!("{go:?}"),
        }
    }

    /// LLM tokens: enforced for the runtimes that report them; OpenClaw's
    /// limits are dropped (it reports none), and so are Codex's dollars.
    #[test]
    fn tokens_where_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = AgentSettings {
            max_tokens_per_day: Some(10_000),
            ..AgentSettings::default()
        };
        record(
            dir.path(),
            NOW - 10,
            Usage {
                usd: None,
                secs: Some(30.0),
                tokens: Some(12_345),
            },
        )
        .unwrap();
        let s = spend(dir.path(), NOW);
        for runtime in ["claude", "codex"] {
            let budget = Budget::of(&file).for_runtime(runtime);
            match check(&budget, &s, NOW) {
                Verdict::Paused { why, .. } => {
                    assert_eq!(why, "12.3k LLM tokens today, budget 10k", "{runtime}")
                }
                go => panic!("{runtime}: {go:?}"),
            }
        }
        let codex = Budget::of(&file).for_runtime("codex");
        assert_eq!(
            (codex.max_usd_per_day, codex.max_usd_per_month),
            (None, None)
        );
        assert_eq!(
            describe(&codex, &s, "codex"),
            "1 of 48 runs today (Codex reports no cost) · 0 min today (no limit) · 12.3k of \
             10k LLM tokens today, 12.3k of 300k this month"
        );
        let openclaw = Budget::of(&file).for_runtime("openclaw");
        assert_eq!(openclaw.max_tokens_per_day, None);
        assert!(matches!(check(&openclaw, &s, NOW), Verdict::Go { .. }));
        assert_eq!(
            describe(&openclaw, &s, "openclaw"),
            "managed by OpenClaw (1 of 48 runs today) · 0 min today (no limit)"
        );
        let json = status_json(&openclaw, &s, "openclaw");
        assert_eq!(json["today"]["tokens"], Value::Null);
        assert_eq!(json["today"]["minutes"], 0.5);
        let json = status_json(&codex, &s, "codex");
        assert_eq!(json["today"]["tokens"], 12_345);
        assert_eq!(json["limits"]["tokens_per_month"], 300_000);
    }

    /// A ledger line of 0.4.3 or older (no `secs`, no `tokens`) counts as
    /// none of them; a line of today reads back in an older wats (it
    /// ignores the new fields).
    #[test]
    fn older_ledger_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            ledger_path(dir.path()),
            format!("{{\"ts\":{},\"usd\":0.5}}\n", NOW - 10),
        )
        .unwrap();
        let s = spend(dir.path(), NOW);
        assert_eq!((s.secs_today, s.tokens_today, s.runs_today), (0.0, 0, 1));
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Old {
            ts: u64,
            usd: Option<f64>,
        }
        record(
            dir.path(),
            NOW,
            Usage {
                usd: Some(0.1),
                secs: Some(5.0),
                tokens: Some(7),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(ledger_path(dir.path())).unwrap();
        let last = text.lines().last().unwrap();
        assert_eq!(
            last,
            format!("{{\"ts\":{NOW},\"usd\":0.1,\"secs\":5.0,\"tokens\":7}}")
        );
        serde_json::from_str::<Old>(last).unwrap();
    }

    #[test]
    fn token_counts_read_short() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(1000), "1k");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(1_500_000), "1.5M");
        assert_eq!(tokens(2_000_000), "2M");
    }
}
