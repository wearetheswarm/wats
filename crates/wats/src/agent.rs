//! A minimal runner (SPEC §2.8, "runner spawn", heartbeat model): the node
//! wakes an agent runtime (Claude Code, headless) when something of
//! interest arrives, and once per heartbeat. The agent acts only through the
//! node's MCP tools; wats never posts on its behalf.
//!
//! Waking is cheap when idle: the node watches incoming events itself and
//! only spawns the agent when there is something new (debounced), or when
//! the heartbeat is due. One run at a time, each with a timeout, and a cap
//! on runs per hour. Every run leaves a transcript (see [`crate::runs`]).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::event::{Event, Kind};
use crate::identity::author_id;
use crate::node::Node;
use crate::runs::{self, RunRecord};
use crate::store::Filter;

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
    /// Debounce: at most one event-triggered run per this interval.
    pub every: Duration,
    /// A run even with nothing new, every this interval.
    pub heartbeat: Duration,
    pub max_runs_per_hour: usize,
    /// When non-empty, only events by these authors (hex keys) are of interest.
    pub allow: Vec<String>,
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
    Events(Vec<String>),
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

    fn events(&self) -> &[String] {
        match self {
            Trigger::Events(ids) => ids,
            Trigger::Heartbeat => &[],
        }
    }
}

/// What the runner makes of a new event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    Wake(&'static str),
    Ignore(&'static str),
}

/// Whether a new event should wake the agent, and why: not its own; from an
/// allowed author if a list is set; an `ask`, or a reply in a thread the
/// agent posted in.
pub fn interest(node: &Node, me: &str, allow: &[String], event: &Event) -> Interest {
    if event.author == me {
        return Interest::Ignore("own post");
    }
    if !allow.is_empty() && !allow.iter().any(|a| a == &event.author) {
        return Interest::Ignore("author not in --allow");
    }
    if event.kind == Kind::Ask {
        return Interest::Wake("ask");
    }
    let Some(root) = &event.thread else {
        return Interest::Ignore("not an ask, not a reply");
    };
    let mine = Filter {
        thread: Some(root.clone()),
        author: Some(me.to_string()),
        ..Filter::default()
    };
    match node.store().search(None, &mine, 1) {
        Ok(found) if !found.is_empty() => {
            Interest::Wake("reply in a thread where the agent posted")
        }
        _ => Interest::Ignore("reply in a thread the agent is not part of"),
    }
}

pub fn is_interesting(node: &Node, me: &str, allow: &[String], event: &Event) -> bool {
    matches!(interest(node, me, allow, event), Interest::Wake(_))
}

/// The prompt of one run: the heartbeat text, then what is new.
pub fn prompt(config: &AgentConfig, me: &str, since: i64, trigger: &Trigger) -> String {
    let mut prompt = config.heartbeat_text.trim_end().to_string();
    prompt.push_str("\n\n---\n");
    prompt.push_str(&format!("Your author id: {me}\n"));
    prompt.push_str(&format!(
        "Cursor: events newer than `since={since}` (pass it to `wait` or compare with `search`).\n"
    ));
    match trigger {
        Trigger::Events(ids) => {
            prompt.push_str("New events of interest (open them with `read`):\n");
            for id in ids.iter().take(MAX_IDS_IN_PROMPT) {
                prompt.push_str(&format!("- {id}\n"));
            }
            if ids.len() > MAX_IDS_IN_PROMPT {
                prompt.push_str(&format!(
                    "- … and {} more: use `search`\n",
                    ids.len() - MAX_IDS_IN_PROMPT
                ));
            }
        }
        Trigger::Heartbeat => {
            prompt.push_str("Heartbeat: nothing specific woke you. Look around briefly.\n");
        }
    }
    prompt
}

/// Write the MCP config the agent CLI is started with.
fn write_mcp_config(config: &AgentConfig) -> Result<PathBuf> {
    std::fs::create_dir_all(&config.work_dir)?;
    let path = config.work_dir.join(format!("mcp-{}.json", config.profile));
    let json = json!({
        "mcpServers": {
            "wats": {
                "type": "stdio",
                "command": config.mcp_command,
                "args": config.mcp_args,
            }
        }
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&json)?)?;
    Ok(path)
}

/// A run in progress.
struct Running {
    record: RunRecord,
    child: tokio::process::Child,
    started: Instant,
    deadline: tokio::time::Instant,
    transcript: PathBuf,
}

/// Start one run: a transcript header, then the agent CLI with its output
/// (stream-json) appended to the transcript.
async fn start_run(
    config: &AgentConfig,
    mcp_config: &PathBuf,
    trigger: &Trigger,
    prompt: String,
) -> Result<Running> {
    let mut record = RunRecord::new(
        &config.runtime,
        &config.profile,
        trigger.name(),
        trigger.events(),
    );
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
    let spawned = tokio::process::Command::new(&config.command)
        .arg("-p")
        .args(["--output-format", "stream-json", "--verbose"])
        .arg("--mcp-config")
        .arg(mcp_config)
        .arg("--strict-mcp-config")
        .arg("--allowedTools")
        .arg("mcp__wats__*")
        .args(&config.extra_args)
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
async fn finish_run(config: &AgentConfig, mut running: Running, outcome: Outcome) {
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

/// Run the agent loop until `shutdown` resolves. A run in progress is then
/// killed.
pub async fn run(
    node: Arc<Node>,
    config: AgentConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let me = author_id(&node.author_key(&config.profile)?);
    let mcp_config = write_mcp_config(&config)?;
    // A marker left by a run that did not finish (crash, kill -9).
    let _ = std::fs::remove_file(runs::current_path(&config.work_dir));
    let mut events = node.store().subscribe();
    let mut pending: Vec<String> = Vec::new();
    let mut since = node.store().last_seq()?;
    let mut last_run: Option<Instant> = None;
    let mut last_heartbeat = Instant::now();
    let mut runs: VecDeque<Instant> = VecDeque::new();
    let mut running: Option<Running> = None;
    let mut capped = false;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(shutdown);
    info!(runtime = %config.runtime, profile = %config.profile, author = %me, "agent runner started");

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                if let Some(run) = running.take() {
                    finish_run(&config, run, Outcome::Stopped).await;
                }
                break;
            }
            outcome = wait_running(&mut running) => {
                if let Some(run) = running.take() {
                    finish_run(&config, run, outcome).await;
                }
            }
            event = events.recv() => match event {
                Ok(stored) => {
                    let event = &stored.event;
                    match interest(&node, &me, &config.allow, event) {
                        Interest::Wake(why) => {
                            if !pending.contains(&event.id) {
                                info!(event = short(&event.id), kind = event.kind.as_str(), "queued for the agent: {why}");
                                pending.push(event.id.clone());
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
                let debounced = last_run.is_none_or(|t| now.duration_since(t) >= config.every);
                let trigger = if !pending.is_empty() && debounced {
                    Some(Trigger::Events(std::mem::take(&mut pending)))
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
                            runs.len(), config.max_runs_per_hour, pending.len() + trigger.events().len()
                        );
                        capped = true;
                    }
                    if let Trigger::Events(ids) = trigger {
                        pending = ids;
                    }
                    last_heartbeat = now;
                    continue;
                }
                capped = false;
                let text = prompt(&config, &me, since, &trigger);
                // Everything up to now is covered by this run.
                since = node.store().last_seq()?;
                runs.push_back(now);
                last_run = Some(now);
                last_heartbeat = now;
                info!(trigger = %trigger.label(), "waking the agent");
                match start_run(&config, &mcp_config, &trigger, text).await {
                    Ok(run) => running = Some(run),
                    Err(e) => warn!(trigger = %trigger.label(), "agent run failed to start: {e:#}"),
                }
            }
        }
    }
    Ok(())
}
