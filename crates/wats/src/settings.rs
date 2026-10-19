//! `<data dir>/config.toml`: what a bare `wats` does, so that the daemon
//! runs as the user configured it. Optional. Precedence: flags, then
//! `WATS_*` environment variables, then this file, then defaults. Flags
//! never write it; `wats config set` and `wats setup` do.
//!
//! ```toml
//! profile = "alice"            # default profile of `wats mcp` sessions
//! infra = "wats"               # ours only; default: wats+n0
//!
//! [agents.claude]              # one table per agent (ADR 0021)
//! runtime = "claude"           # run the Claude agent with the node
//! profile = "alice-claude"     # default: <OS user>-<name>
//! about = "I review Rust code and explain compiler errors."
//! caps = ["rust", "code-review"]
//! every = "30s"
//! heartbeat = "30m"
//!
//! [agents.reviewer]            # a second agent, its own key and budget
//! runtime = "claude"
//! enabled = false              # kept, not run
//!
//! [agent]                      # node-wide agent keys
//! with_mcp = false
//! ```
//!
//! The old single `[agent]` table still loads: it is the agent named after
//! its runtime ([`crate::roster`]). `wats config set` and `wats setup`
//! rewrite it as `[agents.<runtime>]` ([`ConfigFile`]).
//!
//! [`KEYS`] lists every key; unknown keys are errors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const FILE_NAME: &str = "config.toml";

/// The agent runtimes wats can run.
pub const RUNTIMES: [&str; 3] = ["claude", "codex", "openclaw"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub profile: Option<String>,
    pub infra: Option<String>,
    pub port: Option<u16>,
    /// Networks to join; empty: `public`. `--net` replaces the list.
    #[serde(default)]
    pub nets: Vec<String>,
    #[serde(default)]
    pub bootstrap: Vec<String>,
    pub no_default_bootstrap: Option<bool>,
    pub max_blob_size: Option<String>,
    pub blob_quota: Option<String>,
    #[serde(default)]
    pub storage: StorageSettings,
    #[serde(default)]
    pub report: ReportSettings,
    #[serde(default)]
    pub limits: LimitsSettings,
    #[serde(default)]
    pub submit: SubmitSettings,
    /// The old single agent's keys, and the node-wide agent keys
    /// ([`NODE_WIDE_AGENT_KEYS`]).
    #[serde(default)]
    pub agent: AgentSettings,
    /// `[agents.<name>]`: one table per agent (ADR 0021).
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSettings>,
    /// Whether `[agent]` holds keys of an agent, not only node-wide ones
    /// (set by [`Settings::parse`]).
    #[serde(skip)]
    pub legacy_agent: bool,
    #[serde(default)]
    pub update: UpdateSettings,
    #[serde(default)]
    pub halt: HaltSettings,
    #[serde(default)]
    pub health: HealthSettings,
    #[serde(default)]
    pub ask: AskSettings,
    #[serde(default)]
    pub ui: UiSettings,
    #[serde(default)]
    pub openclaw: OpenclawSettings,
    #[serde(default)]
    pub notify: NotifySettings,
}

/// `[notify]`: what reaches the human at this machine.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySettings {
    /// A desktop notification when someone answers or clarifies one of
    /// your asks; unset: on with a desktop session only
    /// ([`crate::desktop::desktop_session`]).
    pub desktop: Option<bool>,
}

/// `[openclaw]`: what the node does for the human's OpenClaw (ADR 0018 §7).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenclawSettings {
    /// Push replies to the asks posted from OpenClaw to its main session.
    pub notify_asks: Option<bool>,
}

/// `[ui]`: what a foreground node prints for the person at the terminal.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSettings {
    pub banner: Option<bool>,
}

/// `[ask]`: the asker's side (ADR 0018 §4).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskSettings {
    pub unanswered_after: Option<String>,
}

/// `[health]`: infra health alerts (`crate::health`).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSettings {
    pub no_peers_after: Option<String>,
}

/// `[halt]`: guardian halts (ADR 0016).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaltSettings {
    /// `false` ignores guardian halts. A file key only: no flag, not in
    /// `wats setup` nor in the listed keys ([`HIDDEN_KEYS`]).
    pub accept: Option<bool>,
    /// Keep serving halt events instead of exiting on a halt (the router).
    pub serve: Option<bool>,
}

/// `[update]`: the daily release check and opt-in auto-upgrade
/// (`crate::update`).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettings {
    pub check: Option<bool>,
    pub auto: Option<bool>,
    pub delay: Option<String>,
    pub security_delay: Option<String>,
    pub url: Option<String>,
}

impl UpdateSettings {
    /// The configured policy over the defaults (check on, auto off, 7 days).
    pub fn policy(&self) -> Result<crate::update::UpdatePolicy> {
        let duration = |value: &Option<String>, what: &str| -> Result<Option<Duration>> {
            value
                .as_deref()
                .map(parse_duration)
                .transpose()
                .map_err(|e| anyhow::anyhow!("config.toml update.{what}: {e}"))
        };
        Ok(crate::update::UpdatePolicy {
            check: self.check.unwrap_or(true),
            auto: self.auto.unwrap_or(false),
            delay: match duration(&self.delay, "delay")? {
                Some(d) => d,
                None => parse_duration(crate::update::DEFAULT_DELAY).map_err(anyhow::Error::msg)?,
            },
            security_delay: duration(&self.security_delay, "security_delay")?,
            url: self.url.clone().filter(|u| !u.trim().is_empty()),
        })
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSettings {
    pub hello_per_min: Option<u32>,
    pub sync_per_min: Option<u32>,
    pub replicate_per_min: Option<u32>,
    pub report_per_min: Option<u32>,
    pub blob_fetches_per_peer: Option<usize>,
    pub challenge: Option<bool>,
    pub challenge_base_bits: Option<u32>,
    pub challenge_max_bits: Option<u32>,
}

impl LimitsSettings {
    /// The configured limits over the defaults.
    pub fn limits(&self) -> crate::limits::Limits {
        let d = crate::limits::Limits::default();
        crate::limits::Limits {
            hello_per_min: self.hello_per_min.unwrap_or(d.hello_per_min),
            sync_per_min: self.sync_per_min.unwrap_or(d.sync_per_min),
            replicate_per_min: self.replicate_per_min.unwrap_or(d.replicate_per_min),
            report_per_min: self.report_per_min.unwrap_or(d.report_per_min),
            blob_fetches_per_peer: self
                .blob_fetches_per_peer
                .unwrap_or(d.blob_fetches_per_peer)
                .max(1),
            challenge: self.challenge.unwrap_or(d.challenge),
            challenge_base_bits: self
                .challenge_base_bits
                .unwrap_or(d.challenge_base_bits)
                .clamp(1, crate::challenge::MAX_BITS),
            challenge_max_bits: self
                .challenge_max_bits
                .unwrap_or(d.challenge_max_bits)
                .clamp(1, crate::challenge::MAX_BITS),
            ..d
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportSettings {
    pub crashes: Option<bool>,
    pub collector: Option<String>,
    pub inbox: Option<String>,
    pub accept: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitSettings {
    pub retention: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSettings {
    pub max: Option<String>,
    /// Collect the blobs nothing keeps, hourly (default: true).
    pub gc: Option<bool>,
}

/// Default disk budget of the data directory (`storage.max`).
pub const DEFAULT_STORAGE_MAX: &str = "10G";

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    pub runtime: Option<String>,
    pub access: Option<String>,
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub tools: Vec<String>,
    pub model: Option<String>,
    pub review: Option<bool>,
    pub ignore_local: Option<bool>,
    pub review_ttl: Option<String>,
    pub notify: Option<bool>,
    pub profile: Option<String>,
    pub about: Option<String>,
    #[serde(default)]
    pub caps: Vec<String>,
    pub every: Option<String>,
    pub heartbeat: Option<String>,
    pub max_runs_per_hour: Option<usize>,
    pub wakes_per_author_per_hour: Option<usize>,
    pub loop_max_exchanges: Option<usize>,
    pub catchup: Option<String>,
    /// Removed keys ([`REMOVED_KEYS`]): still accepted, ignored, with a
    /// warning.
    #[serde(default)]
    pub thread_backoff: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub thread_backoff_max: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub allow: Vec<String>,
    pub heartbeat_file: Option<PathBuf>,
    pub command: Option<PathBuf>,
    #[serde(default)]
    pub args: Vec<String>,
    pub timeout: Option<String>,
    pub max_turns: Option<u32>,
    pub max_usd_per_day: Option<f64>,
    pub max_usd_per_month: Option<f64>,
    pub max_runs_per_day: Option<usize>,
    pub keep_runs: Option<usize>,
    pub keep_days: Option<u64>,
    /// The OpenClaw gateway (`runtime = "openclaw"`, ADR 0017).
    pub openclaw_url: Option<String>,
    /// Plain http to a private gateway address too (a Docker network),
    /// not only loopback.
    pub openclaw_insecure_ok: Option<bool>,
    /// The OpenClaw agent the runs go to (`agentId`).
    pub openclaw_agent: Option<String>,
    /// `[agents.<name>]` only: `false` keeps the table without running it.
    pub enabled: Option<bool>,
    /// `[agents.<name>]` only: the agent keeps `<data dir>/agent` as its
    /// directory, as the single agent did before ADR 0021 (written by the
    /// migration of the old `[agent]` table).
    pub legacy_dir: Option<bool>,
    /// `[agent]` only (node-wide): a background node started by `wats mcp`
    /// runs the enabled agents too.
    pub with_mcp: Option<bool>,
}

/// Keys of `[agent]` that are node-wide, not one agent's: they stay in
/// `[agent]` when the file has `[agents.<name>]` tables. The review queue
/// is the node's (`review_ttl`, `notify`).
pub const NODE_WIDE_AGENT_KEYS: [&str; 3] = ["with_mcp", "review_ttl", "notify"];

/// Keys of `[agents.<name>]` only.
pub const AGENT_ONLY_KEYS: [&str; 2] = ["enabled", "legacy_dir"];

impl AgentSettings {
    /// Check `openclaw_url` against `openclaw_insecure_ok`.
    pub fn check_openclaw_url(&self) -> Result<()> {
        if let Some(url) = &self.openclaw_url {
            crate::openclaw::check_url(url, self.openclaw_insecure_ok == Some(true))?;
        }
        Ok(())
    }
}

impl Settings {
    /// Read `<data dir>/config.toml`; no file means defaults.
    pub fn load(data_dir: &Path) -> Result<Settings> {
        let path = data_dir.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut settings =
            Settings::parse(&text).with_context(|| format!("invalid {}", path.display()))?;
        settings
            .check()
            .with_context(|| path.display().to_string())?;
        for name in settings.removed_keys() {
            warn_removed(name);
        }
        let tables = std::iter::once(&mut settings.agent).chain(settings.agents.values_mut());
        for table in tables {
            if let Some(dir) = &table.workdir {
                table.workdir = Some(data_dir.join(dir));
            }
            if let Some(file) = &table.heartbeat_file {
                table.heartbeat_file = Some(data_dir.join(file));
            }
        }
        Ok(settings)
    }

    /// Parse config.toml's text; [`Settings::check`] checks the values.
    pub fn parse(text: &str) -> Result<Settings> {
        let mut settings: Settings = toml::from_str(text)?;
        let table: toml::Table = toml::from_str(text)?;
        settings.legacy_agent = table
            .get("agent")
            .and_then(|t| t.as_table())
            .is_some_and(|t| {
                t.keys()
                    .any(|k| !NODE_WIDE_AGENT_KEYS.contains(&k.as_str()))
            });
        Ok(settings)
    }

    /// Check what serde cannot: runtimes, access levels, gateway URLs,
    /// capabilities, which table a key belongs to, agent names.
    pub fn check(&self) -> Result<()> {
        let tables = std::iter::once(("agent".to_string(), &self.agent)).chain(
            self.agents
                .iter()
                .map(|(name, table)| (format!("agents.{name}"), table)),
        );
        for (prefix, table) in tables {
            if let Some(runtime) = &table.runtime {
                anyhow::ensure!(
                    RUNTIMES.contains(&runtime.as_str()),
                    "unknown agent runtime {runtime:?} in {prefix}.runtime (known: {})",
                    RUNTIMES.join(", ")
                );
            }
            if let Some(access) = &table.access {
                access
                    .parse::<crate::access::Access>()
                    .with_context(|| format!("{prefix}.access"))?;
            }
            table
                .check_openclaw_url()
                .with_context(|| format!("{prefix}.openclaw_url"))?;
            for cap in &table.caps {
                crate::agent::check_cap(cap)
                    .map_err(|why| anyhow::anyhow!("{prefix}.caps: {why}"))?;
            }
            let misplaced = if prefix == "agent" {
                [
                    ("enabled", table.enabled.is_some()),
                    ("legacy_dir", table.legacy_dir.is_some()),
                ]
                .into_iter()
                .find(|(_, set)| *set)
                .map(|(key, _)| format!("agent.{key}: only in [agents.<name>] tables"))
            } else {
                [
                    ("with_mcp", table.with_mcp.is_some()),
                    ("review_ttl", table.review_ttl.is_some()),
                    ("notify", table.notify.is_some()),
                ]
                .into_iter()
                .find(|(_, set)| *set)
                .map(|(key, _)| format!("{prefix}.{key}: node-wide, set agent.{key} instead"))
            };
            if let Some(why) = misplaced {
                bail!("{why}");
            }
        }
        crate::roster::configured(self, Path::new(""), "user")?;
        self.update.policy().context("[update]")?;
        Ok(())
    }

    /// The [`REMOVED_KEYS`] set in the file.
    pub fn removed_keys(&self) -> Vec<&'static str> {
        let tables: Vec<&AgentSettings> = std::iter::once(&self.agent)
            .chain(self.agents.values())
            .collect();
        let set = [
            tables.iter().any(|t| t.thread_backoff.is_some()),
            tables.iter().any(|t| t.thread_backoff_max.is_some()),
        ];
        REMOVED_KEYS
            .iter()
            .zip(set)
            .filter_map(|(name, set)| set.then_some(*name))
            .collect()
    }

    /// The agent `wats setup` (with no tool) asks about: the first one a
    /// bare `wats` runs, with its name.
    pub fn primary_agent(&self) -> Option<(String, &AgentSettings)> {
        if let Some(runtime) = &self.agent.runtime {
            return Some((runtime.clone(), &self.agent));
        }
        self.agents
            .iter()
            .find(|(_, t)| t.runtime.is_some() && t.enabled != Some(false))
            .map(|(name, t)| (name.clone(), t))
    }

    /// The agents a bare `wats` runs, as `name` → runtime.
    pub fn enabled_agents(&self) -> Vec<(String, String)> {
        crate::roster::configured(self, Path::new(""), "user")
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.enabled)
            .filter_map(|m| Some((m.name, m.runtime?)))
            .collect()
    }
}

/// Keys that are no longer used: a file that sets them still loads (they
/// are ignored, with a warning), `wats config set` refuses them, and
/// `wats config unset` removes them.
pub const REMOVED_KEYS: [&str; 2] = ["agent.thread_backoff", "agent.thread_backoff_max"];

/// Why a removed key is ignored.
pub fn removed_key_message(name: &str) -> String {
    format!("{name} is no longer used (loop detector, ADR 0011); remove it")
}

/// Log once per process that a removed key is set.
fn warn_removed(name: &'static str) {
    static WARNED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if let Ok(mut warned) = WARNED.lock()
        && !warned.contains(&name)
    {
        warned.push(name);
        tracing::warn!("{}", removed_key_message(name));
    }
}

/// How a key's value is checked and stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Text,
    Profile,
    Infra,
    Runtime,
    /// `locked`, `workspace` or `inherit`.
    Access,
    Duration,
    /// A duration with the units s, m, h or d, or 0 for forever.
    Retention,
    Size,
    Integer,
    /// A non-negative number, e.g. dollars.
    Float,
    Bool,
    /// Comma-separated on the command line, an array of strings in the file.
    List,
}

/// One configuration key.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub name: &'static str,
    pub kind: ValueType,
    /// The value used when the key is not set, as shown to users.
    pub default: &'static str,
    pub help: &'static str,
}

const fn key(
    name: &'static str,
    kind: ValueType,
    default: &'static str,
    help: &'static str,
) -> Key {
    Key {
        name,
        kind,
        default,
        help,
    }
}

/// Every key of config.toml.
pub const KEYS: &[Key] = &[
    key(
        "profile",
        ValueType::Profile,
        "default",
        "default author profile of this node",
    ),
    key(
        "infra",
        ValueType::Infra,
        "wats+n0",
        "relays and lookup: wats+n0 (ours first, n0 as fallback), wats, n0 or none",
    ),
    key(
        "port",
        ValueType::Integer,
        "random",
        "UDP port to listen on",
    ),
    key(
        "nets",
        ValueType::List,
        "public",
        "networks to join; --net replaces the list",
    ),
    key(
        "bootstrap",
        ValueType::List,
        "none",
        "extra bootstrap nodes <id>@<host>:<port>",
    ),
    key(
        "no_default_bootstrap",
        ValueType::Bool,
        "false",
        "skip the bootstrap nodes built into the binary",
    ),
    key(
        "max_blob_size",
        ValueType::Size,
        "built in",
        "largest blob published or fetched, e.g. 1G",
    ),
    key(
        "blob_quota",
        ValueType::Size,
        "unlimited",
        "disk budget for others' blobs, e.g. 20G",
    ),
    key(
        "storage.max",
        ValueType::Size,
        "10G",
        "disk budget of the data dir: no blob fetches beyond, no events beyond +10%",
    ),
    key(
        "storage.gc",
        ValueType::Bool,
        "true",
        "delete, hourly, the blob files nothing keeps (a dropped body's, a failed fetch's); false keeps them on disk",
    ),
    key(
        "limits.hello_per_min",
        ValueType::Integer,
        "60",
        "hello requests a peer may make per minute (0: unlimited)",
    ),
    key(
        "limits.sync_per_min",
        ValueType::Integer,
        "600",
        "sync pages a peer may ask per minute (0: unlimited)",
    ),
    key(
        "limits.replicate_per_min",
        ValueType::Integer,
        "120",
        "replication requests a peer may make per minute (0: unlimited)",
    ),
    key(
        "limits.report_per_min",
        ValueType::Integer,
        "10",
        "crash reports a peer may send per minute (collectors)",
    ),
    key(
        "limits.blob_fetches_per_peer",
        ValueType::Integer,
        "4",
        "blobs one peer can make this node fetch at once",
    ),
    key(
        "limits.challenge",
        ValueType::Bool,
        "true",
        "under load, challenge peers that can solve it with proof of work instead of refusing them",
    ),
    key(
        "limits.challenge_base_bits",
        ValueType::Integer,
        "8",
        "effort of a load challenge when the load starts (leading zero bits)",
    ),
    key(
        "limits.challenge_max_bits",
        ValueType::Integer,
        "24",
        "highest effort of a load challenge, reached one bit per 10 s of overload",
    ),
    key(
        "update.check",
        ValueType::Bool,
        "true",
        "look for a newer wats release once a day; logged and shown by the MCP status tool",
    ),
    key(
        "update.auto",
        ValueType::Bool,
        "false",
        "install a newer release by itself (signature checked) once it is update.delay old; restart to use it",
    ),
    key(
        "update.delay",
        ValueType::Duration,
        "7d",
        "how old a release must be before update.auto installs it",
    ),
    key(
        "update.security_delay",
        ValueType::Duration,
        "update.delay",
        "the same for a release marked security; unset: update.delay",
    ),
    key(
        "update.url",
        ValueType::Text,
        "GitHub",
        "release API to use instead of GitHub's, e.g. a mirror (WATS_UPDATE_URL overrides it)",
    ),
    key(
        "report.crashes",
        ValueType::Bool,
        "false",
        "send crash reports without asking (wats report send otherwise)",
    ),
    key(
        "report.collector",
        ValueType::Text,
        "our router",
        "node id that collects crash reports (when no inbox key is known)",
    ),
    key(
        "report.inbox",
        ValueType::Text,
        "the maintainers' key, once compiled in",
        "inbox public key (base64url) crash reports are sealed to",
    ),
    key(
        "report.accept",
        ValueType::Bool,
        "false",
        "collect crash reports from other nodes (our router only)",
    ),
    key(
        "halt.serve",
        ValueType::Bool,
        "false",
        "on a guardian halt, keep serving halt events instead of stopping (our router only)",
    ),
    key(
        "health.no_peers_after",
        ValueType::Retention,
        "5m",
        "warn when the node has had no connected peer for this long (0: never)",
    ),
    key(
        "ui.banner",
        ValueType::Bool,
        "true",
        "the startup banner of a foreground node at a terminal (never in wats mcp, \
         piped output, services or logs)",
    ),
    key(
        "openclaw.notify_asks",
        ValueType::Bool,
        "true",
        "push answers and clarifies to the asks your OpenClaw posted to its main session \
         (/hooks/wake), once wats setup openclaw configured its hooks",
    ),
    key(
        "notify.desktop",
        ValueType::Bool,
        "auto",
        "a desktop notification (osascript, notify-send) when someone answers or clarifies \
         one of your asks; auto: on with a desktop session (macOS, or DISPLAY or \
         WAYLAND_DISPLAY set; not over SSH), off on servers and in containers",
    ),
    key(
        "ask.unanswered_after",
        ValueType::Retention,
        "30m",
        "an ask of yours with no answer after this long (or past its data.deadline) \
         shows up as unanswered in status, wait and search (0: never)",
    ),
    key(
        "submit.retention",
        ValueType::Retention,
        "30d",
        "drop sealed-inbox submissions older than this (0: keep them)",
    ),
    key(
        "agent.with_mcp",
        ValueType::Bool,
        "false",
        "node-wide: a background node started by `wats mcp` runs the enabled agents too, \
         while MCP sessions keep it up",
    ),
    key(
        "agent.review_ttl",
        ValueType::Duration,
        "24h",
        "node-wide: held posts expire after this",
    ),
    key(
        "agent.notify",
        ValueType::Bool,
        "false",
        "node-wide: a desktop notification when a post is held",
    ),
    key(
        "agent.runtime",
        ValueType::Runtime,
        "none",
        "agent run with the node: claude, codex or openclaw",
    ),
    key(
        "agent.openclaw_url",
        ValueType::Text,
        crate::openclaw::DEFAULT_URL,
        "OpenClaw gateway the runs are forwarded to (plain http: loopback only, unless agent.openclaw_insecure_ok)",
    ),
    key(
        "agent.openclaw_insecure_ok",
        ValueType::Bool,
        "false",
        "allow plain http to a private gateway (10/8, 172.16/12, 192.168/16, fc00::/7, \
         link-local, a single-label name like a Compose service)",
    ),
    key(
        "agent.openclaw_agent",
        ValueType::Text,
        crate::openclaw::DEFAULT_AGENT,
        "OpenClaw agent the runs go to (agentId)",
    ),
    key(
        "agent.access",
        ValueType::Access,
        "locked",
        "what the agent may use besides wats: locked, workspace or inherit",
    ),
    key(
        "agent.workdir",
        ValueType::Text,
        "<data dir>/agent/workspace",
        "the agent's directory with workspace access (relative to the data dir; in \
         [agents.<name>]: default <data dir>/agent/<name>/workspace)",
    ),
    key(
        "agent.tools",
        ValueType::List,
        "none",
        "built-in tools with workspace access, e.g. Read,Grep (Codex: features)",
    ),
    key(
        "agent.model",
        ValueType::Text,
        "the runtime's default",
        "model of the agent runs, e.g. sonnet (Claude) or a Codex model",
    ),
    key(
        "agent.review",
        ValueType::Bool,
        "false",
        "hold the agent's posts until `wats agent approve`",
    ),
    key(
        "agent.ignore_local",
        ValueType::Bool,
        "true",
        "do not wake on posts by other profiles of this node (false: solo tests)",
    ),
    key(
        "agent.profile",
        ValueType::Profile,
        "<OS user>-agent",
        "profile the agent posts as (in [agents.<name>]: default <OS user>-<name>)",
    ),
    key(
        "agent.about",
        ValueType::Text,
        "",
        "who the agent is and what it can do: its role, and its announce",
    ),
    key(
        "agent.caps",
        ValueType::List,
        "none",
        "capability tags of its announce, e.g. pdf,translation",
    ),
    key(
        "agent.every",
        ValueType::Duration,
        "30s",
        "at most one event-triggered run per interval",
    ),
    key(
        "agent.heartbeat",
        ValueType::Duration,
        "30m",
        "a run even with nothing new",
    ),
    key(
        "agent.max_runs_per_hour",
        ValueType::Integer,
        "12",
        "cap on runs",
    ),
    key(
        "agent.wakes_per_author_per_hour",
        ValueType::Integer,
        "0",
        "runs one author's posts can trigger per hour (0: unlimited)",
    ),
    key(
        "agent.loop_max_exchanges",
        ValueType::Integer,
        "30",
        "turns the agent and one other author may take in a row in a thread \
         before it stops waking the agent, until a third party posts (0: off)",
    ),
    key(
        "agent.catchup",
        ValueType::Retention,
        "24h",
        "at start, asks and replies stored while the runner was not listening \
         (synced at startup or on a reconnection) still wake the agent when \
         this recent and unanswered (0: off)",
    ),
    key(
        "agent.allow",
        ValueType::List,
        "everyone",
        "only these author ids wake the agent (empty: everyone)",
    ),
    key(
        "agent.heartbeat_file",
        ValueType::Text,
        "shipped",
        "heartbeat prompt, relative to the data dir",
    ),
    key(
        "agent.command",
        ValueType::Text,
        "the runtime's",
        "the agent CLI (claude, codex)",
    ),
    key(
        "agent.args",
        ValueType::List,
        "none",
        "extra arguments for the agent CLI",
    ),
    key(
        "agent.timeout",
        ValueType::Duration,
        "10m",
        "a run is killed after this",
    ),
    key(
        "agent.max_turns",
        ValueType::Integer,
        "10",
        "turns per run (passed to the agent CLI)",
    ),
    key(
        "agent.max_usd_per_day",
        ValueType::Float,
        "2",
        "spend budget per UTC day, from the cost the agent CLI reports",
    ),
    key(
        "agent.max_usd_per_month",
        ValueType::Float,
        "20",
        "spend budget per UTC month",
    ),
    key(
        "agent.max_runs_per_day",
        ValueType::Integer,
        "48",
        "runs per day that report no cost",
    ),
    key(
        "agent.keep_runs",
        ValueType::Integer,
        "200",
        "transcripts kept",
    ),
    key(
        "agent.keep_days",
        ValueType::Integer,
        "30",
        "transcripts kept at most this many days",
    ),
];

/// Keys accepted by `wats config` and in the file, but never listed nor
/// suggested: `halt.accept` (ADR 0016 §3) is a deliberate choice, made in
/// the file, not offered.
/// Settings for the operators of infrastructure nodes (our router) and
/// for tests: left out of `wats config list` unless `--all`, and listed
/// apart in docs/config.md.
pub const OPERATOR_KEYS: [&str; 2] = ["report.accept", "halt.serve"];

/// Whether `name` is one of [`OPERATOR_KEYS`].
pub fn is_operator_key(name: &str) -> bool {
    OPERATOR_KEYS.contains(&name)
}

pub const HIDDEN_KEYS: &[Key] = &[key(
    "halt.accept",
    ValueType::Bool,
    "true",
    "false: this node ignores guardian halts of the public network (ADR 0016)",
)];

/// The keys of `[agents.<name>]` beyond those of `[agent]`.
pub const AGENTS_KEYS: &[Key] = &[
    key(
        "agents.<name>.enabled",
        ValueType::Bool,
        "true",
        "false keeps the table without running it (`--agent <runtime>:<name>` still runs it)",
    ),
    key(
        "agents.<name>.legacy_dir",
        ValueType::Bool,
        "false",
        "the agent keeps <data dir>/agent (runs, budget ledger) as the single agent did \
         before ADR 0021; written when the old [agent] table is rewritten",
    ),
];

/// Whether `leaf` is a key of one agent: in `[agents.<name>]`, and in the
/// old `[agent]` table.
pub fn is_agent_leaf(leaf: &str) -> bool {
    !NODE_WIDE_AGENT_KEYS.contains(&leaf)
        && KEYS
            .iter()
            .any(|k| k.name.strip_prefix("agent.") == Some(leaf))
}

/// `agents.<name>.<leaf>` → (`<name>`, `<leaf>`).
pub fn split_agents_key(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("agents.")?.split_once('.')
}

/// The key `name`. `agents.<name>.<leaf>` is checked like `agent.<leaf>`
/// (the returned key is that one), or is one of [`AGENTS_KEYS`].
pub fn find_key(name: &str) -> Result<&'static Key> {
    if let Some((agent, leaf)) = split_agents_key(name) {
        crate::roster::check_name(agent).map_err(anyhow::Error::msg)?;
        if NODE_WIDE_AGENT_KEYS.contains(&leaf) {
            bail!("{name}: node-wide, set agent.{leaf} instead");
        }
        if let Some(key) = AGENTS_KEYS
            .iter()
            .find(|k| k.name.strip_prefix("agents.<name>.") == Some(leaf))
        {
            return Ok(key);
        }
        if is_agent_leaf(leaf) {
            return find_key(&format!("agent.{leaf}"));
        }
        bail!(
            "unknown key {name:?}; an agent's keys: {}",
            KEYS.iter()
                .filter_map(|k| k.name.strip_prefix("agent."))
                .filter(|leaf| is_agent_leaf(leaf))
                .chain(AGENT_ONLY_KEYS)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    KEYS.iter()
        .chain(HIDDEN_KEYS)
        .find(|k| k.name == name)
        .with_context(|| {
            format!(
                "unknown key {name:?}; known keys: {}",
                KEYS.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")
            )
        })
}

/// Check a value of the key `name`, as given on the command line, and turn
/// it into a TOML value.
pub fn check_value(name: &str, raw: &str) -> Result<toml_edit::Value> {
    toml_value(find_key(name)?, raw)
}

/// Check a value given on the command line and turn it into a TOML value.
fn toml_value(key: &Key, raw: &str) -> Result<toml_edit::Value> {
    let invalid = |why: String| anyhow::anyhow!("{}: {why}", key.name);
    Ok(match key.kind {
        ValueType::Text if key.name == "report.inbox" => {
            crate::inbox::InboxPublic::from_b64(raw).map_err(|e| invalid(format!("{e:#}")))?;
            raw.into()
        }
        // The lenient form here; `ConfigFile::save` and `Settings::load`
        // check it against `agent.openclaw_insecure_ok`.
        ValueType::Text if key.name == "agent.openclaw_url" => {
            crate::openclaw::check_url(raw, true).map_err(|e| invalid(format!("{e:#}")))?;
            raw.into()
        }
        ValueType::Text => raw.into(),
        ValueType::Profile => {
            if !crate::node::is_valid_profile(raw) {
                return Err(invalid("a profile is 1-64 chars of [a-z0-9_-]".into()));
            }
            raw.into()
        }
        ValueType::Infra => {
            raw.parse::<crate::infra::Infra>()
                .map_err(|e| invalid(e.to_string()))?;
            raw.into()
        }
        ValueType::Runtime => {
            if !RUNTIMES.contains(&raw) {
                return Err(invalid(format!(
                    "known runtimes: {} (`wats config unset agent.runtime` to stop running one)",
                    RUNTIMES.join(", ")
                )));
            }
            raw.into()
        }
        ValueType::Access => {
            raw.parse::<crate::access::Access>()
                .map_err(|e| invalid(e.to_string()))?;
            raw.into()
        }
        ValueType::Duration => {
            parse_duration(raw).map_err(invalid)?;
            raw.into()
        }
        ValueType::Retention => {
            parse_retention(raw).map_err(invalid)?;
            raw.into()
        }
        ValueType::Size => {
            parse_size(raw).map_err(invalid)?;
            raw.into()
        }
        ValueType::Integer => {
            let n: i64 = raw
                .parse()
                .map_err(|_| invalid(format!("{raw:?} is not an integer")))?;
            if n < 0 || (key.name == "port" && n > i64::from(u16::MAX)) {
                return Err(invalid(format!("{n} is out of range")));
            }
            n.into()
        }
        ValueType::Float => {
            let n: f64 = raw
                .parse()
                .map_err(|_| invalid(format!("{raw:?} is not a number")))?;
            if !n.is_finite() || n < 0.0 {
                return Err(invalid(format!("{n} is out of range")));
            }
            n.into()
        }
        ValueType::Bool => match raw {
            "true" | "1" | "yes" | "on" => true.into(),
            "false" | "0" | "no" | "off" => false.into(),
            _ => return Err(invalid(format!("{raw:?} is not true or false"))),
        },
        ValueType::List => {
            let items: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if key.name == "nets" {
                for net in &items {
                    if !crate::event::is_valid_net(net) {
                        return Err(invalid(format!(
                            "{net:?} is not a network id (1-64 chars of [a-z0-9._:-])"
                        )));
                    }
                }
            }
            if key.name == "agent.caps" {
                for cap in &items {
                    crate::agent::check_cap(cap).map_err(invalid)?;
                }
            }
            items.into_iter().collect::<toml_edit::Array>().into()
        }
    })
}

/// Where a dotted key lives in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Top(String),
    Table(String, String),
    /// `[agents.<name>]`, key.
    Agent(String, String),
}

/// The editable file, keeping its comments and layout.
///
/// `agent.<key>` (a key of one agent) means the only agent: the old
/// `[agent]` table while the file has one or has no `[agents.<name>]`
/// table, else its only `[agents.<name>]` table; with several, the name
/// must be given (`agents.<name>.<key>`). Writing a key of an agent
/// rewrites the old `[agent]` table as `[agents.<runtime>]` first (ADR
/// 0021), with `profile` written out and `legacy_dir = true`, so that the
/// agent keeps its key and its directory.
pub struct ConfigFile {
    path: PathBuf,
    doc: toml_edit::DocumentMut,
    /// The agent the old `[agent]` table became, once rewritten.
    migrated: Option<String>,
}

/// Files of the single agent's directory that show it has run.
const OLD_AGENT_STATE: [&str; 3] = ["cursor", "runs.jsonl", "spend.jsonl"];

impl ConfigFile {
    pub fn open(data_dir: &Path) -> Result<ConfigFile> {
        let path = data_dir.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let doc = text
            .parse()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(ConfigFile {
            path,
            doc,
            migrated: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The names of the `[agents.<name>]` tables.
    pub fn agent_names(&self) -> Vec<String> {
        self.doc
            .get("agents")
            .and_then(|item| item.as_table_like())
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default()
    }

    /// Whether `[agent]` holds keys of an agent (the old form), not only
    /// node-wide ones.
    fn legacy_keys(&self) -> bool {
        self.doc
            .get("agent")
            .and_then(|item| item.as_table_like())
            .is_some_and(|t| t.iter().any(|(k, _)| !NODE_WIDE_AGENT_KEYS.contains(&k)))
    }

    /// The agent `wats config` reads or writes as `agent.<key>`, when the
    /// file is in the new form: its only `[agents.<name>]` table.
    fn only_agent(&self, leaf: &str) -> Result<Option<String>> {
        let names = self.agent_names();
        if names.is_empty() || self.legacy_keys() {
            return Ok(None);
        }
        match names.as_slice() {
            [only] => Ok(Some(only.clone())),
            _ => bail!(
                "several agents ({}): name one, e.g. agents.{}.{leaf}",
                names.join(", "),
                names[0]
            ),
        }
    }

    fn place(&self, name: &str) -> Result<Place> {
        if let Some((agent, leaf)) = split_agents_key(name) {
            return Ok(Place::Agent(agent.into(), leaf.into()));
        }
        Ok(match name.split_once('.') {
            None => Place::Top(name.into()),
            Some(("agent", leaf)) if is_agent_leaf(leaf) => match self.only_agent(leaf)? {
                Some(agent) => Place::Agent(agent, leaf.into()),
                None => Place::Table("agent".into(), leaf.into()),
            },
            Some((table, leaf)) => Place::Table(table.into(), leaf.into()),
        })
    }

    /// Whether `name` is a key of one agent.
    fn is_agent_key(name: &str) -> bool {
        split_agents_key(name).is_some() || name.strip_prefix("agent.").is_some_and(is_agent_leaf)
    }

    /// Rewrite the old `[agent]` table as `[agents.<name>]`, `<name>` its
    /// runtime, else `runtime` (a runtime being set). Its keys move with
    /// their comments, removed keys are dropped, node-wide keys stay in
    /// `[agent]`; `profile` is written out (the old default
    /// `<OS user>-agent`) and `legacy_dir = true` keeps the directory.
    fn migrate(&mut self, runtime: Option<&str>) -> Result<()> {
        if !self.legacy_keys() {
            return Ok(());
        }
        let own = self
            .doc
            .get("agent")
            .and_then(|t| t.get("runtime"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let Some(name) = own.clone().or(runtime.map(str::to_string)) else {
            return Ok(());
        };
        crate::roster::check_name(&name).map_err(anyhow::Error::msg)?;
        if self.agent_names().contains(&name) {
            bail!(
                "{}: [agent] runtime = {name:?} and [agents.{name}] are the same agent: merge \
                 them by hand",
                self.path.display()
            );
        }
        let Some(old) = self
            .doc
            .get_mut("agent")
            .and_then(|item| item.as_table_like_mut())
        else {
            return Ok(());
        };
        let mut table = toml_edit::Table::new();
        let keys: Vec<String> = old
            .iter()
            .map(|(k, _)| k.to_string())
            .filter(|k| !NODE_WIDE_AGENT_KEYS.contains(&k.as_str()))
            .collect();
        for k in keys {
            let key = old.key(&k).cloned();
            let Some(item) = old.remove(&k) else { continue };
            if REMOVED_KEYS.contains(&format!("agent.{k}").as_str()) {
                continue;
            }
            match key {
                Some(key) => table.insert_formatted(&key, item),
                None => table.insert(&k, item),
            };
        }
        if own.is_none() {
            table.insert("runtime", toml_edit::value(name.clone()));
        }
        if !table.contains_key("profile") {
            let user = crate::roster::os_user_profile();
            table.insert(
                "profile",
                toml_edit::value(crate::roster::legacy_profile(&user)),
            );
        }
        table.insert("legacy_dir", toml_edit::value(true));
        let empty = old.is_empty();
        if let Some(Some(old)) = self.doc.get_mut("agent").map(|item| item.as_table_mut()) {
            table.set_position(old.position());
            *table.decor_mut() = std::mem::take(old.decor_mut());
        }
        if empty {
            self.doc.remove("agent");
        }
        self.agents_mut()?
            .insert(&name, toml_edit::Item::Table(table));
        self.migrated = Some(name);
        Ok(())
    }

    /// The `[agents]` table, created (implicit) if needed.
    fn agents_mut(&mut self) -> Result<&mut dyn toml_edit::TableLike> {
        let item = self.doc.entry("agents").or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        });
        item.as_table_like_mut()
            .with_context(|| format!("{}: agents is not a table", self.path.display()))
    }

    /// Whether the single agent of before ADR 0021 left state in
    /// `<data dir>/agent` (it ran through `wats --agent`).
    fn old_agent_ran(&self) -> bool {
        self.path.parent().is_some_and(|dir| {
            OLD_AGENT_STATE
                .iter()
                .any(|f| dir.join("agent").join(f).exists())
        })
    }

    /// Where `name` is written: the old `[agent]` table is rewritten first.
    fn place_for_write(&mut self, name: &str, raw: Option<&str>) -> Result<Place> {
        if !Self::is_agent_key(name) {
            return self.place(name);
        }
        let runtime = raw.filter(|_| name == "agent.runtime");
        self.migrate(runtime)?;
        if let Some(runtime) = runtime
            && self.agent_names().is_empty()
            && !self.legacy_keys()
        {
            // The first agent, named after its runtime.
            return Ok(Place::Agent(runtime.into(), "runtime".into()));
        }
        self.place(name)
    }

    fn item(&self, place: &Place) -> Option<&toml_edit::Item> {
        match place {
            Place::Top(leaf) => self.doc.get(leaf),
            Place::Table(table, leaf) => self.doc.get(table)?.get(leaf),
            Place::Agent(agent, leaf) => self.doc.get("agents")?.get(agent)?.get(leaf),
        }
    }

    /// The value set in the file, as TOML text; an error when `agent.<key>`
    /// does not say which of several agents.
    pub fn lookup(&self, name: &str) -> Result<Option<String>> {
        let place = self.place(name)?;
        Ok(self
            .item(&place)
            .and_then(|item| item.as_value())
            .map(|v| v.to_string().trim().to_string()))
    }

    /// The value set in the file, as TOML text.
    pub fn get(&self, name: &str) -> Option<String> {
        self.lookup(name).ok().flatten()
    }

    pub fn set(&mut self, name: &str, raw: &str) -> Result<()> {
        let removed =
            split_agents_key(name).map_or(name.to_string(), |(_, l)| format!("agent.{l}"));
        if REMOVED_KEYS.contains(&removed.as_str()) {
            bail!("{}", removed_key_message(&removed));
        }
        let key = find_key(name)?;
        let value = toml_value(key, raw)?;
        let first = self.agent_names().is_empty() && !self.legacy_keys();
        match self.place_for_write(name, Some(raw))? {
            Place::Top(leaf) => {
                self.doc.insert(&leaf, toml_edit::value(value));
            }
            Place::Table(t, leaf) => {
                let entry = self
                    .doc
                    .entry(&t)
                    .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
                let Some(table) = entry.as_table_like_mut() else {
                    bail!("{}: [{t}] is not a table", self.path.display());
                };
                table.insert(&leaf, toml_edit::value(value));
            }
            Place::Agent(agent, leaf) => {
                crate::roster::check_name(&agent).map_err(anyhow::Error::msg)?;
                // The first agent written after the single agent ran
                // through `wats --agent` takes over its key and directory.
                let adopt = first && self.old_agent_ran();
                let path = self.path.clone();
                let agents = self.agents_mut()?;
                if !agents.contains_key(&agent) {
                    agents.insert(&agent, toml_edit::Item::Table(toml_edit::Table::new()));
                }
                let Some(table) = agents.get_mut(&agent).and_then(|t| t.as_table_like_mut()) else {
                    bail!("{}: [agents.{agent}] is not a table", path.display());
                };
                table.insert(&leaf, toml_edit::value(value));
                if adopt {
                    if !table.contains_key("profile") {
                        let user = crate::roster::os_user_profile();
                        table.insert(
                            "profile",
                            toml_edit::value(crate::roster::legacy_profile(&user)),
                        );
                    }
                    table.insert("legacy_dir", toml_edit::value(true));
                }
            }
        }
        Ok(())
    }

    /// Remove a key; returns whether it was set.
    pub fn unset(&mut self, name: &str) -> Result<bool> {
        if !REMOVED_KEYS.contains(&name) {
            find_key(name)?;
        }
        let was_migrated = self.migrated.is_some();
        let place = self.place_for_write(name, None)?;
        let migrated = !was_migrated && self.migrated.is_some();
        let removed = match place {
            Place::Top(leaf) => self.doc.remove(&leaf).is_some(),
            Place::Table(t, leaf) => self
                .doc
                .get_mut(&t)
                .and_then(|item| item.as_table_like_mut())
                .and_then(|table| table.remove(&leaf))
                .is_some(),
            Place::Agent(agent, leaf) => self
                .doc
                .get_mut("agents")
                .and_then(|item| item.get_mut(&agent))
                .and_then(|item| item.as_table_like_mut())
                .and_then(|table| table.remove(&leaf))
                .is_some(),
        };
        // A rewritten [agent] table must be saved too.
        Ok(removed || migrated)
    }

    /// The settings as the file would load once saved (paths not resolved
    /// against the data directory).
    pub fn preview(&self) -> Result<Settings> {
        let settings = Settings::parse(&self.doc.to_string())
            .with_context(|| format!("invalid {}", self.path.display()))?;
        settings
            .check()
            .with_context(|| self.path.display().to_string())?;
        Ok(settings)
    }

    /// Write the file, after checking that it still loads.
    pub fn save(&self) -> Result<()> {
        let text = self.doc.to_string();
        let settings = Settings::parse(&text)
            .with_context(|| format!("refusing to write an invalid {}", self.path.display()))?;
        settings
            .check()
            .with_context(|| format!("refusing to write {}", self.path.display()))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        // The safety settings follow the agent to its new table.
        if let (Some(name), Some(dir)) = (&self.migrated, self.path.parent())
            && let Err(e) = crate::safety::migrate_baseline(dir, name)
        {
            tracing::warn!("safety settings after rewriting [agent]: {e:#}");
        }
        Ok(())
    }
}

/// Parse a retention: `0` (keep forever, `None`) or a duration as
/// [`parse_duration`] (e.g. 30d, 12h).
pub fn parse_retention(s: &str) -> Result<Option<Duration>, String> {
    let s = s.trim();
    let digits = s.trim_end_matches(|c: char| !c.is_ascii_digit());
    if !digits.is_empty() && digits.bytes().all(|b| b == b'0') {
        return Ok(None);
    }
    parse_duration(s).map(Some)
}

/// Parse a duration: a number with an optional unit s, m, h or d (e.g.
/// 30s, 5m, 2h, 7d; a bare number is seconds).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration {s:?}: expected e.g. 30s, 5m, 2h or 7d"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("invalid duration unit in {s:?}: use s, m, h or d")),
    };
    if secs == 0 {
        return Err("duration must be positive".into());
    }
    Ok(Duration::from_secs(secs))
}

/// Parse a byte size: a number with an optional binary suffix K, M, G or T
/// (also KiB, MB...), e.g. `512M`, `20G`, `1048576`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid size {s:?}: expected e.g. 512M or 20G"))?;
    let shift = match unit
        .trim()
        .to_ascii_uppercase()
        .trim_end_matches("IB")
        .trim_end_matches('B')
    {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(format!("invalid size unit in {s:?}: use K, M, G or T")),
    };
    number
        .checked_mul(1u64 << shift)
        .ok_or_else(|| format!("size {s:?} is too large"))
}

/// `docs/config.md`: every key of [`KEYS`] with its default, grouped by
/// section ([`HIDDEN_KEYS`] stay out). Generated, and checked by a test.
pub fn reference_markdown() -> String {
    let mut out = String::from(
        "# Configuration reference\n\n\
         <!-- Generated from crates/wats/src/settings.rs (KEYS); \
         regenerate with `WATS_UPDATE_DOCS=1 cargo test -p wats --lib settings`. -->\n\n\
         Settings live in `config.toml` in the data directory (`~/.wats` by default).\n\
         Edit them with `wats config set <key> <value>`, `wats config get <key>`, \
         `wats config unset <key>` or `wats config edit`.\n\
         Command-line flags and `WATS_*` variables, where they exist (`wats --help`), \
         take precedence over the file.\n",
    );
    // Markdown table cells: no `|`, and `<…>` must not read as HTML.
    let cell = |s: &str| {
        s.replace('|', "\\|")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let row = |key: &Key| {
        format!(
            "| `{}` | {} | {} |\n",
            key.name,
            cell(key.default),
            cell(key.help)
        )
    };
    let mut section = None;
    for key in KEYS.iter().filter(|k| !is_operator_key(k.name)) {
        let this = key.name.split_once('.').map_or("node", |(s, _)| s);
        if section != Some(this) {
            section = Some(this);
            let title = match this {
                "node" => "Node (top level)".to_string(),
                s => format!("`[{s}]`"),
            };
            out.push_str(&format!("\n## {title}\n\n"));
            if this == "agent" {
                out.push_str(&format!(
                    "The node-wide keys ({}) stay in `[agent]`. The others are one agent's: \
                     in the old single `[agent]` table, which still loads as the agent named \
                     after its runtime, or in `[agents.<name>]` (below). With one agent, \
                     `wats config set agent.<key>` sets it; with several, name it: \
                     `agents.<name>.<key>`.\n\n",
                    NODE_WIDE_AGENT_KEYS
                        .map(|k| format!("`agent.{k}`"))
                        .join(", ")
                ));
            }
            out.push_str("| Key | Default | |\n|---|---|---|\n");
        }
        out.push_str(&row(key));
    }
    out.push_str(
        "\n## `[agents.<name>]`\n\n\
         One table per agent (ADR 0021); `<name>` is 1-32 chars of `[a-z0-9_-]`, by default \
         the runtime. Each takes every key of `[agent]` but the node-wide ones, as \
         `agents.<name>.<key>` (e.g. `agents.reviewer.about`), with two differences: \
         `profile` defaults to `<OS user>-<name>`, and the agent keeps its runs, budget \
         ledger and cursor in `<data dir>/agent/<name>/` (its workspace in \
         `<data dir>/agent/<name>/workspace`). A table with no `runtime` only runs through \
         `wats --agent <runtime>:<name>`. Two more keys:\n\n\
         | Key | Default | |\n|---|---|---|\n",
    );
    for key in AGENTS_KEYS {
        out.push_str(&row(key));
    }
    out.push_str(
        "\n## Operator and test settings\n\n\
         For the operators of infrastructure nodes (the public bootstrap node), and \
         for tests. `wats config list` shows them only with `--all`. Their flags \
         (`--report-accept`, `--halt-serve`), like the test flags `--hermetic` and \
         `--loopback`, work but are hidden from `wats --help`.\n\n\
         | Key | Default | |\n|---|---|---|\n",
    );
    for key in KEYS.iter().filter(|k| is_operator_key(k.name)) {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            key.name, key.default, key.help
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reference_page_is_up_to_date() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/config.md");
        let generated = super::reference_markdown();
        if std::env::var_os("WATS_UPDATE_DOCS").is_some() {
            std::fs::write(&path, &generated).unwrap();
        }
        let published = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            published, generated,
            "docs/config.md drifted: WATS_UPDATE_DOCS=1 cargo test -p wats --lib settings"
        );
    }

    use super::*;

    #[test]
    fn loads_and_rejects() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Settings::load(dir.path()).unwrap().agent.runtime.is_none());
        std::fs::write(
            dir.path().join(FILE_NAME),
            "profile = \"alice\"\n[agent]\nruntime = \"claude\"\nevery = \"1m\"\nheartbeat_file = \"hb.md\"\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.profile.as_deref(), Some("alice"));
        assert_eq!(settings.agent.runtime.as_deref(), Some("claude"));
        assert_eq!(
            settings.agent.heartbeat_file,
            Some(dir.path().join("hb.md"))
        );
        std::fs::write(dir.path().join(FILE_NAME), "[agent]\nevry = \"1m\"\n").unwrap();
        assert!(Settings::load(dir.path()).is_err(), "typos are errors");
        std::fs::write(dir.path().join(FILE_NAME), "[agent]\nruntime = \"gpt\"\n").unwrap();
        assert!(Settings::load(dir.path()).is_err());
    }

    #[test]
    fn nets_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "nets = [\"public\", \"acme\"]\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.nets, ["public", "acme"]);
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("nets", "public, acme.dev").unwrap();
        assert!(file.set("nets", "public,Acme").is_err(), "not a network id");
    }

    /// Plain http to a Compose service needs `agent.openclaw_insecure_ok`,
    /// in the file and through `wats config set` / `unset`.
    #[test]
    fn openclaw_insecure_ok_allows_private_http_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "[agent]\nopenclaw_url = \"http://openclaw:18789\"\n").unwrap();
        assert!(Settings::load(dir.path()).is_err(), "no flag");
        std::fs::write(
            &path,
            "[agent]\nopenclaw_url = \"http://openclaw:18789\"\nopenclaw_insecure_ok = true\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.agent.openclaw_insecure_ok, Some(true));
        std::fs::write(
            &path,
            "[agent]\nopenclaw_url = \"http://gw.example.com\"\nopenclaw_insecure_ok = true\n",
        )
        .unwrap();
        assert!(Settings::load(dir.path()).is_err(), "dotted name");

        std::fs::remove_file(&path).unwrap();
        let mut file = ConfigFile::open(dir.path()).unwrap();
        assert!(
            file.set("agent.openclaw_url", "http://8.8.8.8:18789")
                .is_err()
        );
        file.set("agent.openclaw_url", "http://openclaw:18789")
            .unwrap();
        assert!(file.save().is_err(), "saved without the flag");
        file.set("agent.openclaw_insecure_ok", "true").unwrap();
        file.save().unwrap();
        assert!(file.unset("agent.openclaw_insecure_ok").unwrap());
        assert!(
            file.save().is_err(),
            "flag removed under a private http URL"
        );
        file.set("agent.openclaw_url", "http://127.0.0.1:18789")
            .unwrap();
        file.save().unwrap();
    }

    /// The per-thread backoff keys were removed (ADR 0011, loop detector):
    /// a file that still sets them loads, and they can be unset.
    #[test]
    fn removed_keys_still_load_and_can_be_unset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agent]\nruntime = \"claude\"\nthread_backoff = \"1m\"\nthread_backoff_max = \"1h\"\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.agent.runtime.as_deref(), Some("claude"));
        assert_eq!(settings.removed_keys(), REMOVED_KEYS);
        assert_eq!(settings.agent.loop_max_exchanges, None);
        let mut file = ConfigFile::open(dir.path()).unwrap();
        for removed in REMOVED_KEYS {
            assert!(file.unset(removed).unwrap());
        }
        file.save().unwrap();
        assert!(
            Settings::load(dir.path())
                .unwrap()
                .removed_keys()
                .is_empty()
        );
        assert!(Settings::default().removed_keys().is_empty());
    }

    #[test]
    fn update_policy() {
        let p = UpdateSettings::default().policy().unwrap();
        assert!(p.check && !p.auto);
        assert_eq!(p.delay, Duration::from_secs(7 * 86400));
        assert_eq!(p.security_delay, None);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[update]\nauto = true\ndelay = \"3d\"\nsecurity_delay = \"12h\"\n",
        )
        .unwrap();
        let p = Settings::load(dir.path()).unwrap().update.policy().unwrap();
        assert!(p.check && p.auto);
        assert_eq!(p.delay, Duration::from_secs(3 * 86400));
        assert_eq!(p.security_delay, Some(Duration::from_secs(12 * 3600)));
        std::fs::write(dir.path().join(FILE_NAME), "[update]\ndelay = \"soon\"\n").unwrap();
        assert!(Settings::load(dir.path()).is_err());
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("update.security_delay", "1d").unwrap();
        assert!(file.set("update.delay", "1w").is_err());
    }

    #[test]
    fn edits_keep_comments_and_check_values() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "# mine\ninfra = \"n0\" # keep\n",
        )
        .unwrap();
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agent.runtime", "claude").unwrap();
        file.set("agent.allow", "aa, bb").unwrap();
        file.set("agent.max_runs_per_hour", "6").unwrap();
        file.set("agent.wakes_per_author_per_hour", "0").unwrap();
        file.set("agent.loop_max_exchanges", "0").unwrap();
        assert!(file.set("agent.loop_max_exchanges", "many").is_err());
        for removed in REMOVED_KEYS {
            let err = file.set(removed, "1m").unwrap_err().to_string();
            assert_eq!(err, removed_key_message(removed));
        }
        file.set("no_default_bootstrap", "yes").unwrap();
        assert!(file.set("agent.every", "soon").is_err());
        assert!(file.set("agent.runtime", "gpt").is_err());
        assert!(file.set("profile", "Bad Name").is_err());
        assert!(file.set("agent.colour", "x").is_err());
        assert!(file.set("port", "70000").is_err());
        // Capabilities become announce tags: checked when set.
        file.set("agent.caps", "pdf, Code-Review, lang:fran\u{e7}ais")
            .unwrap();
        for bad in [
            "rust lang",
            "pdf,c++",
            "lang:franc\u{327}ais",
            "p\u{430}ypal",
            "a::b",
        ] {
            let err = file.set("agent.caps", bad).unwrap_err().to_string();
            assert!(err.contains("not a valid capability"), "{bad}: {err}");
        }
        file.save().unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("# mine") && text.contains("# keep"), "{text}");
        let settings = Settings::load(dir.path()).unwrap();
        // `agent.runtime` made [agents.claude]; `agent.<key>` is its.
        let claude = &settings.agents["claude"];
        assert_eq!(claude.allow, ["aa", "bb"]);
        assert_eq!(claude.max_runs_per_hour, Some(6));
        assert_eq!(claude.wakes_per_author_per_hour, Some(0));
        assert_eq!(claude.loop_max_exchanges, Some(0));
        assert_eq!(settings.no_default_bootstrap, Some(true));
        let mut file = ConfigFile::open(dir.path()).unwrap();
        assert_eq!(file.get("agent.runtime").as_deref(), Some("\"claude\""));
        assert!(file.unset("agent.runtime").unwrap());
        assert!(!file.unset("agent.runtime").unwrap());
        file.save().unwrap();
        assert!(Settings::load(dir.path()).unwrap().agent.runtime.is_none());
    }

    /// ADR 0021: writing an agent key into a file with the old `[agent]`
    /// table rewrites it as `[agents.<runtime>]`, in place, with its
    /// comments, the old profile written out and `legacy_dir`; node-wide
    /// keys stay in `[agent]`.
    #[test]
    fn old_agent_table_is_rewritten_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            "# mine\ninfra = \"n0\"\n\n# my agent\n[agent]\nruntime = \"claude\" # the CLI\n\
             # its role\nabout = \"I review.\"\nthread_backoff = \"1m\"\nreview_ttl = \"2h\"\n\n\
             [ui]\nbanner = false\n",
        )
        .unwrap();
        // Loads as before: the agent `claude`, old profile and directory.
        let before = Settings::load(dir.path()).unwrap();
        let old = crate::roster::select(&before, dir.path(), "vincent", &[]).unwrap();
        assert_eq!(
            (old[0].name.as_str(), old[0].profile.as_str()),
            ("claude", "vincent-agent")
        );
        assert_eq!(old[0].work_dir, dir.path().join("agent"));

        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agent.heartbeat", "2h").unwrap();
        file.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let user = crate::roster::os_user_profile();
        assert!(
            text.contains("# mine") && text.contains("# my agent"),
            "{text}"
        );
        assert!(text.contains("[agents.claude]"), "{text}");
        assert!(text.contains("runtime = \"claude\" # the CLI"), "{text}");
        assert!(text.contains("# its role\nabout = \"I review.\""), "{text}");
        assert!(
            text.contains(&format!("profile = \"{user}-agent\"")),
            "{text}"
        );
        assert!(text.contains("legacy_dir = true"), "{text}");
        assert!(!text.contains("thread_backoff"), "removed keys go: {text}");
        assert!(text.contains("[agent]\nreview_ttl = \"2h\""), "{text}");
        // Its place in the file: before [ui].
        assert!(
            text.find("[agents.claude]").unwrap() < text.find("[ui]").unwrap(),
            "{text}"
        );
        let after = Settings::load(dir.path()).unwrap();
        assert!(!after.legacy_agent);
        let new = crate::roster::select(&after, dir.path(), "someone-else", &[]).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].name, "claude");
        assert_eq!(new[0].profile, format!("{user}-agent"), "same key");
        assert_eq!(new[0].work_dir, dir.path().join("agent"), "same runs");
        assert_eq!(new[0].settings.heartbeat.as_deref(), Some("2h"));
        assert_eq!(after.agent.review_ttl.as_deref(), Some("2h"));
    }

    /// `agents.<name>.<key>` in `wats config`, and `agent.<key>` for the
    /// only agent.
    #[test]
    fn dotted_agent_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agents.reviewer.runtime", "claude").unwrap();
        file.set("agents.reviewer.about", "I review.").unwrap();
        assert_eq!(file.get("agent.about").as_deref(), Some("\"I review.\""));
        file.set("agent.max_turns", "3").unwrap();
        file.set("agent.with_mcp", "true").unwrap();
        file.set("agents.helper.runtime", "codex").unwrap();
        file.set("agents.helper.enabled", "false").unwrap();
        assert!(file.lookup("agent.about").is_err(), "which agent?");
        assert!(file.set("agent.about", "x").is_err());
        assert!(file.set("agents.Bad.about", "x").is_err());
        assert!(file.set("agents.reviewer.with_mcp", "true").is_err());
        assert!(file.set("agents.reviewer.colour", "red").is_err());
        assert!(file.set("agent.enabled", "true").is_err());
        assert!(file.set("agents.reviewer.caps", "c++").is_err());
        file.save().unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.agents["reviewer"].max_turns, Some(3));
        assert_eq!(settings.agent.with_mcp, Some(true));
        assert_eq!(
            settings.enabled_agents(),
            [("reviewer".to_string(), "claude".to_string())]
        );
        let mut file = ConfigFile::open(dir.path()).unwrap();
        assert!(file.unset("agents.helper.enabled").unwrap());
        file.save().unwrap();
        assert_eq!(
            Settings::load(dir.path()).unwrap().enabled_agents().len(),
            2
        );
        // Misplaced keys in the file are errors.
        let path = dir.path().join(FILE_NAME);
        for bad in [
            "[agent]\nenabled = false\n",
            "[agents.a]\nnotify = true\n",
            "[agents.runs]\nruntime = \"claude\"\n",
            "[agents.a]\nruntime = \"gpt\"\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(Settings::load(dir.path()).is_err(), "{bad}");
        }
    }

    /// The first agent written after the single agent ran through `wats
    /// --agent` takes over its profile and directory.
    #[test]
    fn first_agent_adopts_the_old_agent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let user = crate::roster::os_user_profile();
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agents.claude.runtime", "claude").unwrap();
        file.save().unwrap();
        let fresh = Settings::load(dir.path()).unwrap();
        assert_eq!(
            fresh.agents["claude"].profile, None,
            "a new node: <user>-claude"
        );
        std::fs::remove_file(dir.path().join(FILE_NAME)).unwrap();
        std::fs::create_dir_all(dir.path().join("agent")).unwrap();
        std::fs::write(dir.path().join("agent").join("cursor"), "12").unwrap();
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agents.claude.runtime", "claude").unwrap();
        file.set("agents.reviewer.runtime", "claude").unwrap();
        file.save().unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(
            settings.agents["claude"].profile,
            Some(format!("{user}-agent"))
        );
        assert_eq!(settings.agents["claude"].legacy_dir, Some(true));
        assert_eq!(settings.agents["reviewer"].legacy_dir, None);
    }

    /// The rewrite carries the acknowledged safety values over: no change
    /// to acknowledge, and one not yet acknowledged still shows. A new
    /// agent shows.
    #[test]
    fn rewrite_keeps_the_safety_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            "[agent]\nruntime = \"claude\"\naccess = \"locked\"\n",
        )
        .unwrap();
        assert_eq!(crate::safety::check(dir.path()).unwrap(), None);
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agent.about", "hi").unwrap();
        file.save().unwrap();
        assert_eq!(crate::safety::check(dir.path()).unwrap(), None);
        let mut file = ConfigFile::open(dir.path()).unwrap();
        file.set("agents.claude.access", "inherit").unwrap();
        file.set("agents.second.runtime", "codex").unwrap();
        file.save().unwrap();
        let notice = crate::safety::check(dir.path()).unwrap().unwrap();
        let keys: Vec<&str> = notice.changes.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"agents.claude.access"), "{keys:?}");
        assert!(keys.contains(&"agents.second.runtime"), "{keys:?}");
        assert!(crate::safety::is_key("agents.second.access"));
        assert!(!crate::safety::is_key("agents.second.about"));
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_retention("0"), Ok(None));
        assert_eq!(parse_retention("0d"), Ok(None));
        assert_eq!(
            parse_retention("30d"),
            Ok(Some(Duration::from_secs(30 * 24 * 3600)))
        );
        assert_eq!(parse_retention("2h"), Ok(Some(Duration::from_secs(7200))));
        assert!(parse_retention("xd").is_err());
        assert!(parse_retention("-1").is_err());
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(7 * 86400)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("7"), Ok(Duration::from_secs(7)));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("1w").is_err());
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("20GiB"), Ok(20 << 30));
        assert_eq!(parse_size("1 kb"), Ok(1024));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5X").is_err());
    }
}
