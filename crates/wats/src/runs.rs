//! Agent run transcripts and their index, for `wats agent logs | runs |
//! attach` (ADR 0011).
//!
//! Layout under `<data dir>/agent`:
//! - `runs/<id>.jsonl`: one transcript per run. A `wats_run` header line,
//!   the agent CLI's output as is (Claude Code `stream-json`, plus any
//!   stderr line), then a `wats_end` footer line.
//! - `runs.jsonl`: the index, one [`RunRecord`] per finished run.
//! - `current`: the id of the run in progress, if any.
//!
//! Run ids sort by start time: `20261016T013000Z-events`.

use std::io::{BufRead, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What `wats agent runs` lists for one run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub runtime: String,
    pub profile: String,
    /// `swarm-only` (`locked` before 0.4.0), `workspace` or `inherit`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access: String,
    /// The model: `agent.model`, or what the CLI reported.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// `events` or `heartbeat`.
    pub trigger: String,
    /// The events that woke the agent.
    #[serde(default)]
    pub events: Vec<String>,
    /// Unix seconds.
    pub started: u64,
    /// `ok`, `error` (non-zero exit), `timeout`, `stopped` (node shut down)
    /// or `failed` (could not start, or refused by the OpenClaw gateway);
    /// empty while running.
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// Reported by the agent CLI, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// OpenClaw: the turn's `completion.status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<String>,
    /// OpenClaw: its `replyDisposition` (`visible`, `silent` or `empty`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's
/// days-from-civil).
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468).max(0) as u64
}

/// `(year, month, day, hour, minute, second)` in UTC.
pub(crate) fn civil(ts: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (ts / 86_400) as i64;
    let rem = ts % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        (rem / 3600) as u32,
        (rem / 60 % 60) as u32,
        (rem % 60) as u32,
    )
}

/// `2026-10-16 01:30:00Z`.
pub fn format_time(ts: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(ts);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

impl RunRecord {
    /// A new run starting now, with a fresh id.
    pub fn new(runtime: &str, profile: &str, trigger: &str, events: &[String]) -> Self {
        let started = unix_now();
        let (y, mo, d, h, mi, s) = civil(started);
        RunRecord {
            id: format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z-{trigger}"),
            runtime: runtime.into(),
            profile: profile.into(),
            trigger: trigger.into(),
            events: events.to_vec(),
            started,
            ..RunRecord::default()
        }
    }

    pub fn header(&self) -> Value {
        json!({
            "type": "wats_run", "id": self.id, "runtime": self.runtime,
            "profile": self.profile, "access": self.access, "trigger": self.trigger,
            "events": self.events, "started": self.started,
        })
    }

    pub fn footer(&self) -> Value {
        let mut footer = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Some(obj) = footer.as_object_mut() {
            obj.insert("type".into(), json!("wats_end"));
            obj.remove("events");
        }
        footer
    }
}

pub fn runs_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("runs")
}

pub fn index_path(work_dir: &Path) -> PathBuf {
    work_dir.join("runs.jsonl")
}

pub fn current_path(work_dir: &Path) -> PathBuf {
    work_dir.join("current")
}

/// Where the transcript of run `id` goes.
pub fn transcript_path(work_dir: &Path, id: &str) -> PathBuf {
    runs_dir(work_dir).join(format!("{id}.jsonl"))
}

pub fn write_line(file: &mut std::fs::File, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)
}

pub fn append_index(work_dir: &Path, record: &RunRecord) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path(work_dir))?;
    write_line(&mut file, &serde_json::to_value(record)?)
}

/// The message of an error event: Codex's `error.message` or `message`,
/// OpenCode's `error.data.message`, else its `error.name`.
fn error_message(value: &Value) -> Option<&str> {
    value["error"]["message"]
        .as_str()
        .or(value["message"].as_str())
        .or(value["error"]["data"]["message"].as_str())
        .or(value["error"]["name"].as_str())
}

/// Fill turns, cost and errors from the agent CLI's final `result` line
/// (Claude Code stream-json).
pub fn read_result(transcript: &Path, record: &mut RunRecord) {
    let Ok(file) = std::fs::File::open(transcript) else {
        return;
    };
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // The model Claude Code chose (system init).
        if value["type"] == "system"
            && value["subtype"] == "init"
            && record.model.is_empty()
            && let Some(model) = value["model"].as_str()
        {
            record.model = model.to_string();
        }
        // Codex: turns and failures, no cost. OpenCode (`run --format
        // json`, ADR 0031): a turn and its estimated cost per
        // `step_finish`, and `error` events.
        match value["type"].as_str() {
            Some("turn.completed") => {
                record.turns = Some(record.turns.unwrap_or(0) + 1);
                continue;
            }
            Some("step_finish") => {
                record.turns = Some(record.turns.unwrap_or(0) + 1);
                // A cost of 0 (a model without a price) is no cost: the run
                // then counts against the runs per day.
                if crate::opencode::ENFORCE_USD_BUDGET
                    && let Some(cost) = value["part"]["cost"].as_f64().filter(|c| *c > 0.0)
                {
                    record.cost_usd = Some(record.cost_usd.unwrap_or(0.0) + cost);
                }
                continue;
            }
            // Mistral Vibe (ADR 0033): its run export, which the runner
            // adds once the run ended: steps, cost (Vibe's estimate) and
            // how it ended.
            Some("vibe_export") => {
                if let Some(steps) = value["steps"].as_u64() {
                    record.turns = Some(steps);
                }
                if crate::vibe::ENFORCE_USD_BUDGET
                    && let Some(cost) = value["cost_usd"].as_f64().filter(|c| *c > 0.0)
                {
                    record.cost_usd = Some(cost);
                }
                if record.model.is_empty()
                    && let Some(model) = value["model"].as_str()
                {
                    record.model = model.to_string();
                }
                if value["outcome"].as_str().is_some_and(|o| o != "finished") {
                    if record.status == "ok" {
                        record.status = "error".into();
                    }
                    record.error = error_message(&value)
                        .or(value["outcome"].as_str())
                        .map(|m| m.chars().take(200).collect());
                }
                continue;
            }
            Some("turn.failed") | Some("error") => {
                if record.status == "ok" {
                    record.status = "error".into();
                }
                record.error = error_message(&value).map(|m| m.chars().take(200).collect());
                continue;
            }
            _ => {}
        }
        if value["type"] != "result" {
            continue;
        }
        record.turns = value["num_turns"].as_u64();
        if record.model.is_empty()
            && let Some(usage) = value["modelUsage"].as_object()
            && let Some(model) = usage.keys().next()
        {
            record.model = model.clone();
        }
        record.cost_usd = value["total_cost_usd"].as_f64();
        if value["is_error"] == true && record.status == "ok" {
            record.status = "error".into();
        }
        if value["is_error"] == true {
            record.error = value["subtype"].as_str().map(str::to_string);
        }
    }
}

/// The id of the run in progress, if any.
pub fn current(work_dir: &Path) -> Option<String> {
    std::fs::read_to_string(current_path(work_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ids of the transcripts on disk, oldest first.
pub fn run_ids(work_dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(runs_dir(work_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".jsonl")
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

/// Every run on disk, oldest first: the index record when the run finished,
/// otherwise what its transcript header says (status `running`, or
/// `unfinished` when the node died during the run).
pub fn list(work_dir: &Path) -> Vec<RunRecord> {
    let mut index: std::collections::HashMap<String, RunRecord> = Default::default();
    if let Ok(file) = std::fs::File::open(index_path(work_dir)) {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(record) = serde_json::from_str::<RunRecord>(&line) {
                index.insert(record.id.clone(), record);
            }
        }
    }
    let current = current(work_dir);
    run_ids(work_dir)
        .into_iter()
        .map(|id| {
            index.remove(&id).unwrap_or_else(|| {
                let mut record = header_of(work_dir, &id).unwrap_or_default();
                record.id = id.clone();
                record.status = if current.as_deref() == Some(id.as_str()) {
                    "running".into()
                } else {
                    "unfinished".into()
                };
                record
            })
        })
        .collect()
}

fn header_of(work_dir: &Path, id: &str) -> Option<RunRecord> {
    let file = std::fs::File::open(transcript_path(work_dir, id)).ok()?;
    let line = std::io::BufReader::new(file).lines().next()?.ok()?;
    serde_json::from_str(&line).ok()
}

/// Find a run by id or id prefix (the newest match).
pub fn find(work_dir: &Path, prefix: &str) -> Option<String> {
    run_ids(work_dir)
        .into_iter()
        .rev()
        .find(|id| id.starts_with(prefix))
}

/// Delete transcripts beyond the newest `keep_runs` or older than
/// `keep_days`, and their index records.
pub fn prune(work_dir: &Path, keep_runs: usize, keep_days: u64) -> std::io::Result<()> {
    let ids = run_ids(work_dir);
    let cutoff = SystemTime::now() - Duration::from_secs(keep_days.saturating_mul(86_400));
    let excess = ids.len().saturating_sub(keep_runs);
    let mut removed = false;
    for (n, id) in ids.iter().enumerate() {
        let path = transcript_path(work_dir, id);
        let old = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if n < excess || old {
            std::fs::remove_file(&path)?;
            removed = true;
        }
    }
    if removed {
        let kept: std::collections::HashSet<String> = run_ids(work_dir).into_iter().collect();
        let index = std::fs::read_to_string(index_path(work_dir)).unwrap_or_default();
        let lines: String = index
            .lines()
            .filter(|line| {
                serde_json::from_str::<RunRecord>(line).is_ok_and(|r| kept.contains(&r.id))
            })
            .map(|line| format!("{line}\n"))
            .collect();
        let tmp = index_path(work_dir).with_extension("jsonl.tmp");
        std::fs::write(&tmp, lines)?;
        std::fs::rename(tmp, index_path(work_dir))?;
    }
    Ok(())
}

/// Terminal styles, or nothing.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }
    fn cyan(&self, text: &str) -> String {
        self.paint("36", text)
    }
    fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }
    fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }
    fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }
}

/// Longest tool argument or tool result shown, in chars.
const TOOL_PREVIEW: usize = 300;

/// One line, at most `max` chars.
fn preview(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// `mcp__wats__search` → `wats.search`.
fn tool_name(name: &str) -> String {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", ".", 1),
        None => name.to_string(),
    }
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

fn status_text(style: Style, status: &str) -> String {
    match status {
        "ok" => style.green(status),
        "running" => style.cyan(status),
        "" => String::new(),
        "timeout" | "stopped" | "unfinished" => style.yellow(status),
        other => style.red(other),
    }
}

/// Human-readable lines for one transcript line.
pub fn render(line: &str, style: Style) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        let line = line.trim_end();
        return if line.is_empty() {
            vec![]
        } else {
            vec![style.dim(&format!("  stderr: {line}"))]
        };
    };
    let mut out = Vec::new();
    match value["type"].as_str().unwrap_or("") {
        "wats_run" => {
            let started = value["started"].as_u64().unwrap_or(0);
            let events = value["events"].as_array().map_or(0, Vec::len);
            let why = match value["trigger"].as_str() {
                Some("events") => format!("{events} event(s) of interest"),
                Some(other) => other.to_string(),
                None => "?".into(),
            };
            out.push(style.bold(&format!(
                "── run {} · {} · {} · as {} ──",
                value["id"].as_str().unwrap_or("?"),
                format_time(started),
                why,
                value["profile"].as_str().unwrap_or("?"),
            )));
            for id in value["events"].as_array().into_iter().flatten().take(10) {
                out.push(style.dim(&format!("  woke for {}", id.as_str().unwrap_or("?"))));
            }
        }
        "wats_end" => {
            let mut parts = vec![status_text(style, value["status"].as_str().unwrap_or(""))];
            if let Some(exit) = value["exit"].as_i64() {
                parts.push(format!("exit {exit}"));
            }
            if let Some(secs) = value["seconds"].as_f64() {
                parts.push(format!("{secs:.1} s"));
            }
            if let Some(turns) = value["turns"].as_u64() {
                parts.push(format!("{turns} turns"));
            }
            if let Some(cost) = value["cost_usd"].as_f64() {
                parts.push(format!("${cost:.4}"));
            }
            if let Some(completion) = value["completion"].as_str() {
                parts.push(format!("completion {completion}"));
            }
            if let Some(reply) = value["reply"].as_str() {
                parts.push(format!("reply {reply}"));
            }
            if value["runtime"] == "openclaw"
                && let Some(error) = value["error"].as_str()
            {
                parts.push(error.to_string());
            }
            out.push(format!("■ {}", parts.join(" · ")));
            out.push(String::new());
        }
        "openclaw_forward" => out.push(style.dim(&format!(
            "→ forwarded to OpenClaw {} · agent {} · session {} · {} bytes",
            value["url"].as_str().unwrap_or("?"),
            value["agentId"].as_str().unwrap_or("?"),
            value["sessionKey"].as_str().unwrap_or("?"),
            value["bytes"].as_u64().unwrap_or(0),
        ))),
        "openclaw_reply" => out.push(style.dim(&format!(
            "← OpenClaw: {}",
            value["outcome"].as_str().unwrap_or("?")
        ))),
        "system" if value["subtype"] == "init" => {
            let servers: Vec<String> = value["mcp_servers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| {
                    format!(
                        "{} {}",
                        s["name"].as_str().unwrap_or("?"),
                        s["status"].as_str().unwrap_or("?")
                    )
                })
                .collect();
            out.push(style.dim(&format!(
                "● session {} · model {} · mcp: {}",
                value["session_id"].as_str().map_or("?", |s| &s[..s.len().min(8)]),
                value["model"].as_str().unwrap_or("?"),
                if servers.is_empty() { "none".into() } else { servers.join(", ") },
            )));
        }
        "assistant" => {
            for part in value["message"]["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") => {
                        let text = part["text"].as_str().unwrap_or("").trim();
                        if !text.is_empty() {
                            for line in text.lines() {
                                out.push(format!("  {line}"));
                            }
                        }
                    }
                    Some("tool_use") => out.push(format!(
                        "{} {}",
                        style.cyan(&format!(
                            "→ {}",
                            tool_name(part["name"].as_str().unwrap_or("?"))
                        )),
                        style.dim(&preview(&part["input"].to_string(), TOOL_PREVIEW)),
                    )),
                    _ => {}
                }
            }
        }
        "user" => {
            for part in value["message"]["content"].as_array().into_iter().flatten() {
                if part["type"] == "tool_result" {
                    let mut text = preview(&tool_result_text(&part["content"]), TOOL_PREVIEW);
                    if text.is_empty() {
                        text = "(no text)".into();
                    }
                    if part["is_error"] == true {
                        out.push(style.red(&format!("← ✗ {text}")));
                    } else {
                        out.push(style.dim(&format!("← {text}")));
                    }
                }
            }
        }
        // Codex (`codex exec --json`).
        "thread.started" => out.push(style.dim(&format!(
            "● codex thread {}",
            value["thread_id"].as_str().map_or("?", |s| &s[..s.len().min(8)])
        ))),
        "item.completed" => {
            let item = &value["item"];
            match item["type"].as_str() {
                Some("agent_message") => {
                    for line in item["text"].as_str().unwrap_or("").trim().lines() {
                        out.push(format!("  {line}"));
                    }
                }
                Some("mcp_tool_call") => {
                    out.push(format!(
                        "{} {}",
                        style.cyan(&format!(
                            "→ {}.{}",
                            item["server"].as_str().unwrap_or("?"),
                            item["tool"].as_str().unwrap_or("?")
                        )),
                        style.dim(&preview(&item["arguments"].to_string(), TOOL_PREVIEW)),
                    ));
                    if let Some(error) = item["error"]["message"].as_str() {
                        out.push(style.red(&format!("← ✗ {}", preview(error, TOOL_PREVIEW))));
                    } else {
                        let mut text =
                            preview(&tool_result_text(&item["result"]["content"]), TOOL_PREVIEW);
                        if text.is_empty() {
                            text = "(no text)".into();
                        }
                        out.push(style.dim(&format!("← {text}")));
                    }
                }
                Some("command_execution") => out.push(style.yellow(&format!(
                    "→ shell {} (exit {})",
                    preview(item["command"].as_str().unwrap_or("?"), TOOL_PREVIEW),
                    item["exit_code"]
                ))),
                Some("file_change") => out.push(style.yellow("→ file change")),
                Some("error") => out.push(style.red(&format!(
                    "✗ {}",
                    item["message"].as_str().unwrap_or("error")
                ))),
                _ => {}
            }
        }
        "turn.completed" => out.push(style.green("✓ turn done")),
        "turn.failed" => out.push(style.red(&format!(
            "✗ {}",
            value["error"]["message"].as_str().unwrap_or("turn failed")
        ))),
        "error" => out.push(style.red(&format!("✗ {}", error_message(&value).unwrap_or("error")))),
        // OpenCode (`opencode run --format json`).
        "text" => {
            for line in value["part"]["text"].as_str().unwrap_or("").trim().lines() {
                out.push(format!("  {line}"));
            }
        }
        "tool_use" => {
            let part = &value["part"];
            let state = &part["state"];
            out.push(format!(
                "{} {}",
                style.cyan(&format!("→ {}", part["tool"].as_str().unwrap_or("?"))),
                style.dim(&preview(&state["input"].to_string(), TOOL_PREVIEW)),
            ));
            if state["status"] == "error" {
                out.push(style.red(&format!(
                    "← ✗ {}",
                    preview(state["error"].as_str().unwrap_or("error"), TOOL_PREVIEW)
                )));
            } else {
                let mut text = preview(state["output"].as_str().unwrap_or(""), TOOL_PREVIEW);
                if text.is_empty() {
                    text = "(no text)".into();
                }
                out.push(style.dim(&format!("← {text}")));
            }
        }
        // Mistral Vibe (`vibe -p --output streaming`): one history entry
        // per line, then the runner's `vibe_export`.
        "message" if value["role"] == "assistant" => {
            for part in value["content"].as_array().into_iter().flatten() {
                for line in part["text"].as_str().unwrap_or("").trim().lines() {
                    out.push(format!("  {line}"));
                }
            }
        }
        "effect" => {
            let state = &value["state"];
            out.push(style.cyan(&format!(
                "→ {}",
                preview(value["title"].as_str().unwrap_or("?"), TOOL_PREVIEW)
            )));
            match state["status"].as_str() {
                Some("failed") => out.push(style.red(&format!(
                    "← ✗ {}",
                    preview(
                        state["error"]["message"].as_str().unwrap_or("failed"),
                        TOOL_PREVIEW
                    )
                ))),
                Some("completed") => {
                    let mut text =
                        preview(state["outputText"].as_str().unwrap_or(""), TOOL_PREVIEW);
                    if text.is_empty() {
                        text = "(no text)".into();
                    }
                    out.push(style.dim(&format!("← {text}")));
                }
                Some(other) => out.push(style.dim(&format!("← {other}"))),
                None => {}
            }
        }
        "notice" => out.push(style.dim(&format!(
            "● {}",
            preview(value["message"].as_str().unwrap_or(""), TOOL_PREVIEW)
        ))),
        "vibe_export" => {
            let cost = value["cost_usd"]
                .as_f64()
                .map(|c| format!(" · ${c:.4} (Vibe's estimate)"))
                .unwrap_or_default();
            let steps = value["steps"]
                .as_u64()
                .map(|n| format!(" · {n} steps"))
                .unwrap_or_default();
            match value["outcome"].as_str() {
                Some("finished") => out.push(style.green(&format!("✓ done{steps}{cost}"))),
                outcome => out.push(style.red(&format!(
                    "✗ {}{steps}{cost}",
                    error_message(&value).or(outcome).unwrap_or("ended")
                ))),
            }
        }
        "step_finish" => {
            let part = &value["part"];
            out.push(style.green(&format!(
                "✓ step done{}",
                part["cost"]
                    .as_f64()
                    .filter(|c| *c > 0.0)
                    .map(|c| format!(" · ${c:.4} (estimate)"))
                    .unwrap_or_default()
            )));
        }
        "result" => {
            let ok = value["is_error"] != true;
            let label = if ok {
                style.green("✓ done")
            } else {
                style.red(&format!(
                    "✗ {}",
                    value["subtype"].as_str().unwrap_or("error")
                ))
            };
            out.push(label);
        }
        _ => {}
    }
    out
}

/// Render a whole transcript from `offset`; returns the offset after the
/// last complete line.
fn render_from(
    path: &Path,
    offset: u64,
    style: Style,
    out: &mut impl Write,
    ended: &mut bool,
) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    for line in buf[..complete].split(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(line);
        if line.contains("\"type\":\"wats_end\"") {
            *ended = true;
        }
        for rendered in render(&line, style) {
            writeln!(out, "{rendered}")?;
        }
    }
    out.flush()?;
    Ok(offset + complete as u64)
}

/// Print runs as they are (`wats agent logs` without `-f`).
pub fn print_runs(
    work_dir: &Path,
    ids: &[String],
    style: Style,
    out: &mut impl Write,
) -> std::io::Result<()> {
    for id in ids {
        let mut ended = false;
        render_from(&transcript_path(work_dir, id), 0, style, out, &mut ended)?;
    }
    Ok(())
}

/// Follow transcripts live: the run `start` from byte `offset` (when given),
/// then every later run, forever.
pub async fn follow(
    work_dir: &Path,
    start: Option<(String, u64)>,
    style: Style,
    out: &mut impl Write,
    // Extra lines to print as they come (e.g. held posts), checked every
    // loop.
    news: &mut dyn FnMut() -> Vec<String>,
) -> std::io::Result<()> {
    // The run being followed, its offset, and whether its footer was seen.
    let mut current = start.map(|(id, offset)| (id, offset, false));
    let mut last = current.as_ref().map(|(id, _, _)| id.clone());
    if current.is_none() {
        last = run_ids(work_dir).pop();
    }
    loop {
        if current.is_none() {
            let next = run_ids(work_dir)
                .into_iter()
                .find(|id| last.as_ref().is_none_or(|l| id > l));
            if let Some(id) = next {
                current = Some((id, 0, false));
            }
        }
        if let Some((id, offset, ended)) = current.as_mut() {
            let path = transcript_path(work_dir, id);
            match render_from(&path, *offset, style, out, ended) {
                Ok(next) => *offset = next,
                // Pruned under our feet: move on.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => *ended = true,
                Err(e) => return Err(e),
            }
            let later = run_ids(work_dir).into_iter().any(|other| other > *id);
            let running = self::current(work_dir).as_deref() == Some(id.as_str());
            if *ended || (later && !running) {
                last = Some(id.clone());
                current = None;
                continue;
            }
        }
        for line in news() {
            writeln!(out, "{}", style.yellow(&line))?;
        }
        out.flush()?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The size of a transcript, i.e. the offset after everything printed.
pub fn transcript_len(work_dir: &Path, id: &str) -> u64 {
    std::fs::metadata(transcript_path(work_dir, id)).map_or(0, |m| m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_time(1_760_500_000), "2025-10-15 03:46:40Z");
        assert_eq!(format_time(951_782_400), "2000-02-29 00:00:00Z");
    }

    #[test]
    fn renders_stream_json() {
        let style = Style { color: false };
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"mcp__wats__search","input":{"query":"pdf"}}]}}"#;
        assert_eq!(
            render(tool, style),
            vec![
                "  Looking.".to_string(),
                r#"→ wats.search {"query":"pdf"}"#.to_string()
            ]
        );
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"3 posts"}]}]}}"#;
        assert_eq!(render(result, style), vec!["← 3 posts".to_string()]);
        assert_eq!(render("oops", style), vec!["  stderr: oops".to_string()]);
        let long = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","content":"{}"}}]}}}}"#,
            "x".repeat(400)
        );
        assert!(render(&long, style)[0].ends_with('…'));
    }
}
