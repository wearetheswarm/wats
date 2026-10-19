//! Safety settings (ADR 0011, addendum): the config.toml keys that, once
//! changed, make the node or its agent less safe (halts ignored, review
//! off, access widened, budgets raised, updates or reports redirected).
//!
//! An agent runs as the user, so a same-UID process can always rewrite
//! config.toml. The runner denies it where the runtime can express it
//! ([`crate::access`]); this module catches what slips through: the node
//! keeps the values it last saw in `<data dir>/safety.json` (values, not a
//! hash, so that it can say what changed). The first time, they are
//! recorded silently. Any later difference is logged at startup and shown
//! by `wats status` and the MCP `status` tool until the user runs
//! `wats config ack`, which no MCP tool can do. Changes made with
//! `wats config set` are flagged too: the user acknowledges them
//! explicitly.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The safety-relevant keys of config.toml.
pub const KEYS: &[&str] = &[
    "halt.accept",
    "halt.serve",
    "agent.runtime",
    "agent.access",
    "agent.workdir",
    "agent.tools",
    "agent.allow",
    "agent.review",
    "agent.ignore_local",
    "agent.command",
    "agent.args",
    "agent.heartbeat_file",
    "agent.openclaw_url",
    "agent.openclaw_insecure_ok",
    "agent.openclaw_agent",
    "agent.max_usd_per_day",
    "agent.max_usd_per_month",
    "agent.max_runs_per_hour",
    "agent.max_runs_per_day",
    "agent.wakes_per_author_per_hour",
    "agent.loop_max_exchanges",
    "agent.timeout",
    "agent.max_turns",
    "report.inbox",
    "report.collector",
    "report.crashes",
    "update.check",
    "update.auto",
    "update.delay",
    "update.security_delay",
    "update.url",
    "limits.challenge",
    "storage.gc",
];

pub const FILE_NAME: &str = "safety.json";

/// Shown with every change notice: humans acknowledge, agents do not.
pub const ACK_HINT: &str =
    "(for the user: review, then run `wats config ack` in a terminal; agents must not run it)";

/// What `wats config set` prints after changing a safety key.
pub const SET_HINT: &str = "safety setting changed: run `wats config ack` to acknowledge";

/// Shown for a key that config.toml does not set.
const DEFAULT: &str = "default";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    /// The acknowledged values, as JSON text (`default` when unset).
    baseline: BTreeMap<String, String>,
    /// When a difference was first seen (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changed_at: Option<u64>,
}

/// One key whose value differs from the acknowledged one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub key: String,
    pub from: String,
    pub to: String,
}

/// Unacknowledged changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// When they were first seen (Unix seconds).
    pub at: u64,
    pub changes: Vec<Change>,
}

impl Notice {
    /// `safety settings changed on 2026-10-18: halt.accept true→false, …
    /// (for the user: …)`.
    pub fn message(&self) -> String {
        let (y, m, d, ..) = crate::runs::civil(self.at);
        format!(
            "safety settings changed on {y:04}-{m:02}-{d:02}: {} {ACK_HINT}",
            changes_line(&self.changes)
        )
    }
}

/// `halt.accept true→false, agent.access "locked"→"inherit"`.
pub fn changes_line(changes: &[Change]) -> String {
    changes
        .iter()
        .map(|c| format!("{} {}→{}", c.key, c.from, c.to))
        .collect::<Vec<_>>()
        .join(", ")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The safety keys' values in config.toml, as compact JSON (`default`
/// when unset): a reformatted file is not a change.
pub fn current(data_dir: &Path) -> Result<BTreeMap<String, String>> {
    let path = data_dir.join(crate::settings::FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    Ok(KEYS
        .iter()
        .map(|name| {
            let value = match name.split_once('.') {
                Some((t, leaf)) => table.get(t).and_then(|t| t.get(leaf)),
                None => table.get(*name),
            };
            let shown = value
                .and_then(|v| serde_json::to_string(v).ok())
                .unwrap_or_else(|| DEFAULT.to_string());
            (name.to_string(), shown)
        })
        .collect())
}

fn read(path: &Path) -> Result<Option<Record>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write(path: &Path, record: &Record) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

fn diff(baseline: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<Change> {
    now.iter()
        .filter_map(|(key, to)| {
            let from = baseline.get(key)?;
            (from != to).then(|| Change {
                key: key.clone(),
                from: from.clone(),
                to: to.clone(),
            })
        })
        .collect()
}

/// Compare config.toml with the acknowledged values. The first time (no
/// `safety.json`), record them silently. Keys a newer version added are
/// recorded silently too.
pub fn check(data_dir: &Path) -> Result<Option<Notice>> {
    let now = current(data_dir)?;
    let path = data_dir.join(FILE_NAME);
    let Some(mut record) = read(&path)? else {
        write(
            &path,
            &Record {
                baseline: now,
                changed_at: None,
            },
        )?;
        return Ok(None);
    };
    let mut dirty = false;
    for (key, value) in &now {
        if !record.baseline.contains_key(key) {
            record.baseline.insert(key.clone(), value.clone());
            dirty = true;
        }
    }
    let changes = diff(&record.baseline, &now);
    let notice = if changes.is_empty() {
        // Changed back: nothing left to acknowledge.
        dirty |= record.changed_at.take().is_some();
        None
    } else {
        let at = *record.changed_at.get_or_insert_with(|| {
            dirty = true;
            unix_now()
        });
        Some(Notice { at, changes })
    };
    if dirty {
        write(&path, &record)?;
    }
    Ok(notice)
}

/// `wats config ack`: record the current values as acknowledged. Returns
/// what changed since the last acknowledgement.
pub fn ack(data_dir: &Path) -> Result<Vec<Change>> {
    let now = current(data_dir)?;
    let path = data_dir.join(FILE_NAME);
    let changes = read(&path)?
        .map(|r| diff(&r.baseline, &now))
        .unwrap_or_default();
    write(
        &path,
        &Record {
            baseline: now,
            changed_at: None,
        },
    )?;
    Ok(changes)
}

/// The notice for `status`, if any; a broken config.toml or safety.json is
/// shown as such rather than hidden.
pub fn status_notice(data_dir: &Path) -> Option<String> {
    match check(data_dir) {
        Ok(notice) => notice.map(|n| n.message()),
        Err(e) => Some(format!(
            "safety settings could not be checked: {e:#} {ACK_HINT}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_exist() {
        for key in KEYS {
            crate::settings::find_key(key).unwrap();
        }
    }

    #[test]
    fn baseline_change_ack() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        std::fs::write(data.join("config.toml"), "[agent]\naccess = \"locked\"\n").unwrap();
        // First run: recorded silently.
        assert_eq!(check(data).unwrap(), None);
        assert!(data.join(FILE_NAME).exists());
        assert_eq!(check(data).unwrap(), None);
        // A reformatted file is not a change.
        std::fs::write(
            data.join("config.toml"),
            "# mine\n[agent]\naccess   =   'locked'\n",
        )
        .unwrap();
        assert_eq!(check(data).unwrap(), None);
        std::fs::write(
            data.join("config.toml"),
            "[agent]\naccess = \"inherit\"\n[halt]\naccept = false\n",
        )
        .unwrap();
        let notice = check(data).unwrap().unwrap();
        let message = notice.message();
        assert!(
            message.starts_with("safety settings changed on "),
            "{message}"
        );
        assert!(
            message.contains("agent.access \"locked\"→\"inherit\""),
            "{message}"
        );
        assert!(message.contains("halt.accept default→false"), "{message}");
        assert!(message.ends_with(ACK_HINT), "{message}");
        // The date of the first sighting stays.
        assert_eq!(check(data).unwrap().unwrap().at, notice.at);
        let acked = ack(data).unwrap();
        assert_eq!(acked, notice.changes);
        assert_eq!(check(data).unwrap(), None);
        // Changed and changed back: nothing to acknowledge.
        std::fs::write(data.join("config.toml"), "[agent]\naccess = \"locked\"\n").unwrap();
        assert!(check(data).unwrap().is_some());
        std::fs::write(
            data.join("config.toml"),
            "[agent]\naccess = \"inherit\"\n[halt]\naccept = false\n",
        )
        .unwrap();
        assert_eq!(check(data).unwrap(), None);
    }
}
