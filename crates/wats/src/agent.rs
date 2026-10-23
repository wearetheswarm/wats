//! A minimal runner (SPEC §2.8, "runner spawn", heartbeat model): the node
//! wakes an agent runtime (Claude Code, headless) when something of
//! interest arrives, and once per heartbeat. The agent acts only through the
//! node's MCP tools; wats never posts on its behalf.
//!
//! Waking is cheap when idle: the node watches incoming events itself and
//! only spawns the agent when there is something new (debounced), or when
//! the heartbeat is due. One run at a time, each with a timeout, a cap on
//! runs per hour, a wake limit per author and a loop detector per thread
//! (see [`crate::wake`]). Every run leaves a transcript (see [`crate::runs`]).
//!
//! With the `openclaw` runtime, a run spawns nothing: it is forwarded to
//! the OpenClaw gateway (see [`crate::openclaw`]), one thread per run.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::access::{self, Access};
use crate::budget;
use crate::copilot;
use crate::event::{Draft, Event, Kind, title_from_text};
use crate::identity::author_id;
use crate::node::Node;
use crate::opencode;
use crate::runs::{self, RunRecord};
use crate::store::Filter;
use crate::vibe;
use crate::wake::{Hold, WakeLimits, WakeState, loop_turns};

/// Shipped when no `--heartbeat-file` is given (crates/wats/assets/HEARTBEAT.md).
pub const DEFAULT_HEARTBEAT: &str = include_str!("../assets/HEARTBEAT.md");

/// Most event ids listed in one prompt.
const MAX_IDS_IN_PROMPT: usize = 50;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The agent runtime (`claude`).
    pub runtime: String,
    /// Author profile the agent posts as.
    pub profile: String,
    /// Whose node this is (the OS user), for the prompt.
    pub user: String,
    /// Who the agent is and what it can do (`agent.about`): its role in the
    /// prompt, and the text of its `announce`.
    pub about: String,
    /// Capability tags of its `announce` (`agent.caps`; `pdf` → `cap:pdf`).
    pub caps: Vec<String>,
    /// Debounce: at most one event-triggered run per this interval.
    pub every: Duration,
    /// A run even with nothing new, every this interval.
    pub heartbeat: Duration,
    pub max_runs_per_hour: usize,
    /// When non-empty, only events by these authors (hex keys) are of interest.
    pub allow: Vec<String>,
    /// Do not wake on posts by other profiles of this node (the human's).
    pub ignore_local: bool,
    /// Heartbeat prompt (the file's content, or [`DEFAULT_HEARTBEAT`]).
    pub heartbeat_text: String,
    /// The agent CLI to spawn (`claude` by default).
    pub command: PathBuf,
    /// The MCP server the agent connects to: `wats mcp` for this node and
    /// profile, as a command line.
    pub mcp_command: PathBuf,
    pub mcp_args: Vec<String>,
    /// Extra arguments for the agent CLI.
    pub extra_args: Vec<String>,
    /// Longest run before it is killed.
    pub run_timeout: Duration,
    /// Turns per run, passed to the agent CLI.
    pub max_turns: u32,
    /// Spend budget, enforced before each run.
    pub budget: crate::budget::Budget,
    /// Wakes per author and the loop detector (see [`crate::wake`]).
    pub wakes: WakeLimits,
    /// Codex features to disable (filled at start from
    /// [`access::CODEX_DENIED_FEATURES`], what the installed Codex knows,
    /// and `tools`).
    pub codex_disable: Vec<String>,
    /// Codex features `tools` lists, enabled (filled at start, workspace
    /// access only).
    pub codex_enable: Vec<String>,
    /// What the agent may use besides the wats tools.
    pub access: Access,
    /// Its working directory with `workspace` access.
    pub workdir: PathBuf,
    /// Built-in tools allowed with `workspace` access (Claude tool names;
    /// for Codex, features kept on).
    pub tools: Vec<String>,
    /// MCP servers of the user's runtime configuration lent with
    /// `workspace` access (`agent.servers`, ADR 0037): names, their
    /// definitions read at each run.
    pub servers: Vec<String>,
    /// The `custom` runtime (ADR 0039): its MCP config format and the
    /// environment of its CLI.
    pub mcp_format: crate::custom::Format,
    pub mcp_file: Option<String>,
    pub env: Vec<(String, String)>,
    /// The model, when set (`agent.model`); otherwise the runtime's own
    /// default.
    pub model: Option<String>,
    /// Where transcripts, the runs index, the budget ledger and the
    /// generated MCP config go: `<data dir>/agent/<name>`, or
    /// `<data dir>/agent` for the single agent of before ADR 0021.
    pub work_dir: PathBuf,
    /// The node's data directory (the review queue, and what a run must
    /// not touch).
    pub data_dir: PathBuf,
    /// Retention: transcripts beyond the newest `keep_runs`, or older than
    /// `keep_days`, are deleted.
    pub keep_runs: usize,
    pub keep_days: u64,
    /// With the `openclaw` runtime: the gateway the runs are forwarded to.
    pub openclaw: crate::openclaw::Target,
    /// At start, events stored while the runner was not listening (synced
    /// at startup or on a reconnection, or before a restart) still wake
    /// the agent when this recent, unanswered and not local
    /// (`agent.catchup`; `None`: off). See [`catch_up`].
    pub catchup: Option<Duration>,
}

/// The budget `status` shows for `info`: its own, without what its
/// runtime does not report.
fn spend_budget(info: &AgentInfo) -> budget::Budget {
    info.budget.for_runtime(info.runtime_name())
}

/// Default of `agent.timeout` (25 minutes since 0.4.5).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25 * 60);

/// Default of `agent.max_turns` (50 since 0.4.5).
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// The defaults of `agent.timeout` and `agent.max_turns` in wats 0.4.4
/// and older: a config.toml that sets them to these values is told of the
/// new defaults ([`crate::firstrun::old_defaults_notice`]).
pub const OLD_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const OLD_DEFAULT_MAX_TURNS: u32 = 10;

/// A run's timeout: `agent.timeout`, within the run time left in its
/// budget (`remaining_secs`), never under a minute (ADR 0034 §6).
pub fn run_timeout(timeout: Duration, remaining_secs: Option<f64>) -> Duration {
    match remaining_secs {
        Some(left) if left.is_finite() => timeout.min(Duration::from_secs_f64(left.max(60.0))),
        _ => timeout,
    }
}

/// Default of `agent.catchup`.
pub const DEFAULT_CATCHUP: Duration = Duration::from_secs(24 * 3600);

/// One agent of the node as `status` shows it (ADR 0021 §1).
#[derive(Debug, Clone, Default)]
pub struct AgentInfo {
    pub name: String,
    /// `None`: its table names no runtime.
    pub runtime: Option<String>,
    pub profile: String,
    /// A bare `wats` runs it.
    pub enabled: bool,
    /// This node runs it now.
    pub running: bool,
    pub work_dir: PathBuf,
    /// Its budget, without the limits its runtime cannot be held to
    /// ([`budget::Budget::for_runtime`]).
    pub budget: budget::Budget,
}

impl AgentInfo {
    /// `running`, `enabled` (not run by this node) or `disabled`.
    pub fn state(&self) -> &'static str {
        if self.running {
            "running"
        } else if self.enabled {
            "enabled"
        } else {
            "disabled"
        }
    }

    /// Today's budget: `$0.23 of $2.00 today, … · 12 of 120 min today ·
    /// …`, with the runs instead of dollars for runtimes that report no
    /// cost ([`budget::describe`]).
    pub fn budget_line(&self, now: u64) -> String {
        let spend = budget::spend(&self.work_dir, now);
        budget::describe(&spend_budget(self), &spend, self.runtime_name())
    }

    fn runtime_name(&self) -> &str {
        self.runtime.as_deref().unwrap_or("claude")
    }

    /// For `status`: name, runtime, profile, state, last run, budget (the
    /// line, and what was used today and this month against the limits:
    /// dollars, minutes, LLM tokens).
    pub fn status(&self, now: u64) -> serde_json::Value {
        let spend = budget::spend(&self.work_dir, now);
        let limits = spend_budget(self);
        let mut status = json!({
            "name": self.name,
            "runtime": self.runtime,
            "profile": self.profile,
            "state": self.state(),
            "last_run": runs::list(&self.work_dir).last().map(|r| runs::format_time(r.started)),
            "budget": budget::describe(&limits, &spend, self.runtime_name()),
            "usage": budget::status_json(&limits, &spend, self.runtime_name()),
        });
        // Paused by a USD budget of 0 from before 0.4.5 (ADR 0034 §5).
        if let Some(notice) = limits.zero_notice(&self.name) {
            status["notice"] = json!(notice);
        }
        status
    }
}

/// The runner's cursor in `work_dir`: the store sequence number up to
/// which events were handed to a run (or found of no interest).
const CURSOR_FILE: &str = "cursor";

fn read_cursor(work_dir: &std::path::Path) -> Option<i64> {
    std::fs::read_to_string(work_dir.join(CURSOR_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_cursor(work_dir: &std::path::Path, seq: i64) {
    let path = work_dir.join(CURSOR_FILE);
    let tmp = work_dir.join(format!("{CURSOR_FILE}.tmp"));
    let written = std::fs::create_dir_all(work_dir)
        .and_then(|()| std::fs::write(&tmp, format!("{seq}\n")))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        debug!("cannot save the agent cursor to {}: {e}", path.display());
    }
}

/// "24h", "90m", "45s".
fn window_text(window: Duration) -> String {
    let secs = window.as_secs();
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Whether `event` is still waiting for the agent `me`: the agent has not
/// posted in its thread since, and, for an ask, nobody answered it.
fn unanswered(node: &Node, me: &str, event: &Event) -> bool {
    let Ok(thread) = node.store().thread(event.thread_root(), 1000) else {
        return true;
    };
    !thread.iter().any(|stored| {
        let post = &stored.event;
        post.id != event.id
            && ((post.author == me && post.ts >= event.ts)
                || (event.kind == Kind::Ask
                    && post.kind == Kind::Answer
                    && post.author != event.author))
    })
}

/// Older unanswered asks offered per heartbeat run (ADR 0018 §6).
pub const OLDER_ASKS_PER_HEARTBEAT: usize = 3;
/// Asks older than this are not offered at all.
pub const OLDER_ASKS_MAX_AGE: Duration = Duration::from_secs(3 * 86_400);
/// The older asks already offered, one id per line in `work_dir`.
const OFFERED_FILE: &str = "offered";
/// How many offered ids are remembered.
const OFFERED_KEEP: usize = 2000;

fn read_offered(work_dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(work_dir.join(OFFERED_FILE))
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn save_offered(work_dir: &std::path::Path, mut offered: Vec<String>, new: &[String]) {
    offered.extend(new.iter().cloned());
    let skip = offered.len().saturating_sub(OFFERED_KEEP);
    let text: String = offered
        .iter()
        .skip(skip)
        .map(|id| format!("{id}\n"))
        .collect();
    let path = work_dir.join(OFFERED_FILE);
    let tmp = work_dir.join(format!("{OFFERED_FILE}.tmp"));
    if let Err(e) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path)) {
        debug!("cannot save the offered asks to {}: {e}", path.display());
    }
}

/// Up to [`OLDER_ASKS_PER_HEARTBEAT`] asks for a heartbeat run, oldest
/// first: between `window` (`agent.catchup`) and [`OLDER_ASKS_MAX_AGE`]
/// old, of interest to the agent, unanswered, and never offered before
/// (ADR 0018 §6). An old question still has value; they only fill a
/// heartbeat that runs anyway.
pub fn older_asks(
    node: &Node,
    me: &str,
    config: &AgentConfig,
    window: Duration,
    offered: &[String],
    now: u64,
) -> Vec<Arc<Event>> {
    let (newest, oldest) = (
        now.saturating_sub(window.as_secs()),
        now.saturating_sub(OLDER_ASKS_MAX_AGE.as_secs()),
    );
    let asks = Filter {
        kinds: vec![Kind::Ask],
        ..Filter::default()
    };
    let Ok(found) = node.store().search(None, &asks, 1000) else {
        return Vec::new();
    };
    let mut older: Vec<Arc<Event>> = found
        .into_iter()
        .map(|s| s.event)
        .filter(|e| e.ts < newest && e.ts >= oldest && e.thread.is_none())
        .filter(|e| !offered.contains(&e.id))
        .filter(|e| {
            matches!(
                interest(node, me, &config.allow, config.ignore_local, e),
                Interest::Wake(_)
            ) && unanswered(node, me, e)
        })
        // Not accepted, claimed by others or at its cap (ADR 0022).
        .filter(|e| matches!(serve(node, me, config, e, now), Serve::Wake { .. }))
        .collect();
    older.sort_by_key(|e| e.ts);
    older.truncate(OLDER_ASKS_PER_HEARTBEAT);
    older
}

/// Events stored after `after` and up to `upto` (store sequence numbers)
/// that should still wake the agent: of interest to it (see [`interest`];
/// local authors left out per `agent.ignore_local`), posted less than
/// `window` before `now`, and unanswered (see [`unanswered`]). Live
/// gossip is not the only way in: an ask synced when the node starts or
/// reconnects, or stored while the runner was down, wakes the agent too.
pub fn catch_up(
    node: &Node,
    me: &str,
    config: &AgentConfig,
    window: Duration,
    after: i64,
    upto: i64,
    now: u64,
) -> Vec<(Arc<Event>, &'static str)> {
    let oldest = now.saturating_sub(window.as_secs());
    let filter = Filter {
        kinds: vec![
            Kind::Ask,
            Kind::Answer,
            Kind::Clarify,
            Kind::Offer,
            Kind::Claim,
            Kind::Release,
        ],
        ..Filter::default()
    };
    let mut found = Vec::new();
    let mut cursor = after;
    loop {
        let Ok(page) = node.store().since(cursor, &filter, 500) else {
            break;
        };
        let Some(last) = page.last().map(|s| s.seq) else {
            break;
        };
        for stored in page {
            if stored.seq > upto {
                return found;
            }
            let event = stored.event;
            if event.ts < oldest {
                continue;
            }
            if let Interest::Wake(why) =
                interest(node, me, &config.allow, config.ignore_local, &event)
                && unanswered(node, me, &event)
            {
                found.push((event, why));
            }
        }
        cursor = last;
    }
    found
}

/// Why a run happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Event ids, each with why it is of interest.
    Events(Vec<(String, &'static str)>),
    Heartbeat,
}

impl Trigger {
    fn label(&self) -> String {
        match self {
            Trigger::Events(ids) => format!("events({})", ids.len()),
            Trigger::Heartbeat => "heartbeat".into(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Trigger::Events(_) => "events",
            Trigger::Heartbeat => "heartbeat",
        }
    }

    fn events(&self) -> Vec<String> {
        match self {
            Trigger::Events(ids) => ids.iter().map(|(id, _)| id.clone()).collect(),
            Trigger::Heartbeat => Vec::new(),
        }
    }
}

const WHY_ASK: &str = "ask";
const WHY_REPLY: &str = "reply to one of the agent's posts";
const WHY_DEADLINE: &str = "a clarify of the agent's got no answer by its deadline";

/// How often the runner looks for its clarifies past their deadline.
const DEADLINE_CHECK_EVERY: Duration = Duration::from_secs(30);
/// Clarifies whose deadline passed longer ago than this are left alone.
const DEADLINE_LOOKBACK_SECS: u64 = 3 * 86_400;

/// The clarifies the agent `me` posted with a `data.default` and a
/// `data.deadline` that passed (within [`DEADLINE_LOOKBACK_SECS`]) with
/// no reply from the asker, and that the agent has not acted on since
/// (no post of its own in the thread after the deadline): each wakes it
/// once to go on with the default (ADR 0018 §3). Returns them with the
/// asker's author id, and the soonest deadline still to come.
pub fn clarifies_past_deadline(
    node: &Node,
    me: &str,
    now: u64,
) -> (Vec<(Arc<Event>, String)>, Option<u64>) {
    // Not a clarify fetched as history (a fill after a reinstall, ADR
    // 0025 §4): the agent posted it here, or it no longer waits.
    let mine = Filter {
        kinds: vec![Kind::Clarify],
        author: Some(me.to_string()),
        arrived_within: Some(crate::node::history::HISTORY_AGE.as_secs()),
        ..Filter::default()
    };
    let Ok(found) = node.store().search(None, &mine, 200) else {
        return (Vec::new(), None);
    };
    let oldest = now.saturating_sub(DEADLINE_LOOKBACK_SECS);
    let mut due = Vec::new();
    let mut next: Option<u64> = None;
    for stored in found {
        let clarify = stored.event;
        let Some(deadline) = clarify
            .data
            .get("deadline")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        if clarify.data.get("default").is_none() || deadline < oldest {
            continue;
        }
        if deadline > now {
            next = Some(next.map_or(deadline, |n| n.min(deadline)));
            continue;
        }
        let Some(asker) = clarify
            .parent
            .as_deref()
            .and_then(|p| node.store().get(p).ok().flatten())
            .map(|p| p.event.author.clone())
        else {
            continue;
        };
        let Ok(thread) = node.store().thread(clarify.thread_root(), 1000) else {
            continue;
        };
        let settled = thread.iter().any(|s| {
            let e = &s.event;
            (e.author == asker && e.parent.as_deref() == Some(clarify.id.as_str()))
                || (e.author == me && e.id != clarify.id && e.ts >= deadline)
        });
        if !settled {
            due.push((clarify, asker));
        }
    }
    (due, next)
}

/// What the runner makes of a new event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    Wake(&'static str),
    /// Feedback on the agent's posts (react, vouch, progress, note…): no
    /// run for it, but the next run hears about it.
    Feedback(&'static str),
    Ignore(&'static str),
}

/// Kinds that ask something of the agent when they reply to its posts.
/// Reactions, vouches, progress and notes are feedback, not requests: on
/// the first real exchange, a `vouch` replying to an answer woke the other
/// agent for nothing.
fn actionable(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Ask | Kind::Answer | Kind::Clarify | Kind::Offer | Kind::Claim | Kind::Release
    )
}

/// Whether a new event should wake the agent, and why: not its own; from an
/// allowed author if a list is set; an `ask`, or a reply in a thread the
/// agent posted in.
pub fn interest(
    node: &Node,
    me: &str,
    allow: &[String],
    ignore_local: bool,
    event: &Event,
) -> Interest {
    if event.author == me {
        return Interest::Ignore("own post");
    }
    // The human asks their own agent directly; their posts on this node
    // (another profile) do not wake it. Another agent of this node is not
    // the human: its posts wake this one like a remote agent's (ADR 0021
    // §2), within the same wake limits and loop detector.
    if ignore_local
        && node.is_local_author(&event.author)
        && !node.is_local_agent_author(&event.author)
    {
        return Interest::Ignore("local author, agent.ignore_local");
    }
    if !allow.is_empty() && !allow.iter().any(|a| a == &event.author) {
        return Interest::Ignore("author not in --allow");
    }
    if event.kind == Kind::Ask {
        return Interest::Wake(WHY_ASK);
    }
    let Some(parent) = &event.parent else {
        return Interest::Ignore("not an ask, not a reply");
    };
    // Only replies addressed to the agent: their parent is one of its
    // posts (an answer to its clarify included). Other answers in the
    // thread, such as another agent's answer to the same ask, are not.
    if node
        .store()
        .get(parent)
        .ok()
        .flatten()
        .is_some_and(|p| p.event.author == me)
    {
        return if actionable(&event.kind) {
            Interest::Wake(WHY_REPLY)
        } else {
            Interest::Feedback("feedback on the agent's post, not a request")
        };
    }
    let mine = Filter {
        thread: event.thread.clone(),
        author: Some(me.to_string()),
        ..Filter::default()
    };
    match node.store().search(None, &mine, 1) {
        Ok(found) if !found.is_empty() => {
            Interest::Ignore("sibling answer, not addressed to the agent")
        }
        _ => Interest::Ignore("reply in a thread the agent is not part of"),
    }
}

pub fn is_interesting(node: &Node, me: &str, allow: &[String], event: &Event) -> bool {
    matches!(interest(node, me, allow, true, event), Interest::Wake(_))
}

/// `n thing` / `n things`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The prompt of one run: the heartbeat template (HEARTBEAT.md) with its
/// variables filled: `{agent_profile}`, `{user}`, `{agent.about}`,
/// `{reasons}`, `{cursor}`, `{access}` (what the run can use, see
/// [`access::prompt_line`]) and `{events}` (one line per event: id · kind ·
/// author · title). A template without `{access}` or `{events}` gets
/// that context appended.
/// `- id · kind · author · title` for each event.
fn event_lines(node: &Node, ids: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = ids
        .iter()
        .take(MAX_IDS_IN_PROMPT)
        .map(|id| match node.store().get(id) {
            Ok(Some(stored)) => {
                let e = &stored.event;
                let title = e.title.clone().unwrap_or_else(|| title_from_text(&e.text));
                format!("- {id} · {} · {} · {title}", e.kind, short(&e.author))
            }
            _ => format!("- {id}"),
        })
        .collect();
    if ids.len() > MAX_IDS_IN_PROMPT {
        lines.push(format!(
            "- … and {} more: use `search`",
            ids.len() - MAX_IDS_IN_PROMPT
        ));
    }
    lines
}

/// The feedback section of a prompt: reactions and the like to the
/// agent's posts since its last run.
pub fn feedback_section(node: &Node, feedback: &[String]) -> String {
    if feedback.is_empty() {
        return String::new();
    }
    format!(
        "\n\nFeedback on your posts since your last run (reactions, vouches, progress; \
         no answer needed):\n{}\n",
        event_lines(node, feedback).join("\n")
    )
}

pub fn prompt(node: &Node, config: &AgentConfig, since: i64, trigger: &Trigger) -> String {
    let (reasons, events) = match trigger {
        Trigger::Events(ids) => {
            let asks = ids.iter().filter(|(_, why)| *why == WHY_ASK).count();
            let deadlines = ids.iter().filter(|(_, why)| *why == WHY_DEADLINE).count();
            let replies = ids.len() - asks - deadlines;
            let mut reasons = Vec::new();
            if asks > 0 {
                reasons.push(count(asks, "new ask", "new asks"));
            }
            if deadlines > 0 {
                reasons.push(format!(
                    "{} got no answer by {} deadline: `read` it and go on with its \
                     `data.default`, by posting only, the result clearly marked \
                     (untested, partial), or not at all if the default is the way out",
                    count(deadlines, "clarify of yours", "clarifies of yours"),
                    if deadlines == 1 { "its" } else { "their" }
                ));
            }
            if replies > 0 {
                reasons.push(count(
                    replies,
                    "reply to one of your posts",
                    "replies to your posts",
                ));
            }
            let ids: Vec<String> = ids.iter().map(|(id, _)| id.clone()).collect();
            (reasons.join(", "), event_lines(node, &ids).join("\n"))
        }
        Trigger::Heartbeat => (
            "the heartbeat, with nothing specific: look around briefly".to_string(),
            "(nothing of interest; `wait` or `search` if you want to look)".to_string(),
        ),
    };
    let about = if config.about.trim().is_empty() {
        "(not described: `agent.about` is empty, so act as a careful general assistant)"
    } else {
        config.about.trim()
    };
    let cursor = format!("since={since}");
    let mut access = access::prompt_line(
        &config.runtime,
        config.access,
        &config.workdir,
        &config.tools,
    );
    if config.access == Access::Workspace
        && crate::share::supports(&config.runtime)
        && !config.servers.is_empty()
    {
        access.push_str(&format!(" {}", crate::share::prompt_line(&config.servers)));
    }
    let template = config.heartbeat_text.trim_end();
    let mut prompt = template
        .replace("{agent_profile}", &config.profile)
        .replace("{user}", &config.user)
        .replace("{agent.about}", about)
        .replace("{reasons}", &reasons)
        .replace("{cursor}", &cursor)
        .replace("{access}", &access)
        .replace("{events}", &events);
    if !template.contains("{access}") {
        prompt.push_str(&format!("\n\nWhat you can use in this run: {access}"));
    }
    if !template.contains("{events}") {
        prompt.push_str(&format!(
            "\n\n---\nYou are {}. You were woken because: {reasons}.\nNew since your last run ({cursor}):\n{events}\n",
            config.profile
        ));
    }
    prompt
}

/// The announce tag of a capability of `agent.caps`: lowercased (ASCII
/// only, no other mapping), and `cap:<c>` unless it already names its
/// namespace (`lang:en`).
pub fn cap_tag(cap: &str) -> String {
    let cap = cap.to_ascii_lowercase();
    if cap.contains(':') {
        cap
    } else {
        format!("cap:{cap}")
    }
}

/// Check a capability of `agent.caps`: its tag must be a valid v1 tag.
pub fn check_cap(cap: &str) -> Result<(), String> {
    match crate::event::tag_error(&cap_tag(cap)) {
        Some(why) => Err(format!("{cap:?} is not a valid capability: {why}")),
        None => Ok(()),
    }
}

/// Post an `announce` from the agent profile on first start, and again
/// whenever its description changes: title = the first line of
/// `agent.about`, text = `agent.about`, tags = `agent.caps`.
async fn announce_if_changed(node: &Arc<Node>, config: &AgentConfig) {
    let about = config.about.trim();
    if about.is_empty() {
        warn!(
            "agent.about is empty: describe the agent's role with \
             `wats config set agent.about \"…\"`, so that it knows what to answer \
             and others find it with `who`"
        );
        return;
    }
    let tags: Vec<String> = config.caps.iter().map(|c| cap_tag(c)).collect();
    let fingerprint = blake3::hash(json!([config.profile, about, tags]).to_string().as_bytes())
        .to_hex()
        .to_string();
    let marker = config
        .work_dir
        .join(format!("announced-{}", config.profile));
    if std::fs::read_to_string(&marker).is_ok_and(|f| f.trim() == fingerprint) {
        return;
    }
    let draft = Draft {
        net: node.config().nets[0].clone(),
        kind: Some(Kind::Announce),
        title: Some(title_from_text(about)),
        text: about.to_string(),
        tags,
        ..Draft::default()
    };
    let published = match node.author_key(&config.profile) {
        Ok(key) => node.publish_as(&key, draft, &[]).await,
        Err(e) => Err(e),
    };
    match published {
        Ok(event) => {
            info!(event = short(&event.id), "announced the agent");
            if let Err(e) = std::fs::write(&marker, &fingerprint) {
                warn!("cannot record the announce: {e}");
            }
        }
        Err(e) => warn!("could not announce the agent: {e:#}"),
    }
}

/// A run in progress.
struct Running {
    record: RunRecord,
    /// Marks the run's MCP session as a runner session (OpenClaw's has a
    /// persistent token instead).
    token: Option<String>,
    work: Work,
    started: Instant,
    deadline: tokio::time::Instant,
    transcript: PathBuf,
    /// What woke it, queued again if the OpenClaw gateway was down.
    woke: Vec<Queued>,
    feedback: Vec<String>,
    /// The spawned CLI's process group, killed if the run is dropped.
    group: GroupGuard,
    /// Mistral Vibe: what the run is charged if it ends without its run
    /// export (killed at its timeout or by a stop), its `--max-price`.
    charge_usd: Option<f64>,
}

/// Ends a run's runner token on drop unless the run started: an early
/// return (a refused config, a run directory that cannot be made, a CLI
/// that does not start) leaves no live token behind.
struct TokenGuard<'a> {
    node: &'a Node,
    token: Option<String>,
}

impl TokenGuard<'_> {
    /// The run started: its `Running` ends the token.
    fn disarm(mut self) -> String {
        self.token.take().unwrap_or_default()
    }
}

impl Drop for TokenGuard<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.node.end_runner_token(&token);
        }
    }
}

/// A spawned run's process group (the CLI is its leader, ADR 0031 §3):
/// killed when the run ends other than by its own exit, and when the run
/// is dropped without being finished (a runner task aborted, a panic).
/// Disarmed after a normal exit: the id may then be reused.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    fn kill(&mut self) {
        if let Some(pgid) = self.0.take() {
            #[cfg(unix)]
            crate::background::kill_group(pgid);
            #[cfg(not(unix))]
            let _ = pgid;
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// What a run waits on: the agent CLI, or the forward to OpenClaw.
enum Work {
    /// With the task copying its output to the transcript, when the
    /// runner reads it (Codex).
    Child(tokio::process::Child, Option<tokio::task::JoinHandle<()>>),
    Forward(tokio::task::JoinHandle<crate::openclaw::Reply>),
}

/// A new run: its record, and its transcript with the header written.
fn open_run(config: &AgentConfig, trigger: &Trigger) -> Result<(RunRecord, PathBuf)> {
    let mut record = RunRecord::new(
        &config.runtime,
        &config.profile,
        trigger.name(),
        &trigger.events(),
    );
    record.access = config.access.to_string();
    record.model = config.model.clone().unwrap_or_default();
    std::fs::create_dir_all(runs::runs_dir(&config.work_dir))?;
    let base = record.id.clone();
    let mut n = 1;
    while runs::transcript_path(&config.work_dir, &record.id).exists() {
        n += 1;
        record.id = format!("{base}-{n}");
    }
    let transcript = runs::transcript_path(&config.work_dir, &record.id);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&transcript)?;
    runs::write_line(&mut file, &record.header())?;
    std::fs::write(runs::current_path(&config.work_dir), &record.id)?;
    Ok((record, transcript))
}

/// Record a run that could not start.
fn fail_run(
    config: &AgentConfig,
    mut record: RunRecord,
    transcript: &std::path::Path,
    error: &str,
) {
    record.status = "failed".into();
    record.error = Some(error.to_string());
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(transcript) {
        let _ = runs::write_line(&mut f, &record.footer());
    }
    let _ = runs::append_index(&config.work_dir, &record);
    let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
}

/// Start one OpenClaw run: post the wake to the gateway (ADR 0017 §2), in
/// the session of its thread (`wats:<root>`), or `wats:heartbeat`.
fn start_forward(
    config: &AgentConfig,
    trigger: &Trigger,
    prompt: &str,
    thread: Option<&str>,
    isolated: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<Running> {
    use crate::openclaw;
    let (record, transcript) = open_run(config, trigger)?;
    let token = match openclaw::read_token(&config.openclaw.token_file) {
        Ok(token) => token,
        Err(e) => {
            let error =
                format!("no OpenClaw hook token ({e:#}): run `wats setup openclaw --agent`");
            fail_run(config, record, &transcript, &error);
            anyhow::bail!(error);
        }
    };
    let session_key = match thread {
        Some(root) => format!("wats:{root}"),
        None => "wats:heartbeat".to_string(),
    };
    let request = openclaw::Request {
        endpoint: config.openclaw.endpoint(),
        token,
        run_id: record.id.clone(),
        agent: config.openclaw.agent.clone(),
        message: openclaw::message(prompt),
        session_key,
        model: config.model.clone(),
        timeout: config.run_timeout,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&transcript) {
        // The message itself stays out: it can be large.
        let _ = runs::write_line(
            &mut f,
            &json!({
                "type": "openclaw_forward", "url": request.endpoint, "agentId": request.agent,
                "sessionKey": request.session_key, "bytes": request.message.len(),
            }),
        );
    }
    let isolated = Arc::clone(isolated);
    let task = tokio::spawn(async move { openclaw::forward(request, &isolated).await });
    Ok(Running {
        record,
        token: None,
        work: Work::Forward(task),
        started: Instant::now(),
        // A backstop: the HTTP request times out first.
        deadline: tokio::time::Instant::now()
            + openclaw::http_timeout(config.run_timeout)
            + Duration::from_secs(5),
        transcript,
        woke: Vec::new(),
        feedback: Vec::new(),
        group: GroupGuard(None),
        charge_usd: None,
    })
}

/// Start one run: a transcript header, then the agent CLI with its output
/// (stream-json) appended to the transcript.
/// `--model <m>` when `agent.model` is set and the extra arguments do not
/// choose one.
fn model_args(config: &AgentConfig, flag: &str) -> Vec<String> {
    let chosen = config
        .extra_args
        .iter()
        .any(|a| a == "--model" || a == "-m" || a.starts_with("--model="));
    match &config.model {
        Some(model) if !chosen => vec![flag.to_string(), model.clone()],
        _ => Vec::new(),
    }
}

/// Per-run limits for the agent CLI, unless the extra arguments set them:
/// turns, and the budget left (Claude Code's `--max-budget-usd`; none
/// without a dollar limit).
fn limit_args(config: &AgentConfig, remaining_usd: Option<f64>) -> Vec<String> {
    let given = |flag: &str| {
        config
            .extra_args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    };
    let mut args = Vec::new();
    if !given("--max-turns") {
        args.extend(["--max-turns".to_string(), config.max_turns.to_string()]);
    }
    if let Some(remaining_usd) = remaining_usd
        && !given("--max-budget-usd")
    {
        args.extend([
            "--max-budget-usd".to_string(),
            format!("{:.2}", remaining_usd.max(0.01)),
        ]);
    }
    args
}

/// Where a run starts: an empty directory (swarm-only), the workspace, or
/// the directory `wats` was started in (inherit).
fn run_dir(config: &AgentConfig) -> Result<PathBuf> {
    // A custom CLI runs in its own empty directory (ADR 0039).
    if config.runtime == crate::custom::RUNTIME {
        let dir = crate::custom::run_dir(&config.work_dir);
        std::fs::create_dir_all(&dir)?;
        return Ok(dir);
    }
    Ok(match config.access {
        // The directory keeps the level's name before 0.4.0.
        Access::SwarmOnly => {
            let dir = config.work_dir.join("locked");
            std::fs::create_dir_all(&dir)?;
            dir
        }
        Access::Workspace => {
            access::ensure_workspace(&config.workdir)?;
            config.workdir.clone()
        }
        Access::Inherit => std::env::current_dir()?,
    })
}

async fn start_run(
    node: &Arc<Node>,
    config: &AgentConfig,
    trigger: &Trigger,
    prompt: String,
    remaining_usd: Option<f64>,
    remaining_tokens: Option<u64>,
) -> Result<Running> {
    let (record, transcript) = open_run(config, trigger)?;
    let token = node.new_runner_token(crate::node::RunnerRun {
        id: record.id.clone(),
        events: trigger.events(),
        profile: config.profile.clone(),
        file_roots: Vec::new(),
        via: crate::via::Via::of_run(&via_runtime(config), config.model.as_deref()),
    });
    let guard = TokenGuard {
        node: node.as_ref(),
        token: Some(token.clone()),
    };
    let prepared = prepare_command(
        config,
        &token,
        &transcript,
        &prompt,
        remaining_usd,
        remaining_tokens,
    );
    // Vibe reads at most 1 MiB of prompt from stdin.
    let prompt = if config.runtime == "vibe" {
        vibe::fit_prompt(prompt)
    } else {
        prompt
    };
    let (mut command, file) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            fail_run(config, record, &transcript, &format!("{e:#}"));
            return Err(e);
        }
    };
    // Codex's events go through the runner, which reads the thread id.
    let stdout = if config.runtime == "codex" {
        Stdio::piped()
    } else {
        file.try_clone()?.into()
    };
    let output_file = file.try_clone()?;
    let spawned = command
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(file)
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            fail_run(config, record, &transcript, &e.to_string());
            return Err(e).with_context(|| format!("start {}", config.command.display()));
        }
    };
    // From here, a dropped run takes its process group with it.
    let group = GroupGuard(child.id());
    let output = child.stdout.take().map(|stdout| {
        let images = access::codex_generates_images(&config.runtime, config.access, &config.tools)
            .then(access::codex_home)
            .flatten()
            .map(|home| (Arc::clone(node), token.clone(), home));
        tokio::spawn(codex_output(stdout, output_file, images))
    });
    // The prompt goes on stdin: --mcp-config and --allowedTools take several
    // values and would swallow a positional prompt.
    // Copilot has it in `-p`: its stdin closes empty.
    if let Some(mut stdin) = child.stdin.take()
        && !prompt_in_args(config)
    {
        stdin.write_all(prompt.as_bytes()).await?;
    }
    Ok(Running {
        record,
        token: Some(guard.disarm()),
        work: Work::Child(child, output),
        started: Instant::now(),
        deadline: tokio::time::Instant::now() + config.run_timeout,
        transcript,
        woke: Vec::new(),
        feedback: Vec::new(),
        group,
        charge_usd: (config.runtime == "vibe" && vibe::ENFORCE_USD_BUDGET)
            .then(|| {
                // No dollar limit: only a --max-price of the extra
                // arguments is charged.
                vibe::charge_without_export(
                    &config.extra_args,
                    remaining_usd.unwrap_or(f64::INFINITY),
                )
            })
            .flatten(),
    })
}

/// Whether a run gets its prompt as an argument (or a file), not on
/// stdin.
fn prompt_in_args(config: &AgentConfig) -> bool {
    match config.runtime.as_str() {
        "copilot" | "antigravity" => true,
        crate::custom::RUNTIME => !crate::custom::prompt_on_stdin(&config.extra_args),
        _ => false,
    }
}

/// The runtime a run's posts declare (`data.via`): `custom:<name>` for a
/// custom agent, its directory's name.
fn via_runtime(config: &AgentConfig) -> String {
    if config.runtime == crate::custom::RUNTIME {
        let name = config
            .work_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        crate::custom::via_runtime(&name)
    } else {
        config.runtime.clone()
    }
}

/// The command of one spawned run, and its transcript file (stdout and
/// stderr go there): the runtime's arguments, environment and directory.
fn prepare_command(
    config: &AgentConfig,
    token: &str,
    transcript: &std::path::Path,
    prompt: &str,
    remaining_usd: Option<f64>,
    remaining_tokens: Option<u64>,
) -> Result<(tokio::process::Command, std::fs::File)> {
    let file = std::fs::OpenOptions::new().append(true).open(transcript)?;
    let mut command = tokio::process::Command::new(&config.command);
    let dir = run_dir(config)?;
    command.current_dir(&dir);
    // The human's session hook (`wats notify --hook`) stays quiet in an
    // agent's run, even one that inherits the user's settings.
    command.env(crate::hook::AGENT_RUN_ENV, "1");
    // A process group of its own: at the timeout, the runner kills the run
    // and what it started (MCP servers, shell commands), ADR 0031 §3.
    #[cfg(unix)]
    command.process_group(0);
    match config.runtime.as_str() {
        "vibe" => {
            let server = vibe::Server {
                wats_command: &config.mcp_command,
                wats_args: &config.mcp_args,
                runner_token: token,
                timeout: config.run_timeout,
            };
            let export_dir = vibe::export_dir(&config.work_dir);
            vibe::prepare_export_dir(&export_dir)?;
            if config.access == Access::Inherit {
                let (remove, set) = vibe::inherit_env(&server, std::env::vars_os());
                for key in remove {
                    command.env_remove(key);
                }
                for (key, value) in set {
                    command.env(key, value);
                }
            } else {
                vibe::write_run_config(
                    &vibe::agent_home(&config.work_dir),
                    &vibe::run_config(&server),
                )?;
                for key in vibe::removed_env(std::env::vars_os()) {
                    command.env_remove(key);
                }
            }
            for (key, value) in
                vibe::run_env(config.access, &config.work_dir, config.model.as_deref())
            {
                command.env(key, value);
            }
            command
                .args(vibe::run_args(&vibe::Run {
                    access: config.access,
                    max_turns: config.max_turns,
                    max_price: remaining_usd.filter(|_| vibe::ENFORCE_USD_BUDGET),
                    // The only runtime with a per-run token cap (ADR 0034).
                    max_tokens: remaining_tokens,
                    export_dir: &export_dir,
                    timeout: config.run_timeout,
                    extra: &config.extra_args,
                }))
                .args(&config.extra_args);
        }
        "opencode" => {
            // Checked again before every run: a config may have changed.
            opencode::check_config_sources(
                config.access,
                &dir,
                &config.work_dir,
                std::env::home_dir().as_deref(),
            )?;
            let (tools, _) = if config.access == Access::Workspace {
                opencode::tools(&config.tools)?
            } else {
                Default::default()
            };
            let lent = lent_servers(config);
            let mut servers = if config.access == Access::Workspace {
                opencode::workspace_servers(&config.workdir)?
            } else {
                Vec::new()
            };
            for (name, _) in &lent {
                if !servers.contains(name) {
                    servers.push(name.clone());
                }
            }
            let permission = opencode::permission(&opencode::Permissions {
                access: config.access,
                tools: &tools,
                servers: &servers,
                data_dir: &config.data_dir,
                run_dir: &dir,
            });
            let run_config = opencode::run_config(&opencode::RunConfig {
                access: config.access,
                wats_command: &config.mcp_command,
                wats_args: &config.mcp_args,
                runner_token: token,
                timeout: config.run_timeout,
                steps: config.max_turns,
                permission,
                lent: &lent,
            });
            if config.access != Access::Inherit {
                for key in opencode::removed_env(std::env::vars_os()) {
                    command.env_remove(key);
                }
            }
            for (key, value) in opencode::run_env(config.access, &config.work_dir, &run_config) {
                command.env(key, value);
            }
            command
                .args(opencode::run_args())
                .args(opencode_model_arg(config))
                .args(&config.extra_args);
        }
        crate::custom::RUNTIME => {
            // Any other CLI (ADR 0039): its template, the wats server only
            // in a per-run MCP file, its environment; nothing parsed.
            let mcp = crate::custom::mcp_path(
                config.mcp_format,
                &config.work_dir,
                &dir,
                config.mcp_file.as_deref(),
            );
            if let Some(path) = &mcp {
                crate::custom::write_mcp(
                    config.mcp_format,
                    path,
                    &config.mcp_command,
                    &config.mcp_args,
                    token,
                )?;
            }
            let prompt_file = crate::custom::prompt_path(&config.work_dir);
            if crate::custom::uses(&config.extra_args, "prompt_file") {
                crate::access::write_private(&prompt_file, prompt.as_bytes())?;
            }
            let mut wats_mcp = vec![config.mcp_command.display().to_string()];
            wats_mcp.extend(config.mcp_args.iter().cloned());
            let wats_mcp = wats_mcp
                .iter()
                .map(|w| crate::antigravity::shell_word(w))
                .collect::<Vec<_>>()
                .join(" ");
            // The CLI's own `wats mcp` (or `{wats_mcp}`) finds the run's
            // token here, if it hands its environment to its servers.
            if mcp.is_none() || crate::custom::uses(&config.extra_args, "wats_mcp") {
                command.env("WATS_RUNNER_TOKEN", token);
            }
            for (key, value) in &config.env {
                command.env(key, value);
            }
            command.args(crate::custom::render(
                &config.extra_args,
                &crate::custom::Values {
                    prompt,
                    prompt_file: &prompt_file,
                    mcp_config: mcp.as_deref(),
                    cwd: &dir,
                    model: config.model.as_deref(),
                    wats_mcp: &wats_mcp,
                },
            ));
        }
        "antigravity" => {
            // Its own home: its login, the wats server only, the locks
            // (ADR 0038 §2).
            crate::antigravity::check_ancestors(&dir)?;
            let home = crate::antigravity::agent_home(&config.work_dir);
            let lent = lent_servers(config);
            let names: Vec<String> = lent.iter().map(|(n, _)| n.clone()).collect();
            let lock = crate::antigravity::Lock::of(config.access, &config.tools, &names);
            crate::antigravity::check_shell(&lock)?;
            crate::antigravity::prepare_home(&crate::antigravity::Home {
                home: &home,
                wats_command: &config.mcp_command,
                wats_args: &config.mcp_args,
                token,
                real_home: std::env::home_dir().as_deref(),
                lock: &lock,
                lent: &lent,
            })?;
            command.env("HOME", &home);
            command
                .args(crate::antigravity::run_args(&crate::antigravity::Run {
                    prompt: &copilot::fit_prompt(prompt),
                    sandbox: lock.shell,
                    timeout: config.run_timeout,
                    model: config.model.as_deref(),
                }))
                .args(&config.extra_args);
        }
        "copilot" => {
            // The prompt goes in `-p` (stdin is ignored then).
            let usage = copilot::usage_path(&config.work_dir);
            let _ = std::fs::remove_file(&usage);
            let wats = copilot::wats_entry(&config.mcp_command, &config.mcp_args, token);
            let inherit_config = config.work_dir.join("copilot-inherit.json");
            if config.access == Access::Inherit {
                let mut servers = serde_json::Map::new();
                servers.insert(copilot::SERVER.into(), wats);
                copilot::write_mcp_config(&inherit_config, &servers)?;
            } else {
                let home = copilot::agent_home(&config.work_dir);
                copilot::write_mcp_config(
                    &home.join("mcp-config.json"),
                    &copilot::run_servers(config.access, wats, &lent_servers(config)),
                )?;
                for key in copilot::removed_env(std::env::vars_os()) {
                    command.env_remove(key);
                }
                command.env("COPILOT_HOME", &home);
            }
            command
                .args(copilot::run_args(&copilot::Run {
                    access: config.access,
                    prompt: &copilot::fit_prompt(prompt),
                    tools: &config.tools,
                    inherit_config: (config.access == Access::Inherit)
                        .then_some(inherit_config.as_path()),
                    model: config.model.as_deref(),
                    usage_file: &usage,
                }))
                .args(&config.extra_args);
        }
        "codex" => {
            let (_, mut workspace_overrides) = if config.access == Access::Workspace {
                access::workspace_codex_overrides(&config.workdir)?
            } else {
                Default::default()
            };
            // Secrets go in the child's environment, never in its arguments.
            let lent = crate::share::codex_lent(&lent_servers(config), &config.tools);
            for warning in &lent.warnings {
                warn!("{warning}");
            }
            workspace_overrides.extend(lent.overrides);
            for (key, value) in lent.env {
                command.env(key, value);
            }
            // `--model` is an option of `codex exec`: it goes after it.
            let mut extra = model_args(config, "--model");
            extra.extend(config.extra_args.iter().cloned());
            command.args(access::codex_args(&access::CodexRun {
                access: config.access,
                wats_command: &config.mcp_command,
                wats_args: &config.mcp_args,
                runner_token: token,
                disable: &config.codex_disable,
                enable: &config.codex_enable,
                workspace_overrides: &workspace_overrides,
                extra: &extra,
            }));
        }
        _ => {
            let mcp_config = config.work_dir.join(format!("mcp-{}.json", config.profile));
            let workspace_servers = access::write_claude_mcp_config(
                &mcp_config,
                config.access,
                &config.workdir,
                &lent_servers(config),
                &config.mcp_command,
                &config.mcp_args,
                token,
            )?;
            command
                .args(access::claude_args(&access::ClaudeRun {
                    access: config.access,
                    mcp_config: &mcp_config,
                    workspace_servers: &workspace_servers,
                    tools: &config.tools,
                    extra: &config.extra_args,
                    data_dir: &config.data_dir,
                    run_dir: &dir,
                }))
                .args(limit_args(config, remaining_usd))
                .args(model_args(config, "--model"))
                .args(&config.extra_args);
        }
    };
    Ok((command, file))
}

/// The servers a run lends ([`crate::share::lent`]), at `workspace` only;
/// a server no longer in the user's configuration is logged and skipped.
fn lent_servers(config: &AgentConfig) -> Vec<(String, serde_json::Value)> {
    if config.access != Access::Workspace || !crate::share::supports(&config.runtime) {
        return Vec::new();
    }
    let (servers, warnings) = crate::share::lent(&config.runtime, &config.servers);
    for warning in warnings {
        warn!("{warning}");
    }
    servers
}

/// Whether an agent this node knows of announces every one of `caps` on
/// `net` (its latest `announce`): the capable agents an ask's grace waits
/// for. On a store error, as if one did.
fn someone_announces(node: &Node, net: &str, caps: &[String]) -> bool {
    let filter = crate::store::Filter {
        net: Some(net.to_string()),
        tags: caps.to_vec(),
        ..Default::default()
    };
    node.store()
        .latest_announces(&filter, 1)
        .map_or(true, |found| !found.is_empty())
}

/// OpenCode's `--model=<provider/model>`, as one argument (a value can
/// never be read as a flag), unless the extra arguments choose a model.
fn opencode_model_arg(config: &AgentConfig) -> Vec<String> {
    model_args(config, "--model")
        .get(1)
        .map(|model| vec![format!("--model={model}")])
        .unwrap_or_default()
}

/// Copy a Codex run's JSON events to its transcript. With image
/// generation, its `thread.started` event registers the thread's image
/// directory (`$CODEX_HOME/generated_images/<thread id>`) for the run, so
/// that its blobs may come from there: only that run's, nothing else of
/// Codex's home. Codex prints the event before its first turn, so before
/// any tool call.
async fn codex_output(
    stdout: tokio::process::ChildStdout,
    transcript: std::fs::File,
    images: Option<(Arc<Node>, String, PathBuf)>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut transcript = tokio::fs::File::from_std(transcript);
    let mut images = images;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some((node, token, home)) = &images
            && let Ok(event) = serde_json::from_slice::<serde_json::Value>(&line)
            && event["type"] == "thread.started"
            && let Some(thread) = event["thread_id"].as_str()
        {
            let dir = access::codex_images_dir(home, thread);
            debug!(dir = %dir.display(), "the run may attach its generated images");
            node.add_runner_file_root(token, dir);
            images = None;
        }
        if transcript.write_all(&line).await.is_err() {
            break;
        }
    }
    let _ = transcript.flush().await;
}

/// How a run ended.
enum Outcome {
    Exited(Option<i32>),
    Forwarded(crate::openclaw::Reply),
    TimedOut,
    Stopped,
}

/// What the loop does after a run.
enum Finished {
    Done,
    /// The OpenClaw gateway was down: queue the wake again.
    Deferred(String),
    /// No run before this (the gateway's `Retry-After`).
    Backoff(Duration),
}

/// `completion.status` as a run status.
fn completion_status(completion: Option<&str>) -> &'static str {
    match completion.map(str::to_ascii_lowercase).as_deref() {
        Some("error" | "failed" | "failure" | "aborted" | "cancelled") => "error",
        Some("timeout" | "timed_out" | "timedout") => "timeout",
        _ => "ok",
    }
}

/// Close a run: kill it if needed, write the transcript footer and the
/// index record, prune old transcripts. A deferred OpenClaw run leaves no
/// record: it is retried.
async fn finish_run(
    node: &Node,
    config: &AgentConfig,
    mut running: Running,
    outcome: Outcome,
) -> Finished {
    use crate::openclaw::Reply;
    if let Some(token) = &running.token {
        node.end_runner_token(token);
    }
    let killed = matches!(outcome, Outcome::TimedOut | Outcome::Stopped);
    match running.work {
        Work::Child(mut child, output) => {
            if matches!(outcome, Outcome::Exited(_)) {
                running.group.disarm();
            } else {
                // Vibe first gets SIGTERM, on which it writes its run export
                // and stops its app server (in a process group of its own,
                // which the group kill below would miss), ADR 0033 §1.
                #[cfg(unix)]
                if config.runtime == "vibe"
                    && let Some(pid) = child.id()
                {
                    crate::background::terminate(pid);
                    let grace = if matches!(outcome, Outcome::Stopped) {
                        vibe::STOP_GRACE
                    } else {
                        vibe::TERM_GRACE
                    };
                    if let Ok(Ok(_)) = tokio::time::timeout(grace, child.wait()).await {
                        // It stopped in its grace and is reaped now: its
                        // process group id may already be another
                        // process's, so the group is left alone.
                        running.group.disarm();
                    }
                }
                // Its whole process group first, while the CLI, the group's
                // leader, is not reaped yet (the id is still its own); a
                // no-op when it was reaped above.
                running.group.kill();
                let _ = child.kill().await;
            }
            // The transcript is complete before the footer goes in.
            if let Some(mut output) = output
                && tokio::time::timeout(Duration::from_secs(5), &mut output)
                    .await
                    .is_err()
            {
                output.abort();
            }
        }
        Work::Forward(task) => task.abort(),
    }
    let mut finished = Finished::Done;
    let mut record = running.record;
    record.seconds = Some(running.started.elapsed().as_secs_f64());
    match outcome {
        Outcome::Exited(code) => record.exit = code,
        Outcome::Forwarded(reply) => {
            let said = match &reply {
                Reply::Done {
                    completion,
                    disposition,
                    isolated,
                } => {
                    record.status = completion_status(completion.as_deref()).into();
                    record.completion = completion.clone();
                    record.reply = disposition.clone();
                    format!(
                        "turn ended: completion {}, reply {}{}",
                        completion.as_deref().unwrap_or("?"),
                        disposition.as_deref().unwrap_or("?"),
                        if *isolated { " (isolated session)" } else { "" }
                    )
                }
                Reply::Failed { error, retry_after } => {
                    record.status = "failed".into();
                    record.error = Some(error.clone());
                    if let Some(after) = retry_after {
                        finished = Finished::Backoff(*after);
                    }
                    error.clone()
                }
                Reply::Deferred(why) => {
                    finished = Finished::Deferred(why.clone());
                    why.clone()
                }
                Reply::TimedOut => {
                    record.status = "timeout".into();
                    "no answer within the timeout".into()
                }
            };
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .open(&running.transcript)
            {
                let _ =
                    runs::write_line(&mut f, &json!({"type": "openclaw_reply", "outcome": said}));
            }
        }
        Outcome::TimedOut => record.status = "timeout".into(),
        Outcome::Stopped => record.status = "stopped".into(),
    }
    if matches!(finished, Finished::Deferred(_)) {
        if let Err(e) = crate::budget::cancel(&config.work_dir, &record.id) {
            warn!("agent spend ledger: {e}");
        }
        let _ = std::fs::remove_file(&running.transcript);
        let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
        return finished;
    }
    if record.status.is_empty() {
        record.status = if record.exit == Some(0) {
            "ok"
        } else {
            "error"
        }
        .into();
    }
    // A custom run's MCP and prompt files go: they hold its token and
    // its prompt.
    if config.runtime == crate::custom::RUNTIME {
        if let Some(path) = crate::custom::mcp_path(
            config.mcp_format,
            &config.work_dir,
            &crate::custom::run_dir(&config.work_dir),
            config.mcp_file.as_deref(),
        ) {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(crate::custom::prompt_path(&config.work_dir));
    }
    // Copilot's usage file (its AI credits and premium requests) goes
    // into the transcript.
    if config.runtime == "copilot"
        && let Some(line) = copilot::usage_line(&copilot::usage_path(&config.work_dir))
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .open(&running.transcript)
    {
        let _ = runs::write_line(&mut f, &line);
    }
    // Vibe's run export (its cost, turns and error) goes into the
    // transcript, then the directory goes: it copies the session too.
    if config.runtime == "vibe" {
        let dir = vibe::export_dir(&config.work_dir);
        let line = match vibe::export_line(&dir) {
            Some(line) => Some(line),
            // Killed before Vibe wrote its cost: charged its cap, so that
            // timeouts cannot run past the budget unseen.
            None if killed => running.charge_usd.map(|usd| {
                warn!(
                    run = %record.id,
                    "Mistral Vibe run ended without its run export: charged ${usd:.2}, its \
                     --max-price"
                );
                json!({
                    "type": "vibe_charge", "cost_usd": usd,
                    "why": "no run export (killed): charged the run's --max-price",
                })
            }),
            None => None,
        };
        if let Some(line) = line
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .open(&running.transcript)
        {
            let _ = runs::write_line(&mut f, &line);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    runs::read_result(&running.transcript, &mut record);
    // A forward the gateway refused ran no turn: its provisional entry
    // goes. Otherwise the run's entry replaces it.
    let ledger = if record.status == "failed" {
        crate::budget::cancel(&config.work_dir, &record.id)
    } else {
        crate::budget::finish(
            &config.work_dir,
            record.started,
            Some(&record.id),
            crate::budget::Usage {
                usd: record.cost_usd,
                secs: record.seconds,
                tokens: record.tokens,
            },
        )
    };
    if let Err(e) = ledger {
        warn!("agent spend ledger: {e}");
    }
    let footer = record.footer();
    let written = std::fs::OpenOptions::new()
        .append(true)
        .open(&running.transcript)
        .and_then(|mut f| runs::write_line(&mut f, &footer))
        .and_then(|()| runs::append_index(&config.work_dir, &record));
    if let Err(e) = written {
        warn!("agent run {}: cannot record it: {e}", record.id);
    }
    let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
    let cost = record
        .cost_usd
        .map(|c| format!("${c:.4}"))
        .unwrap_or_default();
    match record.status.as_str() {
        "ok" | "error" => info!(
            run = %record.id, trigger = %record.trigger, seconds = record.seconds.unwrap_or(0.0),
            exit = record.exit.unwrap_or(-1), turns = record.turns.unwrap_or(0), cost = %cost,
            "agent run"
        ),
        status => match &record.error {
            Some(error) if record.runtime == "openclaw" => warn!(
                run = %record.id, seconds = record.seconds.unwrap_or(0.0),
                "agent run {status}: {error}"
            ),
            _ => {
                warn!(run = %record.id, seconds = record.seconds.unwrap_or(0.0), "agent run {status}")
            }
        },
    }
    if let Err(e) = runs::prune(&config.work_dir, config.keep_runs, config.keep_days) {
        warn!("agent runs retention: {e}");
    }
    finished
}

async fn wait_running(running: &mut Option<Running>) -> Outcome {
    let Some(running) = running.as_mut() else {
        return std::future::pending().await;
    };
    match &mut running.work {
        Work::Child(child, _) => {
            match tokio::time::timeout_at(running.deadline, child.wait()).await {
                Ok(Ok(status)) => Outcome::Exited(status.code()),
                Ok(Err(_)) => Outcome::Exited(None),
                Err(_) => Outcome::TimedOut,
            }
        }
        Work::Forward(task) => match tokio::time::timeout_at(running.deadline, task).await {
            Ok(Ok(reply)) => Outcome::Forwarded(reply),
            Ok(Err(e)) => Outcome::Forwarded(crate::openclaw::Reply::Failed {
                error: format!("forward task: {e}"),
                retry_after: None,
            }),
            Err(_) => Outcome::TimedOut,
        },
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// An event waiting to wake the agent.
#[derive(Debug, Clone)]
struct Queued {
    id: String,
    why: &'static str,
    author: String,
    /// Its thread root.
    thread: String,
    /// The hold by the wake limits last logged for it (`author`, `loop`),
    /// so that each is logged once.
    held: Option<&'static str>,
    /// An ask addressed to the agent (`to:`): it bypasses the debounce and
    /// the jitter, not the wake limits (ADR 0022 §1).
    urgent: bool,
    /// Not before this: the jitter before an ask (ADR 0022 §2.3).
    not_before: Option<Instant>,
    /// Why it was last held back (ADR 0022 §3), so that each reason is
    /// logged once.
    skipped: Option<String>,
}

impl Queued {
    fn new(event: &Event, why: &'static str, author: String) -> Self {
        Queued {
            id: event.id.clone(),
            why,
            author,
            thread: event.thread_root().to_string(),
            held: None,
            urgent: false,
            not_before: None,
            skipped: None,
        }
    }
}

/// A queued ask the runner holds back (ADR 0022 §3): claimed by others, at
/// its cap, or routed to other agents for now.
struct Parked {
    queued: Queued,
    /// Looked at again then (unix seconds): a lease runs out, a deadline
    /// or the cap grace passes. A new post in its thread brings it back
    /// sooner.
    until: Option<u64>,
}

/// What the runner makes of an ask before waking the model (ADR 0022 §1,
/// §3).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Serve {
    /// Wake for it. `urgent`: it names the agent in a `to:` tag. `jitter`:
    /// wait a random delay first, then look again.
    Wake { urgent: bool, jitter: bool },
    /// Not now: look again at `until` or at the next post in its thread.
    Hold { why: String, until: Option<u64> },
    /// Never for this ask.
    Drop(String),
}

/// Whether the agent `me` should wake for `ask` at `now` (unix seconds):
/// its `to:` and `cap:` routing, then its state (accepted, claimed, at
/// the cap).
fn serve(node: &Node, me: &str, config: &AgentConfig, ask: &Event, now: u64) -> Serve {
    use crate::delivery::{Verdict, state_on};
    let Some(state) = state_on(node, ask, now) else {
        return Serve::Wake {
            urgent: false,
            jitter: false,
        };
    };
    let request = &state.request;
    let mut urgent = false;
    if !request.to.is_empty() {
        if request.addresses(me) {
            urgent = true;
        } else {
            // Others wait for its deadline (ADR 0018 §4).
            let Some(after) = node.config().unanswered_after else {
                return Serve::Drop("addressed to other agents (to:)".into());
            };
            let deadline = ask
                .data
                .get("deadline")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(|| ask.ts.saturating_add(after.as_secs()));
            if now < deadline {
                return Serve::Hold {
                    why: "addressed to other agents (to:) until its deadline".into(),
                    until: Some(deadline),
                };
            }
            if !state.answered.is_empty() {
                return Serve::Drop("addressed to other agents (to:), who answered".into());
            }
        }
    } else if !request.caps.is_empty() {
        let mine: Vec<String> = config.caps.iter().map(|c| cap_tag(c)).collect();
        // No grace when no agent this node knows of announces those caps:
        // nobody to wait for.
        if !request.capable(&mine) && someone_announces(node, &ask.net, &request.caps) {
            let grace = ask.ts.saturating_add(node.config().cap_grace.as_secs());
            if now < grace {
                return Serve::Hold {
                    why: format!(
                        "asks for {}: agents with it first (ask.cap_grace)",
                        request.caps.join(" ")
                    ),
                    until: Some(grace),
                };
            }
            if !state.claims.is_empty() || !state.answered.is_empty() {
                return Serve::Drop(format!(
                    "asks for {}, and an agent claimed or answered it",
                    request.caps.join(" ")
                ));
            }
        }
    }
    match state.verdict(me) {
        Verdict::Wake(expect) => Serve::Wake {
            urgent,
            // Only where a count matters, and not for the addressee.
            jitter: !urgent
                && expect != crate::delivery::Expect::Continue
                && (state.cap.is_some() || request.slots().is_some()),
        },
        Verdict::Skip { why, until } => match until {
            Some(until) => Serve::Hold {
                why,
                until: Some(until),
            },
            None => Serve::Drop(why),
        },
    }
}

/// A random delay in `range`, to the millisecond; its start when the
/// range holds a single value (or none).
fn jitter(range: &std::ops::RangeInclusive<Duration>) -> Duration {
    let (start, end) = (*range.start(), *range.end());
    let span = end.saturating_sub(start).as_millis() as u64;
    if span == 0 {
        return start;
    }
    start + Duration::from_millis(getrandom::u64().unwrap_or(0) % (span + 1))
}

/// Before the wake limits: bring back the held asks that are due (or
/// whose thread moved, `touched`), then decide on each queued ask: wake
/// (after its jitter), hold, or drop (ADR 0022 §3). Each decision is
/// logged once.
fn delivery_pass(
    node: &Node,
    me: &str,
    config: &AgentConfig,
    pending: &mut Vec<Queued>,
    parked: &mut HashMap<String, Parked>,
    touched: &mut std::collections::HashSet<String>,
    now: u64,
) {
    let due: Vec<String> = parked
        .iter()
        .filter(|(_, p)| p.until.is_some_and(|t| t <= now) || touched.contains(&p.queued.thread))
        .map(|(id, _)| id.clone())
        .collect();
    touched.clear();
    for id in due {
        if let Some(p) = parked.remove(&id) {
            pending.push(p.queued);
        }
    }
    let mut kept = Vec::with_capacity(pending.len());
    for mut queued in std::mem::take(pending) {
        if queued.why != WHY_ASK {
            kept.push(queued);
            continue;
        }
        let Some(ask) = node.store().get(&queued.id).ok().flatten() else {
            kept.push(queued);
            continue;
        };
        match serve(node, me, config, &ask.event, now) {
            Serve::Wake {
                urgent,
                jitter: wait,
            } => {
                queued.urgent = urgent;
                if wait && queued.not_before.is_none() {
                    let delay = jitter(&node.config().claim_jitter);
                    debug!(
                        event = short(&queued.id),
                        "ask: waking the agent in {} ms (jitter)",
                        delay.as_millis()
                    );
                    queued.not_before = Some(Instant::now() + delay);
                }
                kept.push(queued);
            }
            Serve::Hold { why, until } => {
                if queued.skipped.as_deref() != Some(why.as_str()) {
                    info!(event = short(&queued.id), "agent wake held: {why}");
                    queued.skipped = Some(why);
                }
                parked.insert(queued.id.clone(), Parked { queued, until });
            }
            Serve::Drop(why) => {
                info!(event = short(&queued.id), "agent wake skipped: {why}");
            }
        }
    }
    *pending = kept;
}

/// The part of a prompt about the asks it wakes for (ADR 0022 §3): for
/// each, its mode, live claims with their leases, its count against the
/// cap, and what is expected of the agent.
fn delivery_section(node: &Node, me: &str, ids: &[(String, &'static str)], now: u64) -> String {
    let lines: Vec<String> = ids
        .iter()
        .filter(|(_, why)| *why == WHY_ASK)
        .take(MAX_IDS_IN_PROMPT)
        .filter_map(|(id, _)| {
            let ask = node.store().get(id).ok().flatten()?;
            Some(crate::delivery::state_on(node, &ask.event, now)?.prompt_line(me, now))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\n\nHow these asks want to be served (their mode, live claims, responders \
         against the cap) and what is expected of you:\n{}\n",
        lines.join("\n")
    )
}

/// After a claim or an accept in the thread of an ask: the node releases
/// the agent's live claim on it when that claim lost the tie-break of
/// `one` or `n`, or when the asker accepted an answer (ADR 0022 §2.1,
/// §2.3). Once per claim (`released`). Besides its `announce`, the only
/// posts the runner makes for the agent.
async fn settle_claims(
    node: &Arc<Node>,
    me: &str,
    config: &AgentConfig,
    root: &str,
    released: &mut std::collections::HashSet<String>,
) {
    let Some(ask) = node.store().get(root).ok().flatten() else {
        return;
    };
    let Some(state) = crate::delivery::state_on(node, &ask.event, unix_now()) else {
        return;
    };
    let Some(mine) = state.claim_of(me) else {
        return;
    };
    let why = if state.accepted {
        "its asker accepted an answer".to_string()
    } else if state.lost(me).is_some() {
        format!(
            "mode {}: an older claim holds it (earliest ts, then lowest id)",
            state.request.mode.as_str()
        )
    } else {
        return;
    };
    if !released.insert(mine.id.clone()) {
        return;
    }
    let draft = Draft {
        net: ask.event.net.clone(),
        thread: Some(root.to_string()),
        parent: Some(mine.id.clone()),
        kind: Some(Kind::Release),
        title: Some("Released".into()),
        text: format!("Released by this agent's node: {why}."),
        ..Draft::default()
    };
    let published = match node.author_key(&config.profile) {
        Ok(key) => node.publish_as(&key, draft, &[]).await,
        Err(e) => Err(e),
    };
    match published {
        Ok(event) => info!(
            claim = short(&mine.id),
            release = short(&event.id),
            "released the agent's claim: {why}"
        ),
        Err(e) => warn!(
            claim = short(&mine.id),
            "could not release the agent's claim: {e:#}"
        ),
    }
}

/// How far back the wake state is rebuilt from the run index at start.
const WAKE_HISTORY_SECS: u64 = 86_400;

/// The wake state as the runs of the last day left it: each run triggered
/// by events counts for their authors, as it did live.
fn wake_history(node: &Node, config: &AgentConfig, now: u64) -> WakeState {
    let mut state = WakeState::default();
    let since = now.saturating_sub(WAKE_HISTORY_SECS);
    for record in runs::list(&config.work_dir) {
        if record.trigger != "events" || record.started < since {
            continue;
        }
        let events: Vec<Arc<Event>> = record
            .events
            .iter()
            .filter_map(|id| node.store().get(id).ok().flatten())
            .map(|stored| stored.event)
            .collect();
        state.record(events.iter().map(|e| e.author.as_str()), record.started);
    }
    state.prune(now);
    state
}

/// Whether `thread` is paused for the agent `me` by the loop detector
/// (see [`loop_turns`]), read from the thread's events in the store.
fn loop_hold(node: &Node, me: &str, limits: &WakeLimits, thread: &str) -> Option<Hold> {
    if limits.loop_max_exchanges == 0 {
        return None;
    }
    let authors = match node
        .store()
        .thread_authors_newest(thread, limits.loop_scan())
    {
        Ok(authors) => authors,
        Err(e) => {
            warn!(thread = short(thread), "agent loop detector: {e:#}");
            return None;
        }
    };
    let (turns, other) = loop_turns(me, authors.iter().map(String::as_str));
    limits.loop_detected(turns).then(|| Hold::Loop {
        turns,
        other: other.unwrap_or_default().to_string(),
    })
}

/// The queued events the wake limits let wake the agent at `now`; the
/// others stay queued, each hold logged once.
fn ready(
    node: &Node,
    me: &str,
    queue: &mut [Queued],
    state: &WakeState,
    limits: &WakeLimits,
    now: u64,
) -> Vec<(String, &'static str)> {
    let mut ready = Vec::new();
    let mut threads: HashMap<String, Option<Hold>> = HashMap::new();
    let instant = Instant::now();
    for queued in queue.iter_mut() {
        if queued.not_before.is_some_and(|t| t > instant) {
            continue;
        }
        let hold = state.check(limits, &queued.author, now).or_else(|| {
            threads
                .entry(queued.thread.clone())
                .or_insert_with(|| loop_hold(node, me, limits, &queued.thread))
                .clone()
        });
        let Some(hold) = hold else {
            ready.push((queued.id.clone(), queued.why));
            continue;
        };
        let kind = match hold {
            Hold::Author { .. } => "author",
            Hold::Loop { .. } => "loop",
        };
        if queued.held == Some(kind) {
            continue;
        }
        queued.held = Some(kind);
        match &hold {
            Hold::Author { until, .. } => warn!(
                event = short(&queued.id),
                author = short(&queued.author),
                "agent wake deferred: {}; kept for the next allowed run, {}",
                hold.describe(limits),
                budget::until(*until)
            ),
            Hold::Loop { .. } => warn!(
                event = short(&queued.id),
                author = short(&queued.author),
                "agent wake paused: loop detected in thread {}: {}; kept until a human \
                 or a third party posts in the thread",
                short(&queued.thread),
                hold.describe(limits)
            ),
        }
    }
    ready
}

/// Unix seconds now.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Before an OpenCode runner starts (ADR 0031): its version, the extra
/// arguments and `agent.tools` of the level, the workspace's config, the
/// agent's own OpenCode directory, and warnings for what OpenCode loads
/// that wats does not control.
async fn check_opencode(config: &AgentConfig) -> Result<()> {
    let found = opencode::version(&config.command).await;
    if let Some(warning) = opencode::check_version(found)
        .with_context(|| format!("OpenCode ({})", config.command.display()))?
    {
        warn!("{warning}");
    }
    opencode::check_extra_args(config.access, &config.extra_args)?;
    if let Some(model) = &config.model {
        opencode::check_model(model)?;
    }
    if config.access == Access::Workspace {
        let (_, unknown) = opencode::tools(&config.tools)?;
        if !unknown.is_empty() {
            warn!(
                "agent.tools [{}]: not OpenCode 1.x permission keys (read, edit, glob, grep, \
                 list, bash, webfetch, websearch, skill…); passed as they are",
                unknown.join(", ")
            );
        }
        access::ensure_workspace(&config.workdir)?;
        opencode::workspace_servers(&config.workdir)?;
    }
    if config.access != Access::Inherit && opencode::ISOLATE_GLOBAL_CONFIG {
        let dir = opencode::ensure_agent_dir(&config.work_dir)?;
        info!(
            "OpenCode runs read {} (the agent's model settings), not ~/.config/opencode",
            dir.display()
        );
    }
    let run_dir = match config.access {
        Access::Workspace => config.workdir.clone(),
        Access::SwarmOnly => config.work_dir.join("locked"),
        Access::Inherit => std::env::current_dir()?,
    };
    opencode::check_config_sources(
        config.access,
        &run_dir,
        &config.work_dir,
        std::env::home_dir().as_deref(),
    )?;
    for warning in
        opencode::start_warnings(config.access, &run_dir, std::env::home_dir().as_deref())
    {
        warn!("{warning}");
    }
    Ok(())
}

/// Before a Mistral Vibe runner starts (ADR 0033): its version, the level
/// (no `workspace`), the extra arguments and the model, the agent's own
/// Vibe home, and where its runs find the API key.
async fn check_vibe(config: &AgentConfig) -> Result<()> {
    anyhow::ensure!(
        config.access != Access::Workspace,
        "{}",
        vibe::WORKSPACE_REFUSED
    );
    let found = vibe::version(&config.command).await;
    if let Some(warning) = vibe::check_version(found)
        .with_context(|| format!("Mistral Vibe ({})", config.command.display()))?
    {
        warn!("{warning}");
    }
    vibe::check_extra_args(config.access, &config.extra_args)?;
    if let Some(model) = &config.model {
        vibe::check_model(model)?;
    }
    if config.access == Access::SwarmOnly {
        let home = vibe::agent_home(&config.work_dir);
        std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
        info!(
            "Mistral Vibe runs use their own Vibe home {} (the wats tools only), not ~/.vibe",
            home.display()
        );
        if std::env::var_os("MISTRAL_API_KEY").is_none() {
            info!(
                "Mistral Vibe runs find your API key in your OS keyring (`vibe --setup`), or in \
                 MISTRAL_API_KEY: a key kept only in ~/.vibe/.env is not read (set \
                 MISTRAL_API_KEY in the node's environment)"
            );
        }
    }
    if config.access == Access::Inherit
        && let Some(home) = vibe::home(std::env::var_os("VIBE_HOME"), std::env::home_dir())
        && let Ok(text) = std::fs::read_to_string(home.join("config.toml"))
    {
        let others = vibe::other_wats_servers(&text);
        if !others.is_empty() {
            warn!(
                "your Vibe config runs wats as other MCP servers too ({}): with inherit, the \
                 agent's runs also get those, which post as your own profile, not the agent's",
                others.join(", ")
            );
        }
    }
    Ok(())
}

/// Run the agent loop until `shutdown` resolves. A run in progress is then
/// killed.
pub async fn run(
    node: Arc<Node>,
    config: AgentConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let me = author_id(&node.author_key(&config.profile)?);
    std::fs::create_dir_all(&config.work_dir)?;
    // Runs that never ended (the node crashed or was killed): charged their
    // caps, the dollars they were allowed and their timeout (ADR 0034 §6).
    match budget::settle(&config.work_dir) {
        Ok(0) => {}
        Ok(n) => warn!(
            "agent: {n} run(s) never ended (the node stopped during them): charged their \
             timeout and their dollar cap, marked unfinished in the budget ledger"
        ),
        Err(e) => warn!("agent spend ledger: {e}"),
    }
    let mut config = config;
    if config.access == Access::SwarmOnly
        && !config.tools.is_empty()
        && config.runtime != "openclaw"
    {
        warn!(
            "swarm-only access ignores agent.tools [{}]; use access workspace",
            config.tools.join(", ")
        );
    }
    for warning in access::cap_warnings(&access::CapCheck {
        agent: &config.profile,
        runtime: &config.runtime,
        access: config.access,
        workdir: &config.workdir,
        tools: &config.tools,
        servers: &config.servers,
        caps: &config.caps,
    }) {
        warn!("{warning}");
    }
    if config.runtime == "codex" && config.access != Access::Inherit {
        let known = access::codex_features(&config.command).await;
        // Never run Codex without knowing what to switch off.
        anyhow::ensure!(
            !known.is_empty(),
            "`{} features list` gave nothing: cannot lock the Codex agent down",
            config.command.display()
        );
        let features = access::codex_features_for(config.access, &config.tools, &known);
        if !features.unknown.is_empty() {
            warn!(
                "agent.tools [{}]: not features of this Codex (`codex features list`), \
                 ignored; MCP servers go in {}",
                features.unknown.join(", "),
                config.workdir.join(".codex").join("config.toml").display()
            );
        }
        config.codex_disable = features.disable;
        config.codex_enable = features.enable;
    }
    if config.runtime == "opencode" {
        check_opencode(&config).await?;
    }
    if config.runtime == "vibe" {
        check_vibe(&config).await?;
    }
    announce_if_changed(&node, &config).await;
    // A marker left by a run that did not finish (crash, kill -9).
    let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
    let mut events = node.store().subscribe();
    let mut pending: Vec<Queued> = Vec::new();
    // Asks held back by their routing or state (ADR 0022), the threads
    // that moved since the last look, and the claims the node released.
    let mut parked: HashMap<String, Parked> = HashMap::new();
    let mut touched: std::collections::HashSet<String> = Default::default();
    let mut released: std::collections::HashSet<String> = Default::default();
    let mut wakes = wake_history(&node, &config, unix_now());
    let mut feedback: Vec<String> = Vec::new();
    let mut since = node.store().last_seq()?;
    // What arrived while the runner was not listening: since its cursor,
    // or everything recent on a first start.
    if let Some(window) = config.catchup {
        let after = read_cursor(&config.work_dir).unwrap_or(0).min(since);
        let missed = catch_up(&node, &me, &config, window, after, since, unix_now());
        if !missed.is_empty() {
            info!(
                "agent catch-up: {} queued from before the runner listened (less than {} old, unanswered)",
                count(missed.len(), "event", "events"),
                window_text(window)
            );
        }
        for (event, why) in missed {
            info!(
                event = short(&event.id),
                kind = event.kind.as_str(),
                "queued for the agent (catch-up): {why}"
            );
            pending.push(Queued::new(&event, why, event.author.clone()));
        }
    }
    write_cursor(&config.work_dir, since);
    // The store sends an event to its subscribers just after it commits
    // it: one committed before the runner listened can still reach it, and
    // must not wake it as new (the catch-up above, when on, has it).
    let listening_from = since;
    // Clarifies past their deadline already queued (ADR 0018 §3).
    let mut deadline_checked: Option<Instant> = None;
    let mut next_deadline: Option<u64> = None;
    let mut deadline_queued: std::collections::HashSet<String> = Default::default();
    let mut last_run: Option<Instant> = None;
    let mut last_heartbeat = Instant::now();
    let mut runs: VecDeque<Instant> = VecDeque::new();
    let mut running: Option<Running> = None;
    let mut capped = false;
    let mut paused_until: Option<u64> = None;
    let openclaw = config.runtime == "openclaw";
    // OpenClaw: the gateway refused caller session keys (for the process).
    let isolated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // OpenClaw: the gateway was down at the last run (logged once).
    let mut gateway_down = false;
    // OpenClaw: no run before this (`Retry-After`).
    let mut backoff_until: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(shutdown);
    info!(runtime = %config.runtime, profile = %config.profile, author = %me, "agent runner started");
    if openclaw {
        info!(
            gateway = %crate::openclaw::redacted_url(&config.openclaw.url), agent_id = %config.openclaw.agent,
            "agent runs are forwarded to OpenClaw (POST /hooks/agent)"
        );
        info!("{}", crate::openclaw::SPEND_CAPS);
        if crate::openclaw::read_token(&config.openclaw.token_file).is_err() {
            warn!(
                "no OpenClaw hook token in {}: runs will fail until `wats setup openclaw --agent`",
                config.openclaw.token_file.display()
            );
        }
    }

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                if let Some(run) = running.take() {
                    finish_run(&node, &config, run, Outcome::Stopped).await;
                }
                break;
            }
            outcome = wait_running(&mut running) => {
                if let Some(mut run) = running.take() {
                    let woke = std::mem::take(&mut run.woke);
                    let taken = std::mem::take(&mut run.feedback);
                    match finish_run(&node, &config, run, outcome).await {
                        Finished::Deferred(why) => {
                            // Not a run: it does not count, and what woke
                            // it waits for the next one.
                            runs.pop_back();
                            if !gateway_down {
                                warn!(
                                    "agent run deferred: {why}; {} events kept for the next run",
                                    woke.len()
                                );
                                gateway_down = true;
                            }
                            let mut queue: Vec<Queued> = woke
                                .into_iter()
                                .map(|q| Queued { held: None, ..q })
                                .collect();
                            queue.retain(|q| !pending.iter().any(|p| p.id == q.id));
                            queue.append(&mut pending);
                            pending = queue;
                            for id in taken.into_iter().rev() {
                                if !feedback.contains(&id) {
                                    feedback.insert(0, id);
                                }
                            }
                        }
                        Finished::Backoff(after) => {
                            gateway_down = false;
                            warn!("agent runs paused for {} s (the gateway's Retry-After)", after.as_secs());
                            backoff_until = Some(Instant::now() + after);
                        }
                        Finished::Done => {
                            if std::mem::take(&mut gateway_down) {
                                info!("the OpenClaw gateway answers again");
                            }
                        }
                    }
                }
            }
            event = events.recv() => match event {
                Ok(stored) if stored.seq <= listening_from => {
                    debug!(event = short(&stored.event.id), "not for the agent: stored before the runner listened");
                }
                Ok(stored) => {
                    let event = &stored.event;
                    // A claim or an accept may settle the agent's own claim
                    // (ADR 0022 §2.3); any post may free a held ask.
                    let accept = event.kind == Kind::React && event.data.contains_key("accept");
                    if event.thread.is_some() && (event.kind == Kind::Claim || accept) {
                        settle_claims(&node, &me, &config, event.thread_root(), &mut released).await;
                    }
                    // History (a fill, a catch-up, a fetch on read) wakes
                    // the agent only within `agent.catchup`, by `ts`, as
                    // what it missed before it listened, frees no held ask
                    // and is no feedback (ADR 0025 §4).
                    let window = config.catchup.unwrap_or(DEFAULT_CATCHUP).as_secs();
                    let history = unix_now().saturating_sub(event.ts) > window;
                    if !history && parked.values().any(|p| p.queued.thread == event.thread_root()) {
                        touched.insert(event.thread_root().to_string());
                    }
                    match interest(&node, &me, &config.allow, config.ignore_local, event) {
                        Interest::Wake(_) | Interest::Feedback(_) if history => {
                            debug!(event = short(&event.id), kind = event.kind.as_str(), "not for the agent: history older than the catch-up window");
                        }
                        Interest::Wake(why) => {
                            if !pending.iter().any(|q| q.id == event.id) {
                                info!(event = short(&event.id), kind = event.kind.as_str(), "queued for the agent: {why}");
                                pending.push(Queued::new(event, why, event.author.clone()));
                            }
                        }
                        Interest::Feedback(why) => {
                            info!(event = short(&event.id), kind = event.kind.as_str(), "kept for the next run: {why}");
                            if !feedback.contains(&event.id) {
                                feedback.push(event.id.clone());
                            }
                        }
                        Interest::Ignore(why) => {
                            debug!(event = short(&event.id), kind = event.kind.as_str(), "not for the agent: {why}");
                        }
                    }
                }
                Err(RecvError::Lagged(n)) => warn!("agent runner missed {n} events"),
                Err(RecvError::Closed) => break,
            },
            _ = tick.tick(), if running.is_none() => {
                let now = Instant::now();
                while runs.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(3600)) {
                    runs.pop_front();
                }
                let unix_now = unix_now();
                wakes.prune(unix_now);
                if backoff_until.is_some_and(|t| now < t) {
                    continue;
                }
                // Every 30 s (new clarifies), and at the soonest deadline.
                if deadline_checked.is_none_or(|t| now.duration_since(t) >= DEADLINE_CHECK_EVERY)
                    || next_deadline.is_some_and(|d| unix_now >= d)
                {
                    deadline_checked = Some(now);
                    let (due, next) = clarifies_past_deadline(&node, &me, unix_now);
                    next_deadline = next;
                    for (clarify, asker) in due {
                        if deadline_queued.insert(clarify.id.clone()) {
                            info!(event = short(&clarify.id), "queued for the agent: {WHY_DEADLINE}");
                            pending.push(Queued::new(&clarify, WHY_DEADLINE, asker));
                        }
                    }
                }
                delivery_pass(&node, &me, &config, &mut pending, &mut parked, &mut touched, unix_now);
                // The end of a jitter comes before the next tick.
                if let Some(soonest) = pending.iter().filter_map(|q| q.not_before).filter(|t| *t > now).min()
                    && soonest < now + Duration::from_secs(1)
                {
                    tick.reset_at(tokio::time::Instant::from_std(soonest));
                }
                let debounced = last_run.is_none_or(|t| now.duration_since(t) >= config.every);
                // Events stay queued until a run actually starts with them.
                // An ask addressed to the agent bypasses the debounce.
                let urgent = pending.iter().any(|q| q.urgent);
                let mut ready = if debounced || urgent {
                    ready(&node, &me, &mut pending, &wakes, &config.wakes, unix_now)
                } else {
                    Vec::new()
                };
                if !debounced {
                    ready.retain(|(id, _)| pending.iter().any(|q| &q.id == id && q.urgent));
                }
                // OpenClaw: one session per thread, so one thread per run;
                // the others wait for the next run.
                let thread = if openclaw && !ready.is_empty() {
                    let root = pending
                        .iter()
                        .find(|q| q.id == ready[0].0)
                        .map(|q| q.thread.clone());
                    ready.retain(|(id, _)| {
                        pending.iter().any(|q| &q.id == id && Some(&q.thread) == root.as_ref())
                    });
                    root
                } else {
                    None
                };
                let trigger = if !ready.is_empty() {
                    Some(Trigger::Events(ready))
                } else if now.duration_since(last_heartbeat) >= config.heartbeat {
                    Some(Trigger::Heartbeat)
                } else {
                    None
                };
                let Some(trigger) = trigger else { continue };
                if runs.len() >= config.max_runs_per_hour {
                    if !capped {
                        warn!(
                            "agent run skipped: {} runs in the last hour (cap {}); {} pending events kept",
                            runs.len(), config.max_runs_per_hour, pending.len()
                        );
                        capped = true;
                    }
                    last_heartbeat = now;
                    continue;
                }
                capped = false;
                let spend = budget::spend(&config.work_dir, unix_now);
                let (remaining_usd, remaining_secs, remaining_tokens) =
                    match budget::check(&config.budget, &spend, unix_now) {
                    budget::Verdict::Paused { why, until } => {
                        if paused_until != Some(until) {
                            warn!("agent paused, budget reached: {why}; {}", budget::until(until));
                            paused_until = Some(until);
                        }
                        last_heartbeat = now;
                        continue;
                    }
                    budget::Verdict::Go {
                        remaining_usd,
                        remaining_secs,
                        remaining_tokens,
                    } => {
                        if paused_until.take().is_some() {
                            info!(
                                "agent budget available again: {}",
                                budget::describe(&config.budget, &spend, &config.runtime)
                            );
                        }
                        (remaining_usd, remaining_secs, remaining_tokens)
                    }
                };
                // The run's timeout, within the minutes left today and this
                // month (at least one: `check` starts no run with less).
                let timeout = run_timeout(config.run_timeout, remaining_secs);
                let clamped;
                let run_config = if timeout < config.run_timeout {
                    info!(
                        "agent run timeout {}s, the run time left in its budget (agent.timeout {}s)",
                        timeout.as_secs(),
                        config.run_timeout.as_secs()
                    );
                    clamped = AgentConfig {
                        run_timeout: timeout,
                        ..config.clone()
                    };
                    &clamped
                } else {
                    &config
                };
                let mut text = prompt(&node, &config, since, &trigger);
                if let Trigger::Events(ids) = &trigger {
                    text.push_str(&delivery_section(&node, &me, ids, unix_now));
                }
                // A heartbeat also offers a few older unanswered asks.
                if let (Trigger::Heartbeat, Some(window)) = (&trigger, config.catchup) {
                    let offered = read_offered(&config.work_dir);
                    let older = older_asks(&node, &me, &config, window, &offered, unix_now);
                    if !older.is_empty() {
                        let ids: Vec<String> = older.iter().map(|e| e.id.clone()).collect();
                        info!("heartbeat: offering {} nobody answered", count(ids.len(), "older ask", "older asks"));
                        text.push_str(&format!(
                            "\n\nOlder asks nobody answered (more than {} old; optional: help \
                             only where you can add real value, otherwise skip them):\n{}\n",
                            window_text(window),
                            event_lines(&node, &ids).join("\n")
                        ));
                        save_offered(&config.work_dir, offered, &ids);
                    }
                }
                let taken = std::mem::take(&mut feedback);
                text.push_str(&feedback_section(&node, &taken));
                // What the human said about the posts they rejected.
                let feedback = crate::review::take_feedback(&config.data_dir, &config.profile);
                if !feedback.is_empty() {
                    text.push_str("\n\nFeedback from your human on your held posts:\n");
                    text.push_str(&feedback.join("\n"));
                    text.push('\n');
                }
                // Everything up to now is covered by this run.
                since = node.store().last_seq()?;
                write_cursor(&config.work_dir, since);
                let mut woke = Vec::new();
                if let Trigger::Events(ids) = &trigger {
                    let ids: std::collections::HashSet<&str> =
                        ids.iter().map(|(id, _)| id.as_str()).collect();
                    let kept;
                    (woke, kept) = std::mem::take(&mut pending)
                        .into_iter()
                        .partition(|q| ids.contains(q.id.as_str()));
                    pending = kept;
                    wakes.record(woke.iter().map(|q| q.author.as_str()), unix_now);
                }
                runs.push_back(now);
                last_run = Some(now);
                last_heartbeat = now;
                info!(trigger = %trigger.label(), "waking the agent");
                let started = if openclaw {
                    start_forward(run_config, &trigger, &text, thread.as_deref(), &isolated)
                } else {
                    start_run(&node, run_config, &trigger, text, remaining_usd, remaining_tokens)
                        .await
                };
                match started {
                    Ok(mut run) => {
                        // Charged its caps if it never ends (ADR 0034 §6).
                        let cap = budget::Usage {
                            usd: match config.runtime.as_str() {
                                "vibe" => run.charge_usd,
                                // OpenCode's dollars are held to its
                                // estimate, with no per-run cap: what is
                                // left, as for Claude Code's.
                                "claude" | "opencode" => remaining_usd,
                                _ => None,
                            },
                            secs: Some(timeout.as_secs_f64()),
                            tokens: None,
                        };
                        if let Err(e) =
                            budget::begin(&config.work_dir, run.record.started, &run.record.id, cap)
                        {
                            warn!("agent spend ledger: {e}");
                        }
                        run.woke = woke;
                        run.feedback = taken;
                        running = Some(run);
                    }
                    Err(e) => warn!(trigger = %trigger.label(), "agent run failed to start: {e:#}"),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn caps_become_lowercase_tags() {
        use super::{cap_tag, check_cap};
        assert_eq!(cap_tag("pdf"), "cap:pdf");
        assert_eq!(cap_tag("Code-Review"), "cap:code-review");
        assert_eq!(cap_tag("Lang:EN"), "lang:en");
        assert_eq!(cap_tag("\u{c9}t\u{e9}"), "cap:\u{c9}t\u{e9}", "ASCII only");
        assert!(check_cap("Code-Review").is_ok());
        assert!(
            check_cap("\u{c9}t\u{e9}").is_err(),
            "non-ASCII uppercase is not mapped"
        );
        assert!(check_cap("rust lang").is_err());
    }

    #[test]
    fn jitter_stays_in_its_range() {
        use super::jitter;
        use std::time::Duration;
        let ms = Duration::from_millis;
        assert_eq!(jitter(&(Duration::ZERO..=Duration::ZERO)), Duration::ZERO);
        assert_eq!(jitter(&(ms(1500)..=ms(1500))), ms(1500), "fixed");
        assert_eq!(jitter(&(ms(900)..=ms(100))), ms(900), "empty: its start");
        for _ in 0..200 {
            let d = jitter(&(ms(200)..=ms(300)));
            assert!((ms(200)..=ms(300)).contains(&d), "{d:?}");
        }
    }

    #[test]
    fn heartbeat_opens_like_the_mcp_instructions() {
        let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(words(super::DEFAULT_HEARTBEAT).starts_with(&words(crate::mcp::OPENING)));
    }

    /// Only the user acknowledges changed safety settings: nothing the
    /// agent is woken with names the command.
    #[test]
    fn wake_prompt_never_names_the_ack() {
        for text in [
            super::DEFAULT_HEARTBEAT,
            crate::mcp::OPENING,
            crate::openclaw::PREAMBLE,
            &crate::openclaw::message("wake"),
        ] {
            assert!(!text.contains("config ack"), "{text}");
        }
    }
}
