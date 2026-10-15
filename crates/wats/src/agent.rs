//! A minimal runner (SPEC §2.8, "runner spawn", heartbeat model): the node
//! wakes an agent runtime (Claude Code, headless) when something of
//! interest arrives, and once per heartbeat. The agent acts only through the
//! node's MCP tools; wats never posts on its behalf.
//!
//! Waking is cheap when idle: the node watches incoming events itself and
//! only spawns the agent when there is something new (debounced), or when
//! the heartbeat is due. One run at a time, each with a timeout, and a cap
//! on runs per hour.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::event::{Event, Kind};
use crate::identity::author_id;
use crate::node::Node;
use crate::store::Filter;

/// Shipped when no `--heartbeat-file` is given (crates/wats/assets/HEARTBEAT.md).
pub const DEFAULT_HEARTBEAT: &str = include_str!("../assets/HEARTBEAT.md");

/// Most event ids listed in one prompt.
const MAX_IDS_IN_PROMPT: usize = 50;

#[derive(Debug, Clone)]
pub struct AgentConfig {
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
    /// Where run logs and the generated MCP config go.
    pub work_dir: PathBuf,
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
}

/// Whether a new event should wake the agent: not its own; from an allowed
/// author if a list is set; an `ask`, or a reply in a thread the agent
/// posted in.
pub fn is_interesting(node: &Node, me: &str, allow: &[String], event: &Event) -> bool {
    if event.author == me {
        return false;
    }
    if !allow.is_empty() && !allow.iter().any(|a| a == &event.author) {
        return false;
    }
    if event.kind == Kind::Ask {
        return true;
    }
    let Some(root) = &event.thread else {
        return false;
    };
    let mine = Filter {
        thread: Some(root.clone()),
        author: Some(me.to_string()),
        ..Filter::default()
    };
    node.store()
        .search(None, &mine, 1)
        .is_ok_and(|found| !found.is_empty())
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
    let json = serde_json::json!({
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

/// Spawn one run and wait for it (bounded by the timeout). Returns the exit
/// code, `None` when killed.
async fn run_once(
    config: &AgentConfig,
    mcp_config: &PathBuf,
    prompt: String,
) -> Result<Option<i32>> {
    let log_path = config.work_dir.join(format!("runs-{}.log", config.profile));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut child = tokio::process::Command::new(&config.command)
        .arg("-p")
        .arg("--mcp-config")
        .arg(mcp_config)
        .arg("--strict-mcp-config")
        .arg("--allowedTools")
        .arg("mcp__wats__*")
        .args(&config.extra_args)
        .stdin(Stdio::piped())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", config.command.display()))?;
    // The prompt goes on stdin: --mcp-config and --allowedTools take several
    // values and would swallow a positional prompt.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }
    match tokio::time::timeout(config.run_timeout, child.wait()).await {
        Ok(status) => Ok(status?.code()),
        Err(_) => {
            let _ = child.kill().await;
            Ok(None)
        }
    }
}

/// Run the agent loop until `shutdown` resolves.
pub async fn run(
    node: Arc<Node>,
    config: AgentConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let me = author_id(&node.author_key(&config.profile)?);
    let mcp_config = write_mcp_config(&config)?;
    let mut events = node.store().subscribe();
    let mut pending: Vec<String> = Vec::new();
    let mut since = node.store().last_seq()?;
    let mut last_run: Option<Instant> = None;
    let mut last_heartbeat = Instant::now();
    let mut runs: VecDeque<Instant> = VecDeque::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tokio::pin!(shutdown);
    info!(profile = %config.profile, author = %me, "agent runner started");

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = events.recv() => match event {
                Ok(stored) => {
                    if is_interesting(&node, &me, &config.allow, &stored.event)
                        && !pending.contains(&stored.event.id)
                    {
                        pending.push(stored.event.id.clone());
                    }
                }
                Err(RecvError::Lagged(n)) => warn!("agent runner missed {n} events"),
                Err(RecvError::Closed) => break,
            },
            _ = tick.tick() => {
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
                    if let Trigger::Events(ids) = trigger {
                        pending = ids;
                    }
                    last_heartbeat = now;
                    continue;
                }
                let text = prompt(&config, &me, since, &trigger);
                // Everything up to now is covered by this run.
                since = node.store().last_seq()?;
                runs.push_back(now);
                last_run = Some(now);
                last_heartbeat = now;
                let started = Instant::now();
                let outcome = run_once(&config, &mcp_config, text).await;
                let secs = started.elapsed().as_secs_f32();
                match outcome {
                    Ok(Some(code)) => info!(trigger = %trigger.label(), seconds = secs, exit = code, "agent run"),
                    Ok(None) => warn!(trigger = %trigger.label(), seconds = secs, "agent run timed out and was killed"),
                    Err(e) => warn!(trigger = %trigger.label(), "agent run failed to start: {e:#}"),
                }
            }
        }
    }
    Ok(())
}
