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
    /// Where transcripts, the runs index and the generated MCP config go
    /// (`<data dir>/agent`).
    pub work_dir: PathBuf,
    /// Retention: transcripts beyond the newest `keep_runs`, or older than
    /// `keep_days`, are deleted.
    pub keep_runs: usize,
    pub keep_days: u64,
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
/// `{reasons}`, `{cursor}` and `{events}` (one line per event: id · kind ·
/// author · title). A template without `{events}` gets that context
/// appended.
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
            let replies = ids.len() - asks;
            let mut reasons = Vec::new();
            if asks > 0 {
                reasons.push(count(asks, "new ask", "new asks"));
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
    let template = config.heartbeat_text.trim_end();
    let mut prompt = template
        .replace("{agent_profile}", &config.profile)
        .replace("{user}", &config.user)
        .replace("{agent.about}", about)
        .replace("{reasons}", &reasons)
        .replace("{cursor}", &cursor)
        .replace("{events}", &events);
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
    /// Marks the run's MCP session as a runner session.
    token: String,
    child: tokio::process::Child,
    started: Instant,
    deadline: tokio::time::Instant,
    transcript: PathBuf,
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
    let token = node.new_runner_token(crate::node::RunnerRun {
        id: record.id.clone(),
        events: trigger.events(),
    });
    let mut command = tokio::process::Command::new(&config.command);
    command.current_dir(run_dir(config)?);
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
            record.status = "failed".into();
            record.error = Some(e.to_string());
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&transcript) {
                let _ = runs::write_line(&mut f, &record.footer());
            }
            let _ = runs::append_index(&config.work_dir, &record);
            let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
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
        token,
        child,
        started: Instant::now(),
        deadline: tokio::time::Instant::now() + config.run_timeout,
        transcript,
    })
}

/// How a run ended.
enum Outcome {
    Exited(Option<i32>),
    TimedOut,
    Stopped,
}

/// Close a run: kill it if needed, write the transcript footer and the
/// index record, prune old transcripts.
async fn finish_run(node: &Node, config: &AgentConfig, mut running: Running, outcome: Outcome) {
    node.end_runner_token(&running.token);
    if !matches!(outcome, Outcome::Exited(_)) {
        let _ = running.child.kill().await;
    }
    let mut record = running.record;
    record.seconds = Some(running.started.elapsed().as_secs_f64());
    match outcome {
        Outcome::Exited(code) => record.exit = code,
        Outcome::TimedOut => record.status = "timeout".into(),
        Outcome::Stopped => record.status = "stopped".into(),
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
    if let Err(e) = crate::budget::record(&config.work_dir, record.started, record.cost_usd) {
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
        status => {
            warn!(run = %record.id, seconds = record.seconds.unwrap_or(0.0), "agent run {status}")
        }
    }
    if let Err(e) = runs::prune(&config.work_dir, config.keep_runs, config.keep_days) {
        warn!("agent runs retention: {e}");
    }
}

async fn wait_running(running: &mut Option<Running>) -> Outcome {
    let Some(running) = running.as_mut() else {
        return std::future::pending().await;
    };
    match tokio::time::timeout_at(running.deadline, running.child.wait()).await {
        Ok(Ok(status)) => Outcome::Exited(status.code()),
        Ok(Err(_)) => Outcome::Exited(None),
        Err(_) => Outcome::TimedOut,
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
    let mut last_run: Option<Instant> = None;
    let mut last_heartbeat = Instant::now();
    let mut runs: VecDeque<Instant> = VecDeque::new();
    let mut running: Option<Running> = None;
    let mut capped = false;
    let mut paused_until: Option<u64> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(shutdown);
    info!(runtime = %config.runtime, profile = %config.profile, author = %me, "agent runner started");

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                if let Some(run) = running.take() {
                    finish_run(&node, &config, run, Outcome::Stopped).await;
                }
                break;
            }
            outcome = wait_running(&mut running) => {
                if let Some(run) = running.take() {
                    finish_run(&node, &config, run, outcome).await;
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
                let debounced = last_run.is_none_or(|t| now.duration_since(t) >= config.every);
                // Events stay queued until a run actually starts with them.
                let ready = if debounced {
                    ready(&node, &me, &mut pending, &wakes, &config.wakes, unix_now)
                } else {
                    Vec::new()
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
                text.push_str(&feedback_section(&node, &std::mem::take(&mut feedback)));
                // What the human said about the posts they rejected.
                if let Some(data_dir) = config.work_dir.parent() {
                    let feedback = crate::review::take_feedback(data_dir);
                    if !feedback.is_empty() {
                        text.push_str("\n\nFeedback from your human on your held posts:\n");
                        text.push_str(&feedback.join("\n"));
                        text.push('\n');
                    }
                }
                // Everything up to now is covered by this run.
                since = node.store().last_seq()?;
                if let Trigger::Events(ids) = &trigger {
                    let ids: std::collections::HashSet<&str> =
                        ids.iter().map(|(id, _)| id.as_str()).collect();
                    let (woke, kept): (Vec<Queued>, Vec<Queued>) = std::mem::take(&mut pending)
                        .into_iter()
                        .partition(|q| ids.contains(q.id.as_str()));
                    pending = kept;
                    wakes.record(woke.iter().map(|q| q.author.as_str()), unix_now);
                }
                runs.push_back(now);
                last_run = Some(now);
                last_heartbeat = now;
                info!(trigger = %trigger.label(), "waking the agent");
                match start_run(&node, &config, &trigger, text, remaining_usd).await {
                    Ok(run) => running = Some(run),
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
}
