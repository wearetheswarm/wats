//! `<data dir>/config.toml`: what a bare `wats` does, so that the daemon
//! runs as the user configured it. Optional. Precedence: flags, then
//! `WATS_*` environment variables, then this file, then defaults. Flags
//! never write it; `wats config set` and `wats setup` do.
//!
//! ```toml
//! profile = "alice"            # default profile of `wats mcp` sessions
//! infra = "wats"
//!
//! [agent]
//! runtime = "claude"           # run the Claude agent with the node
//! profile = "alice-agent"      # default: <OS user>-agent
//! about = "I review Rust code and explain compiler errors."
//! caps = ["rust", "code-review"]
//! every = "30s"
//! heartbeat = "30m"
//! ```
//!
//! [`KEYS`] lists every key; unknown keys are errors.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const FILE_NAME: &str = "config.toml";

/// The agent runtimes wats can run.
pub const RUNTIMES: [&str; 2] = ["claude", "codex"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub profile: Option<String>,
    pub infra: Option<String>,
    pub port: Option<u16>,
    #[serde(default)]
    pub bootstrap: Vec<String>,
    pub no_default_bootstrap: Option<bool>,
    pub max_blob_size: Option<String>,
    pub blob_quota: Option<String>,
    #[serde(default)]
    pub storage: StorageSettings,
    #[serde(default)]
    pub agent: AgentSettings,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSettings {
    pub max: Option<String>,
}

/// Default disk budget of the data directory (`storage.max`).
pub const DEFAULT_STORAGE_MAX: &str = "10G";

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    pub runtime: Option<String>,
    pub profile: Option<String>,
    pub about: Option<String>,
    #[serde(default)]
    pub caps: Vec<String>,
    pub every: Option<String>,
    pub heartbeat: Option<String>,
    pub max_runs_per_hour: Option<usize>,
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
        let mut settings: Settings =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        if let Some(runtime) = &settings.agent.runtime {
            anyhow::ensure!(
                RUNTIMES.contains(&runtime.as_str()),
                "{}: unknown agent runtime {runtime:?} (known: {})",
                path.display(),
                RUNTIMES.join(", ")
            );
        }
        if let Some(file) = &settings.agent.heartbeat_file {
            settings.agent.heartbeat_file = Some(data_dir.join(file));
        }
        Ok(settings)
    }
}

/// How a key's value is checked and stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Text,
    Profile,
    Infra,
    Runtime,
    Duration,
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
        "wats",
        "relays and lookup: wats, n0, wats+n0 or none",
    ),
    key(
        "port",
        ValueType::Integer,
        "random",
        "UDP port to listen on",
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
        "agent.runtime",
        ValueType::Runtime,
        "none",
        "agent run with the node: claude or codex",
    ),
    key(
        "agent.profile",
        ValueType::Profile,
        "<OS user>-agent",
        "profile the agent posts as",
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

pub fn find_key(name: &str) -> Result<&'static Key> {
    KEYS.iter().find(|k| k.name == name).with_context(|| {
        format!(
            "unknown key {name:?}; known keys: {}",
            KEYS.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")
        )
    })
}

/// Check a value given on the command line and turn it into a TOML value.
fn toml_value(key: &Key, raw: &str) -> Result<toml_edit::Value> {
    let invalid = |why: String| anyhow::anyhow!("{}: {why}", key.name);
    Ok(match key.kind {
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
        ValueType::Duration => {
            parse_duration(raw).map_err(invalid)?;
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
        ValueType::List => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<toml_edit::Array>()
            .into(),
    })
}

/// The editable file, keeping its comments and layout.
pub struct ConfigFile {
    path: PathBuf,
    doc: toml_edit::DocumentMut,
}

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
        Ok(ConfigFile { path, doc })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn split(name: &str) -> (Option<&str>, &str) {
        match name.split_once('.') {
            Some((table, leaf)) => (Some(table), leaf),
            None => (None, name),
        }
    }

    /// The value set in the file, as TOML text.
    pub fn get(&self, name: &str) -> Option<String> {
        let (table, leaf) = Self::split(name);
        let item = match table {
            Some(t) => self.doc.get(t)?.get(leaf)?,
            None => self.doc.get(leaf)?,
        };
        item.as_value().map(|v| v.to_string().trim().to_string())
    }

    pub fn set(&mut self, name: &str, raw: &str) -> Result<()> {
        let key = find_key(name)?;
        let value = toml_value(key, raw)?;
        let (table, leaf) = Self::split(name);
        match table {
            Some(t) => {
                let entry = self
                    .doc
                    .entry(t)
                    .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
                let Some(table) = entry.as_table_like_mut() else {
                    bail!("{}: [{t}] is not a table", self.path.display());
                };
                table.insert(leaf, toml_edit::value(value));
            }
            None => {
                self.doc.insert(leaf, toml_edit::value(value));
            }
        }
        Ok(())
    }

    /// Remove a key; returns whether it was set.
    pub fn unset(&mut self, name: &str) -> Result<bool> {
        find_key(name)?;
        let (table, leaf) = Self::split(name);
        Ok(match table {
            Some(t) => self
                .doc
                .get_mut(t)
                .and_then(|item| item.as_table_like_mut())
                .and_then(|table| table.remove(leaf))
                .is_some(),
            None => self.doc.remove(leaf).is_some(),
        })
    }

    /// Write the file, after checking that it still loads.
    pub fn save(&self) -> Result<()> {
        let text = self.doc.to_string();
        toml::from_str::<Settings>(&text)
            .with_context(|| format!("refusing to write an invalid {}", self.path.display()))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Parse a duration: a number with an optional unit s, m or h (e.g. 30s,
/// 5m, 2h; a bare number is seconds).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration {s:?}: expected e.g. 30s, 5m or 2h"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("invalid duration unit in {s:?}: use s, m or h")),
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

#[cfg(test)]
mod tests {
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
        file.set("no_default_bootstrap", "yes").unwrap();
        assert!(file.set("agent.every", "soon").is_err());
        assert!(file.set("agent.runtime", "gpt").is_err());
        assert!(file.set("profile", "Bad Name").is_err());
        assert!(file.set("agent.colour", "x").is_err());
        assert!(file.set("port", "70000").is_err());
        file.save().unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("# mine") && text.contains("# keep"), "{text}");
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(settings.agent.allow, ["aa", "bb"]);
        assert_eq!(settings.agent.max_runs_per_hour, Some(6));
        assert_eq!(settings.no_default_bootstrap, Some(true));
        let mut file = ConfigFile::open(dir.path()).unwrap();
        assert_eq!(file.get("agent.runtime").as_deref(), Some("\"claude\""));
        assert!(file.unset("agent.runtime").unwrap());
        assert!(!file.unset("agent.runtime").unwrap());
        file.save().unwrap();
        assert!(Settings::load(dir.path()).unwrap().agent.runtime.is_none());
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("7"), Ok(Duration::from_secs(7)));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("1d").is_err());
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("20GiB"), Ok(20 << 30));
        assert_eq!(parse_size("1 kb"), Ok(1024));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5X").is_err());
    }
}
