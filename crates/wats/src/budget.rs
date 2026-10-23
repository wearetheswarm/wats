//! The agent's budget (ADR 0011, ADR 0034), enforced by the node before
//! each run, per UTC day and per UTC month:
//!
//! - **dollars**, summed from the cost the agent CLI reports (Claude Code:
//!   `total_cost_usd`);
//! - **minutes**, the wall time of the runs, measured by the node (every
//!   runtime);
//! - **LLM tokens**, as the agent CLI reports them (Claude Code, Codex,
//!   OpenCode, Mistral Vibe; [`reports_tokens`]).
//!
//! Each monthly limit is 30 × its daily one ([`MONTH_DAYS`]) unless
//! config.toml sets it, and 0 is no limit, for the day and the month alike.
//! A run in progress keeps its own timeout: a budget stops the next runs.
//! Runs that report no cost also count against a number of runs per day.
//!
//! The ledger, `<data dir>/agent/spend.jsonl`, has one line per run:
//! `{"ts": <start, unix seconds>, "usd": <cost or null>, "secs": <wall
//! time>, "tokens": <LLM tokens>}` (`secs` and `tokens` since 0.4.5: an
//! older line counts as 0 of each). It is kept for two months,
//! independently of transcript retention.

use std::io::Write;
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
    /// config.toml sets `max_usd_per_day = 0` (`.0`) or
    /// `max_usd_per_month = 0` (`.1`), which paused the agent before
    /// 0.4.5, and the data directory's setup has not been confirmed since
    /// (its revision is below [`ZERO_CONFIRMED_REVISION`]): the agent stays
    /// paused ([`Budget::legacy_zero`], ADR 0034 §5).
    pub legacy_zero: (bool, bool),
}

/// The setup revision from which a USD budget of 0 means no limit: the
/// one whose budget question settles it (ADR 0034 §5).
pub const ZERO_CONFIRMED_REVISION: u32 = 2;

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
            legacy_zero: (false, false),
        }
    }

    /// [`Budget::of`], and the pause of a USD budget of 0 written before
    /// 0.4.5 (when it meant paused): `revision` is the data directory's
    /// setup revision ([`crate::firstrun::stored_revision`]), `work_dir`
    /// the agent's directory, whose [`CONFIRMED_FILE`] says the budget
    /// question confirmed this agent's budget.
    pub fn of_dir(file: &AgentSettings, revision: Option<u32>, work_dir: &Path) -> Budget {
        Budget {
            legacy_zero: legacy_zero(file, revision, work_dir),
            ..Budget::of(file)
        }
    }

    /// Whether a USD budget of 0 written before 0.4.5 keeps the agent
    /// paused.
    pub fn paused_by_zero(&self) -> bool {
        self.legacy_zero.0 || self.legacy_zero.1
    }

    /// The keys that keep it paused: `max_usd_per_day = 0`, …
    fn zero_keys(&self) -> String {
        let mut keys = Vec::new();
        if self.legacy_zero.0 {
            keys.push("max_usd_per_day = 0");
        }
        if self.legacy_zero.1 {
            keys.push("max_usd_per_month = 0");
        }
        keys.join(" and ")
    }

    /// The one-line notice of an agent paused by a USD budget of 0 from
    /// before 0.4.5 (ADR 0034 §5), for `wats status`, the node's start
    /// and the MCP `status` tool.
    pub fn zero_notice(&self, agent: &str) -> Option<String> {
        self.paused_by_zero().then(|| {
            let keys = self.zero_keys();
            let one = !(self.legacy_zero.0 && self.legacy_zero.1);
            format!(
                "agent {agent}: {keys} meant paused before 0.4.5 and now {} no limit; it stays \
                 paused until you run `wats setup` (or set agents.{agent}.enabled = false to \
                 keep it off)",
                if one { "means" } else { "mean" }
            )
        })
    }

    /// Without the limits `runtime` cannot be held to: dollars for one
    /// that reports no cost, LLM tokens for one that reports none.
    /// A USD budget of 0 from before 0.4.5 still pauses Codex, which it
    /// paused then (no cost reported, so nothing ever reached 0); not
    /// OpenClaw, whose USD budgets never applied.
    pub fn for_runtime(mut self, runtime: &str) -> Budget {
        if !reports_cost(runtime) {
            self.max_usd_per_day = None;
            self.max_usd_per_month = None;
        }
        if runtime == "openclaw" {
            self.legacy_zero = (false, false);
        }
        if !reports_tokens(runtime) {
            self.max_tokens_per_day = None;
            self.max_tokens_per_month = None;
        }
        self
    }
}

/// Whether `file` sets a USD budget of 0 (day, month) that, with the data
/// directory at setup `revision`, still means paused: written before 0.4.5
/// and not confirmed since, neither for this agent (its `work_dir` has no
/// [`CONFIRMED_FILE`]) nor for the whole directory (revision 2, recorded
/// once every agent's is). No config.toml (`None`) sets nothing.
pub fn legacy_zero(file: &AgentSettings, revision: Option<u32>, work_dir: &Path) -> (bool, bool) {
    let old = revision.is_some_and(|r| r < ZERO_CONFIRMED_REVISION) && !confirmed(work_dir);
    let zero = |v: Option<f64>| old && v.is_some_and(|x| x <= 0.0);
    (zero(file.max_usd_per_day), zero(file.max_usd_per_month))
}

/// The marker, in an agent's directory, that the budget question confirmed
/// its budget (ADR 0034 §5): a USD budget of 0 then means no limit.
pub const CONFIRMED_FILE: &str = "budget-confirmed";

/// Whether the budget question confirmed the budget of the agent of
/// `work_dir`.
pub fn confirmed(work_dir: &Path) -> bool {
    work_dir.join(CONFIRMED_FILE).exists()
}

/// Record that the budget question confirmed the budget of the agent of
/// `work_dir`.
pub fn confirm(work_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(work_dir)?;
    std::fs::write(work_dir.join(CONFIRMED_FILE), "confirmed by wats setup\n")
}

/// Whether the node holds `runtime` to a dollar budget, from the cost
/// its CLI reports: Claude Code (`total_cost_usd`), OpenCode (the `cost`
/// of each `step_finish`, ADR 0031) and Mistral Vibe (its export's
/// `cost_usd`, ADR 0033), these two estimates while their
/// `ENFORCE_USD_BUDGET` is on. Codex and OpenClaw report none.
pub fn reports_cost(runtime: &str) -> bool {
    match runtime {
        "codex" | "openclaw" | "copilot" | "antigravity" => false,
        "opencode" => crate::opencode::ENFORCE_USD_BUDGET,
        "vibe" => crate::vibe::ENFORCE_USD_BUDGET,
        _ => true,
    }
}

/// Whether the cost `runtime` reports is its own estimate from the
/// model's catalog prices, not the bill: OpenCode and Mistral Vibe.
pub fn estimates_cost(runtime: &str) -> bool {
    matches!(runtime, "opencode" | "vibe")
}

/// Whether the agent CLI of `runtime` reports a run's LLM tokens: Claude
/// Code (the `usage` of its `result`), OpenCode (the `tokens` of each
/// `step_finish`), Mistral Vibe (its export's `usage`; not a run killed
/// before it) and Codex (the `usage` of each
/// `turn.completed`). OpenClaw's gateway returns none.
pub fn reports_tokens(runtime: &str) -> bool {
    !matches!(runtime, "openclaw" | "copilot")
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
    /// today and this month): dollars (`None` without a dollar limit), and
    /// run time in seconds (`None` without a time limit; at least a
    /// minute), which caps the run's timeout; LLM tokens (`None` without
    /// a token limit), which only Vibe can be capped by (`--max-tokens`).
    Go {
        remaining_usd: Option<f64>,
        remaining_secs: Option<f64>,
        remaining_tokens: Option<u64>,
    },
    /// No: why, and when the window resets (unix seconds).
    Paused { why: String, until: u64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    ts: u64,
    usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<u64>,
    /// The run's id, on its provisional entry and the one replacing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    /// Written when the run starts ([`begin`]), replaced when it ends
    /// ([`finish`]); one left at a node's start is from a run that never
    /// ended (a crash, SIGKILL), and [`settle`] charges it its caps. Not
    /// counted while provisional.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    provisional: bool,
    /// The caps a provisional entry is charged if its run never ends: the
    /// dollars it was allowed, its timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cap_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cap_secs: Option<f64>,
    /// Settled by [`settle`]: charged its caps, its run never ended.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unfinished: bool,
}

/// A line of the ledger: an entry, or a line this wats cannot read, kept
/// as it is.
enum Line {
    Entry(Entry),
    Raw(String),
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

/// Every line of the ledger, in order; an unreadable one (a torn write,
/// a newer format) is kept as it is, never dropped.
fn lines(work_dir: &Path) -> Vec<Line> {
    let Ok(bytes) = std::fs::read(ledger_path(work_dir)) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match serde_json::from_str::<Entry>(l) {
            Ok(entry) => Line::Entry(entry),
            Err(_) => Line::Raw(l.to_string()),
        })
        .collect()
}

fn entries(work_dir: &Path) -> Vec<Entry> {
    lines(work_dir)
        .into_iter()
        .filter_map(|l| match l {
            Line::Entry(e) => Some(e),
            Line::Raw(_) => None,
        })
        .collect()
}

/// Write the whole ledger again, atomically (a temporary file renamed).
fn rewrite(work_dir: &Path, lines: &[Line]) -> std::io::Result<()> {
    let path = ledger_path(work_dir);
    let mut text = String::new();
    for line in lines {
        match line {
            Line::Entry(e) => text.push_str(&serde_json::to_string(e)?),
            Line::Raw(raw) => text.push_str(raw),
        }
        text.push('\n');
    }
    // On disk before the rename, and the rename itself made durable where
    // that is cheap (unix): a power loss leaves the old ledger or the new
    // one, never an empty one.
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Append one entry, as one write with its newline; after a newline first
/// when the file does not end with one (a torn line stays a line apart).
fn append(work_dir: &Path, entry: &Entry) -> std::io::Result<()> {
    std::fs::create_dir_all(work_dir)?;
    let path = ledger_path(work_dir);
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    let ends_cleanly = match std::fs::read(&path) {
        Ok(bytes) => bytes.last().is_none_or(|b| *b == b'\n'),
        Err(_) => true,
    };
    if !ends_cleanly {
        line.insert(0, '\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())
}

/// Add a run to the ledger, dropping entries older than two months (lines
/// it cannot read are kept).
pub fn record(work_dir: &Path, ts: u64, usage: Usage) -> std::io::Result<()> {
    finish(work_dir, ts, None, usage)
}

/// A run starts: its provisional entry, charged `cap` (the dollars it is
/// allowed, its timeout) if it never ends ([`settle`]). Not counted
/// meanwhile.
pub fn begin(work_dir: &Path, ts: u64, run: &str, cap: Usage) -> std::io::Result<()> {
    append(
        work_dir,
        &Entry {
            ts,
            run: Some(run.to_string()),
            provisional: true,
            cap_usd: cap.usd.filter(|x| x.is_finite() && *x > 0.0),
            cap_secs: cap.secs.filter(|s| s.is_finite() && *s >= 0.0),
            ..Entry::default()
        },
    )
}

/// A run ended: its entry replaces its provisional one (`run`), and
/// entries older than two months are dropped.
pub fn finish(work_dir: &Path, ts: u64, run: Option<&str>, usage: Usage) -> std::io::Result<()> {
    let all = lines(work_dir);
    let cutoff = ts.saturating_sub(KEEP_SECS);
    let drop = |l: &Line| match l {
        Line::Entry(e) => {
            e.ts < cutoff || (e.provisional && run.is_some() && e.run.as_deref() == run)
        }
        Line::Raw(_) => false,
    };
    let entry = Entry {
        ts,
        usd: usage.usd,
        secs: usage.secs.filter(|s| s.is_finite() && *s >= 0.0),
        tokens: usage.tokens,
        run: run.map(str::to_string),
        ..Entry::default()
    };
    if all.iter().any(drop) {
        let mut kept: Vec<Line> = all.into_iter().filter(|l| !drop(l)).collect();
        kept.push(Line::Entry(entry));
        return rewrite(work_dir, &kept);
    }
    append(work_dir, &entry)
}

/// A run that ran no turn (refused by the OpenClaw gateway, deferred):
/// its provisional entry goes, nothing is charged.
pub fn cancel(work_dir: &Path, run: &str) -> std::io::Result<()> {
    let all = lines(work_dir);
    let ours =
        |l: &Line| matches!(l, Line::Entry(e) if e.provisional && e.run.as_deref() == Some(run));
    if !all.iter().any(ours) {
        return Ok(());
    }
    let kept: Vec<Line> = all.into_iter().filter(|l| !ours(l)).collect();
    rewrite(work_dir, &kept)
}

/// At a runner's start: the provisional entries left by runs that never
/// ended (the node crashed or was killed) are charged their caps (the
/// dollars allowed, the timeout) and marked unfinished. Returns how many.
pub fn settle(work_dir: &Path) -> std::io::Result<usize> {
    let mut all = lines(work_dir);
    let mut settled = 0;
    for line in &mut all {
        if let Line::Entry(e) = line
            && e.provisional
        {
            e.provisional = false;
            e.unfinished = true;
            e.usd = e.cap_usd;
            e.secs = e.cap_secs;
            settled += 1;
        }
    }
    if settled > 0 {
        rewrite(work_dir, &all)?;
    }
    Ok(settled)
}

/// What was spent today and this month (UTC), as of `now`.
pub fn spend(work_dir: &Path, now: u64) -> Spend {
    let (today, month) = (day_start(now), month_start(now));
    let mut spend = Spend::default();
    for entry in entries(work_dir).into_iter().filter(|e| !e.provisional) {
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

/// `until` of a pause that lasts until the user runs `wats setup` (a USD
/// budget of 0 from before 0.4.5).
pub const UNTIL_SETUP: u64 = 0;

pub fn check(budget: &Budget, spend: &Spend, now: u64) -> Verdict {
    if budget.paused_by_zero() {
        return Verdict::Paused {
            why: format!(
                "{} meant paused before 0.4.5 (now: no limit)",
                budget.zero_keys()
            ),
            until: UNTIL_SETUP,
        };
    }
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
    // Less than a minute left is none: a run gets at least a minute.
    if let Some(max) = budget.max_minutes_per_month
        && spend.secs_month + 60.0 > max as f64 * 60.0
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
        && spend.secs_today + 60.0 > max as f64 * 60.0
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
    let secs = [
        budget
            .max_minutes_per_day
            .map(|max| max as f64 * 60.0 - spend.secs_today),
        budget
            .max_minutes_per_month
            .map(|max| max as f64 * 60.0 - spend.secs_month),
    ];
    let tokens = [
        budget
            .max_tokens_per_day
            .map(|max| max.saturating_sub(spend.tokens_today)),
        budget
            .max_tokens_per_month
            .map(|max| max.saturating_sub(spend.tokens_month)),
    ];
    Verdict::Go {
        remaining_usd: left.into_iter().flatten().reduce(f64::min),
        remaining_secs: secs.into_iter().flatten().reduce(f64::min),
        remaining_tokens: tokens.into_iter().flatten().min(),
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
    if budget.paused_by_zero() {
        parts.push(format!(
            "paused until `wats setup` ({} meant paused before 0.4.5)",
            budget.zero_keys()
        ));
    }
    match runtime {
        "openclaw" => parts.push(format!(
            "{} ({} of {} runs today)",
            crate::openclaw::BUDGET,
            spend.uncosted_today,
            budget.max_runs_per_day
        )),
        _ if !reports_cost(runtime) && estimates_cost(runtime) => parts.push(format!(
            "{} of {} runs today ({}'s costs not enforced)",
            spend.uncosted_today,
            budget.max_runs_per_day,
            runtime_name(runtime)
        )),
        _ if !reports_cost(runtime) => parts.push(format!(
            "{} of {} runs today ({} reports no cost)",
            spend.uncosted_today,
            budget.max_runs_per_day,
            runtime_name(runtime)
        )),
        _ => {
            let mut usd = metric(
                format!("${:.2}", spend.usd_today),
                budget.max_usd_per_day.map(|m| format!("${m:.2}")),
                format!("${:.2}", spend.usd_month),
                budget.max_usd_per_month.map(|m| format!("${m:.2}")),
                "",
            );
            if estimates_cost(runtime) {
                usd.push_str(&format!(
                    " ({}'s cost estimate, not the bill)",
                    runtime_name(runtime)
                ));
            }
            parts.push(usd)
        }
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
        "paused_until_setup": budget.paused_by_zero(),
    })
}

fn runtime_name(runtime: &str) -> &str {
    match runtime {
        "codex" => "Codex",
        "claude" => "Claude Code",
        "openclaw" => "OpenClaw",
        "opencode" => "OpenCode",
        "vibe" => "Mistral Vibe",
        "copilot" => "GitHub Copilot CLI",
        "antigravity" => "Antigravity CLI",
        other => other,
    }
}

/// `until 2026-10-17 00:00:00Z`; `until you run `wats setup``.
pub fn until(ts: u64) -> String {
    if ts == UNTIL_SETUP {
        return "until you run `wats setup`".to_string();
    }
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
                remaining_secs: None,
                remaining_tokens: None,
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
                remaining_usd: None,
                remaining_secs: None,
                remaining_tokens: None,
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
        // 10 of the day's minutes left (30 of the month's): the next run's
        // timeout is capped to them.
        assert_eq!(
            check(&budget, &s, NOW),
            Verdict::Go {
                remaining_usd: None,
                remaining_secs: Some(600.0),
                remaining_tokens: None,
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
        for runtime in ["claude", "codex", "opencode", "vibe"] {
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

    /// A ledger line of 0.4.4 or older (no `secs`, no `tokens`) counts as
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

    /// A USD budget of 0 paused the agent before 0.4.5 and means no limit
    /// since: written before (setup revision below 2), it keeps the agent
    /// paused, with its notice, until the budget question confirms it
    /// (revision 2). Codex too (a 0 paused it then); not OpenClaw.
    #[test]
    fn a_zero_from_before_stays_paused() {
        let day = AgentSettings {
            max_usd_per_day: Some(0.0),
            ..AgentSettings::default()
        };
        let month = AgentSettings {
            max_usd_per_month: Some(0.0),
            ..AgentSettings::default()
        };
        let both = AgentSettings {
            max_usd_per_day: Some(0.0),
            max_usd_per_month: Some(0.0),
            ..AgentSettings::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        assert_eq!(legacy_zero(&day, Some(1), w), (true, false));
        assert_eq!(legacy_zero(&month, Some(1), w), (false, true));
        assert_eq!(legacy_zero(&day, Some(2), w), (false, false));
        assert_eq!(legacy_zero(&day, None, w), (false, false));
        assert_eq!(
            legacy_zero(&AgentSettings::default(), Some(1), w),
            (false, false)
        );
        let paused = Budget::of_dir(&day, Some(1), w);
        match check(&paused, &Spend::default(), NOW) {
            Verdict::Paused { why, until } => {
                assert_eq!(
                    why,
                    "max_usd_per_day = 0 meant paused before 0.4.5 (now: no limit)"
                );
                assert_eq!(until, UNTIL_SETUP);
            }
            go => panic!("{go:?}"),
        }
        assert_eq!(super::until(UNTIL_SETUP), "until you run `wats setup`");
        assert_eq!(
            paused.zero_notice("claude").unwrap(),
            "agent claude: max_usd_per_day = 0 meant paused before 0.4.5 and now means no \
             limit; it stays paused until you run `wats setup` (or set agents.claude.enabled = \
             false to keep it off)"
        );
        assert!(describe(&paused, &Spend::default(), "claude").starts_with(
            "paused until `wats setup` (max_usd_per_day = 0 meant paused before 0.4.5) · "
        ));
        assert_eq!(
            status_json(&paused, &Spend::default(), "claude")["paused_until_setup"],
            true
        );
        assert!(
            Budget::of_dir(&both, Some(1), w)
                .zero_notice("a")
                .unwrap()
                .starts_with("agent a: max_usd_per_day = 0 and max_usd_per_month = 0 meant paused before 0.4.5 and now mean no limit")
        );
        // Codex: still paused; OpenClaw: never was.
        assert!(paused.for_runtime("codex").paused_by_zero());
        assert!(!paused.for_runtime("openclaw").paused_by_zero());
        // Confirmed for the whole directory (revision 2): no limit.
        let confirmed = Budget::of_dir(&day, Some(2), w);
        assert_eq!(confirmed.zero_notice("claude"), None);
        assert_eq!(
            check(&confirmed, &Spend::default(), NOW),
            Verdict::Go {
                remaining_usd: None,
                remaining_secs: None,
                remaining_tokens: None,
            }
        );
        // Or for this agent only (its marker), at revision 1; another
        // agent's directory stays paused.
        let other = tempfile::tempdir().unwrap();
        confirm(w).unwrap();
        assert!(confirmed_dir_unpaused(&day, w));
        assert!(Budget::of_dir(&day, Some(1), other.path()).paused_by_zero());
    }

    fn confirmed_dir_unpaused(file: &AgentSettings, w: &Path) -> bool {
        !Budget::of_dir(file, Some(1), w).paused_by_zero()
    }

    /// OpenCode and Mistral Vibe: held to dollars from their own cost
    /// estimate while their `ENFORCE_USD_BUDGET` is on (said so), and to
    /// minutes and LLM tokens.
    #[test]
    fn opencode_and_vibe_estimate_their_cost() {
        let file = AgentSettings {
            max_minutes_per_day: Some(60),
            max_tokens_per_day: Some(1_000_000),
            ..AgentSettings::default()
        };
        let spend = Spend {
            usd_today: 0.5,
            usd_month: 0.5,
            tokens_today: 1_500,
            tokens_month: 1_500,
            ..Spend::default()
        };
        for (runtime, name, enforced) in [
            ("opencode", "OpenCode", crate::opencode::ENFORCE_USD_BUDGET),
            ("vibe", "Mistral Vibe", crate::vibe::ENFORCE_USD_BUDGET),
        ] {
            assert_eq!(reports_cost(runtime), enforced);
            assert!(reports_tokens(runtime));
            let budget = Budget::of(&file).for_runtime(runtime);
            let said = describe(&budget, &spend, runtime);
            if enforced {
                assert_eq!(budget.max_usd_per_day, Some(2.0));
                assert!(
                    said.starts_with(&format!(
                        "$0.50 of $2.00 today, $0.50 of $60.00 this month ({name}'s cost \
                         estimate, not the bill) · 0 of 60 min today"
                    )),
                    "{said}"
                );
            } else {
                assert_eq!(budget.max_usd_per_day, None);
                assert!(
                    said.contains(&format!("({name}'s costs not enforced)")),
                    "{said}"
                );
            }
            assert!(
                said.ends_with("1.5k of 1M LLM tokens today, 1.5k of 30M this month"),
                "{said}"
            );
        }
    }

    /// A run's provisional entry (ADR 0034 §6): not counted while it runs,
    /// replaced by the run's when it ends, gone for a run that ran no turn,
    /// and, left by a node that stopped during the run, charged its caps
    /// at the next start, marked unfinished.
    #[test]
    fn provisional_entries_are_settled() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        let cap = Usage {
            usd: Some(1.5),
            secs: Some(1500.0),
            tokens: None,
        };
        begin(w, NOW - 100, "r1", cap).unwrap();
        assert_eq!(spend(w, NOW).runs_today, 0, "not counted while it runs");
        finish(
            w,
            NOW - 100,
            Some("r1"),
            Usage {
                usd: Some(0.2),
                secs: Some(30.0),
                tokens: Some(100),
            },
        )
        .unwrap();
        let s = spend(w, NOW);
        assert_eq!((s.runs_today, s.tokens_today), (1, 100));
        assert!((s.usd_today - 0.2).abs() < 1e-9);
        // Refused by the gateway: nothing charged.
        begin(w, NOW - 50, "r2", cap).unwrap();
        cancel(w, "r2").unwrap();
        assert_eq!(spend(w, NOW).runs_today, 1);
        // A run the node never finished (crash, SIGKILL): charged its caps.
        begin(w, NOW - 10, "r3", cap).unwrap();
        assert_eq!(settle(w).unwrap(), 1);
        assert_eq!(settle(w).unwrap(), 0, "settled once");
        let s = spend(w, NOW);
        assert_eq!(s.runs_today, 2);
        assert!((s.usd_today - 1.7).abs() < 1e-9, "{s:?}");
        assert!((s.secs_today - 1530.0).abs() < 1e-9, "{s:?}");
        let text = std::fs::read_to_string(ledger_path(w)).unwrap();
        assert!(text.contains("\"unfinished\":true"), "{text}");
        assert!(!text.contains("provisional"), "{text}");
        // Without a dollar cap: an uncosted run, with its timeout.
        begin(
            w,
            NOW - 5,
            "r4",
            Usage {
                usd: None,
                secs: Some(60.0),
                tokens: None,
            },
        )
        .unwrap();
        settle(w).unwrap();
        assert_eq!(spend(w, NOW).uncosted_today, 1);
    }

    /// The ledger keeps lines it cannot read, even when it prunes, and an
    /// entry after a torn line (no trailing newline) starts on a line of
    /// its own.
    #[test]
    fn unreadable_ledger_lines_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        let old = NOW - 90 * 86_400;
        std::fs::write(
            ledger_path(w),
            format!(
                "{{\"ts\":{old},\"usd\":1.0}}\nnot json\n{{\"ts\":{},\"usd\":0.5",
                NOW - 60
            ),
        )
        .unwrap();
        record(w, NOW, usd(0.1)).unwrap();
        let text = std::fs::read_to_string(ledger_path(w)).unwrap();
        assert!(text.contains("not json\n"), "{text}");
        assert!(text.ends_with('\n'), "{text}");
        assert!(!text.contains(&format!("\"ts\":{old}")), "pruned: {text}");
        // The torn line is a line of its own, kept; the new entry counts.
        let s = spend(w, NOW);
        assert!((s.usd_today - 0.1).abs() < 1e-9, "{s:?}");
        // Appending after a torn line: a newline first.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(ledger_path(dir.path()), "{\"ts\":1,\"usd\":0.5").unwrap();
        record(dir.path(), NOW, usd(0.1)).unwrap();
        let text = std::fs::read_to_string(ledger_path(dir.path())).unwrap();
        assert!(text.starts_with("{\"ts\":1,\"usd\":0.5\n{"), "{text}");
        assert!((spend(dir.path(), NOW).usd_today - 0.1).abs() < 1e-9);
    }

    /// The run time left caps the next run's timeout, never under a minute:
    /// with less than a minute left, no run starts.
    #[test]
    fn minutes_left_cap_the_run() {
        let budget = Budget::of(&AgentSettings {
            max_usd_per_day: Some(0.0),
            max_minutes_per_day: Some(10),
            ..AgentSettings::default()
        });
        let spent = |secs: f64| Spend {
            secs_today: secs,
            secs_month: secs,
            ..Spend::default()
        };
        match check(&budget, &spent(9.0 * 60.0 - 30.0), NOW) {
            Verdict::Go { remaining_secs, .. } => assert_eq!(remaining_secs, Some(90.0)),
            p => panic!("{p:?}"),
        }
        assert!(matches!(
            check(&budget, &spent(9.0 * 60.0 + 30.0), NOW),
            Verdict::Paused { .. }
        ));
        let t = std::time::Duration::from_secs(25 * 60);
        assert_eq!(crate::agent::run_timeout(t, Some(90.0)).as_secs(), 90);
        assert_eq!(crate::agent::run_timeout(t, Some(10.0)).as_secs(), 60);
        assert_eq!(crate::agent::run_timeout(t, None), t);
        assert_eq!(crate::agent::run_timeout(t, Some(1e9)), t);
        // LLM tokens left, for Vibe's --max-tokens.
        let tokens = Budget::of(&AgentSettings {
            max_tokens_per_day: Some(1000),
            ..AgentSettings::default()
        });
        let s = Spend {
            tokens_today: 400,
            tokens_month: 400,
            ..Spend::default()
        };
        match check(&tokens, &s, NOW) {
            Verdict::Go {
                remaining_tokens, ..
            } => assert_eq!(remaining_tokens, Some(600)),
            p => panic!("{p:?}"),
        }
    }
}
