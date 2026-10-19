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
use crate::event::{Draft, Event, Kind, title_from_text};
use crate::identity::author_id;
use crate::node::Node;
use crate::runs::{self, RunRecord};
use crate::store::Filter;
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
    /// What the agent may use besides the wats tools.
    pub access: Access,
    /// Its working directory with `workspace` access.
    pub workdir: PathBuf,
    /// Built-in tools allowed with `workspace` access (Claude tool names;
    /// for Codex, features kept on).
    pub tools: Vec<String>,
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
    /// Its budget (USD caps unset for OpenClaw: its own).
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

    /// Today's budget: `$0.23 of $2.00 today, …`, or the runs for
    /// runtimes that report no cost.
    pub fn budget_line(&self, now: u64) -> String {
        let spend = budget::spend(&self.work_dir, now);
        match self.runtime.as_deref() {
            Some("openclaw") => format!(
                "{} ({} of {} runs today)",
                crate::openclaw::BUDGET,
                spend.uncosted_today,
                self.budget.max_runs_per_day
            ),
            Some("codex") => format!(
                "{} of {} runs today (Codex reports no cost)",
                spend.uncosted_today, self.budget.max_runs_per_day
            ),
            _ => budget::describe(&self.budget, &spend),
        }
    }

    /// For `status`: name, runtime, profile, state, last run, budget.
    pub fn status(&self, now: u64) -> serde_json::Value {
        json!({
            "name": self.name,
            "runtime": self.runtime,
            "profile": self.profile,
            "state": self.state(),
            "last_run": runs::list(&self.work_dir).last().map(|r| runs::format_time(r.started)),
            "budget": self.budget_line(now),
        })
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
    let mine = Filter {
        kinds: vec![Kind::Clarify],
        author: Some(me.to_string()),
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
    // (another profile) do not wake it.
    if ignore_local && node.is_local_author(&event.author) {
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
    let access = access::prompt_line(
        &config.runtime,
        config.access,
        &config.workdir,
        &config.tools,
    );
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
}

/// What a run waits on: the agent CLI, or the forward to OpenClaw.
enum Work {
    Child(tokio::process::Child),
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
/// turns, and the budget left (Claude Code's `--max-budget-usd`).
fn limit_args(config: &AgentConfig, remaining_usd: f64) -> Vec<String> {
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
    if !given("--max-budget-usd") {
        args.extend([
            "--max-budget-usd".to_string(),
            format!("{:.2}", remaining_usd.max(0.01)),
        ]);
    }
    args
}

/// Where a run starts: an empty directory (locked), the workspace, or the
/// directory `wats` was started in (inherit).
fn run_dir(config: &AgentConfig) -> Result<PathBuf> {
    Ok(match config.access {
        Access::Locked => {
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
    node: &Node,
    config: &AgentConfig,
    trigger: &Trigger,
    prompt: String,
    remaining_usd: f64,
) -> Result<Running> {
    let (record, transcript) = open_run(config, trigger)?;
    let file = std::fs::OpenOptions::new().append(true).open(&transcript)?;
    let token = node.new_runner_token(crate::node::RunnerRun {
        id: record.id.clone(),
        events: trigger.events(),
        profile: config.profile.clone(),
    });
    let mut command = tokio::process::Command::new(&config.command);
    let dir = run_dir(config)?;
    command.current_dir(&dir);
    match config.runtime.as_str() {
        "codex" => {
            let (_, workspace_overrides) = if config.access == Access::Workspace {
                access::workspace_codex_overrides(&config.workdir)?
            } else {
                Default::default()
            };
            // `--model` is an option of `codex exec`: it goes after it.
            let mut extra = model_args(config, "--model");
            extra.extend(config.extra_args.iter().cloned());
            command.args(access::codex_args(&access::CodexRun {
                access: config.access,
                wats_command: &config.mcp_command,
                wats_args: &config.mcp_args,
                runner_token: &token,
                disable: &config.codex_disable,
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
                &config.mcp_command,
                &config.mcp_args,
                &token,
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
    let spawned = command
        .stdin(Stdio::piped())
        .stdout(file.try_clone()?)
        .stderr(file)
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            fail_run(config, record, &transcript, &e.to_string());
            node.end_runner_token(&token);
            return Err(e).with_context(|| format!("start {}", config.command.display()));
        }
    };
    // The prompt goes on stdin: --mcp-config and --allowedTools take several
    // values and would swallow a positional prompt.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }
    Ok(Running {
        record,
        token: Some(token),
        work: Work::Child(child),
        started: Instant::now(),
        deadline: tokio::time::Instant::now() + config.run_timeout,
        transcript,
        woke: Vec::new(),
        feedback: Vec::new(),
    })
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
    running: Running,
    outcome: Outcome,
) -> Finished {
    use crate::openclaw::Reply;
    if let Some(token) = &running.token {
        node.end_runner_token(token);
    }
    match running.work {
        Work::Child(mut child) => {
            if !matches!(outcome, Outcome::Exited(_)) {
                let _ = child.kill().await;
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
    runs::read_result(&running.transcript, &mut record);
    // A forward the gateway refused ran no turn.
    if record.status != "failed"
        && let Err(e) = crate::budget::record(&config.work_dir, record.started, record.cost_usd)
    {
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
        Work::Child(child) => match tokio::time::timeout_at(running.deadline, child.wait()).await {
            Ok(Ok(status)) => Outcome::Exited(status.code()),
            Ok(Err(_)) => Outcome::Exited(None),
            Err(_) => Outcome::TimedOut,
        },
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
    for queued in queue.iter_mut() {
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

/// Run the agent loop until `shutdown` resolves. A run in progress is then
/// killed.
pub async fn run(
    node: Arc<Node>,
    config: AgentConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let me = author_id(&node.author_key(&config.profile)?);
    std::fs::create_dir_all(&config.work_dir)?;
    let mut config = config;
    if config.runtime == "codex" && config.access != Access::Inherit {
        let known = access::codex_features(&config.command).await;
        config.codex_disable = access::CODEX_DENIED_FEATURES
            .iter()
            .filter(|f| known.iter().any(|k| k == *f))
            .filter(|f| config.access != Access::Workspace || !config.tools.iter().any(|t| t == *f))
            .map(|f| f.to_string())
            .collect();
        // Never run Codex without knowing what to switch off.
        anyhow::ensure!(
            !known.is_empty(),
            "`{} features list` gave nothing: cannot lock the Codex agent down",
            config.command.display()
        );
    }
    announce_if_changed(&node, &config).await;
    // A marker left by a run that did not finish (crash, kill -9).
    let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
    let mut events = node.store().subscribe();
    let mut pending: Vec<Queued> = Vec::new();
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
            pending.push(Queued {
                id: event.id.clone(),
                why,
                author: event.author.clone(),
                thread: event.thread_root().to_string(),
                held: None,
            });
        }
    }
    write_cursor(&config.work_dir, since);
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
            gateway = %config.openclaw.url, agent_id = %config.openclaw.agent,
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
                Ok(stored) => {
                    let event = &stored.event;
                    match interest(&node, &me, &config.allow, config.ignore_local, event) {
                        Interest::Wake(why) => {
                            if !pending.iter().any(|q| q.id == event.id) {
                                info!(event = short(&event.id), kind = event.kind.as_str(), "queued for the agent: {why}");
                                pending.push(Queued {
                                    id: event.id.clone(),
                                    why,
                                    author: event.author.clone(),
                                    thread: event.thread_root().to_string(),
                                    held: None,
                                });
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
                            pending.push(Queued {
                                id: clarify.id.clone(),
                                why: WHY_DEADLINE,
                                author: asker,
                                thread: clarify.thread_root().to_string(),
                                held: None,
                            });
                        }
                    }
                }
                let debounced = last_run.is_none_or(|t| now.duration_since(t) >= config.every);
                // Events stay queued until a run actually starts with them.
                let mut ready = if debounced {
                    ready(&node, &me, &mut pending, &wakes, &config.wakes, unix_now)
                } else {
                    Vec::new()
                };
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
                let remaining_usd = match budget::check(&config.budget, &spend, unix_now) {
                    budget::Verdict::Paused { why, until } => {
                        if paused_until != Some(until) {
                            warn!("agent paused, budget reached: {why}; {}", budget::until(until));
                            paused_until = Some(until);
                        }
                        last_heartbeat = now;
                        continue;
                    }
                    budget::Verdict::Go { remaining_usd } => {
                        if paused_until.take().is_some() {
                            info!("agent budget available again: {}", budget::describe(&config.budget, &spend));
                        }
                        remaining_usd
                    }
                };
                let mut text = prompt(&node, &config, since, &trigger);
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
                    start_forward(&config, &trigger, &text, thread.as_deref(), &isolated)
                } else {
                    start_run(&node, &config, &trigger, text, remaining_usd).await
                };
                match started {
                    Ok(mut run) => {
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
