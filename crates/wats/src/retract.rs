//! Author retractions (ADR 0020 §5.1): the `x.wearetheswarm.retract`
//! extension kind.
//!
//! A retraction is an ordinary signed v1 event of kind [`KIND`]: `refs`
//! lists the author's own posts to retract (1 to [`MAX_REFS`]), `text` a
//! short, possibly generic reason. No core change: its rules are node
//! policy ([`check_policy`]): a **root post** (no thread, no parent) with
//! at least one ref. A root post rather than a reply, because its refs may
//! sit in several threads, and a retraction belongs to none of them.
//!
//! What a node does with one (see `Store::apply_retraction`):
//!
//! - a referenced post it holds, by the retraction's author or by a key
//!   that author rotated from (the `rotate` chain this node holds): its
//!   body is dropped, reason `retraction`, trigger the retraction's id;
//! - a referenced post it does not hold yet: a drop record is written now,
//!   so only its tombstone is stored when it arrives. The author cannot be
//!   checked before the post arrives, so the store checks it then, against
//!   every retraction it holds that cites the post: when none matches, the
//!   record is deleted and the body stored;
//! - a post by someone else (or by a key whose rotation link this node
//!   does not hold yet): ignored, body kept. Retractions stay stored, so
//!   when a `rotate` arrives, the retractions of the new key and its
//!   successors are applied again: a missing link only delays them;
//! - key successions (`rotate`) and halt events are never retracted: nodes
//!   need their bodies to follow keys and halts.
//!
//! Authoring: `wats retract` (humans, any profile, no limit) and the MCP
//! `retract` tool (own posts only). Agent profiles get at most
//! [`AGENT_LIMIT`] retracted posts per rolling [`AGENT_WINDOW_SECS`],
//! counted in `<data dir>/agent/retractions.jsonl` (so a restart does not
//! reset it), which `wats status` also reads; and the owner's node keeps a
//! local audit copy of each post an agent retracts, in
//! `<data dir>/agent/retracted/<id>.json` (mode 0600), never served.

// Retractions come from the network: never panic on them.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::{Draft, Event, Kind};
use crate::wire::{AnyEvent, RawEvent};

/// The extension kind of a retraction.
pub const KIND: &str = "x.wearetheswarm.retract";
/// Most posts one retraction names (the v1 cap on `refs`).
pub const MAX_REFS: usize = crate::event::MAX_REFS_V1;
/// The title of a retraction (a root post needs one).
pub const TITLE: &str = "Retraction";
/// The text when the author gives no reason.
pub const DEFAULT_REASON: &str = "retracted by the author";
/// Longest reason, in characters: a short, generic sentence, never the
/// retracted content.
pub const MAX_REASON_CHARS: usize = 280;
/// Posts an agent profile may retract per rolling window.
pub const AGENT_LIMIT: usize = 3;
/// The rolling window of [`AGENT_LIMIT`]: 24 hours.
pub const AGENT_WINDOW_SECS: u64 = 24 * 3600;
/// How far back `wats status` lists agent retractions: 7 days.
pub const STATUS_SECS: u64 = 7 * 24 * 3600;
/// `<data dir>/agent/retractions.jsonl`: one line per retraction made by an
/// agent profile through MCP.
pub const LOG_FILE: &str = "retractions.jsonl";
/// `<data dir>/agent/retracted/`: the audit copies.
pub const AUDIT_DIR: &str = "retracted";

/// Whether `kind` is the retraction kind.
pub fn is_retract(kind: &str) -> bool {
    kind == KIND
}

/// Kinds a retraction never drops: key successions and halt events, whose
/// bodies nodes need to follow keys and halts.
pub fn unretractable(kind: &str) -> bool {
    kind == Kind::Rotate.as_str() || crate::halt::is_halt_kind(kind)
}

/// Node policy on a retraction: a root post with at least one ref.
pub fn check_policy(thread: Option<&str>, parent: Option<&str>, refs: &[String]) -> Result<()> {
    if refs.is_empty() {
        bail!("a retraction ({KIND}) names at least one post in refs");
    }
    if thread.is_some() || parent.is_some() {
        bail!("a retraction ({KIND}) is a root post, not a reply");
    }
    Ok(())
}

/// The draft of a retraction of `refs` (1 to 32, sorted here) on `net`.
pub fn draft(net: &str, mut refs: Vec<String>, reason: Option<&str>) -> Result<Draft> {
    refs.sort();
    refs.dedup();
    if refs.len() > MAX_REFS {
        bail!("at most {MAX_REFS} posts per retraction");
    }
    check_policy(None, None, &refs)?;
    Ok(Draft {
        net: net.to_string(),
        kind: Some(Kind::Ext(KIND.to_string())),
        title: Some(TITLE.to_string()),
        text: reason_text(reason)?,
        refs,
        ..Draft::default()
    })
}

/// The text of a retraction: the reason, trimmed, or [`DEFAULT_REASON`].
pub fn reason_text(reason: Option<&str>) -> Result<String> {
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    match reason {
        None => Ok(DEFAULT_REASON.to_string()),
        Some(r) if r.chars().count() > MAX_REASON_CHARS => {
            bail!("the reason is at most {MAX_REASON_CHARS} characters: keep it short and generic")
        }
        Some(r) => Ok(r.to_string()),
    }
}

/// A retraction as a node applies it: from the full event, or from its
/// tombstone (its author, id and refs are in the header, so a retraction
/// whose own body was dropped still counts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retraction {
    pub id: String,
    pub author: String,
    pub refs: Vec<String>,
}

impl Retraction {
    pub fn from_event(event: &Event) -> Option<Self> {
        (is_retract(event.kind.as_str())
            && check_policy(
                event.thread.as_deref(),
                event.parent.as_deref(),
                &event.refs,
            )
            .is_ok())
        .then(|| Retraction {
            id: event.id.clone(),
            author: event.author.clone(),
            refs: event.refs.clone(),
        })
    }

    /// A v1 tombstone of a retraction, signature checked.
    pub fn from_raw(raw: &RawEvent) -> Option<Self> {
        (raw.v == 1
            && raw.tombstoned
            && raw.signature_checked
            && is_retract(&raw.kind)
            && check_policy(raw.thread.as_deref(), raw.parent.as_deref(), &raw.refs).is_ok())
        .then(|| Retraction {
            id: raw.id.clone(),
            author: raw.author.clone(),
            refs: raw.refs.clone(),
        })
    }

    pub fn from_any(event: &AnyEvent) -> Option<Self> {
        match event {
            AnyEvent::Known(e) => Self::from_event(e),
            AnyEvent::Opaque(r) => Self::from_raw(r),
        }
    }
}

/// One retraction made by an agent profile through MCP: a line of
/// `<data dir>/agent/retractions.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRetraction {
    /// Unix seconds.
    pub at: u64,
    pub profile: String,
    /// The retracted posts.
    pub ids: Vec<String>,
    pub reason: String,
    /// The retraction events published (one per network).
    #[serde(default)]
    pub events: Vec<String>,
    /// The agent run that made it, when a runner session did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join(LOG_FILE)
}

/// Every agent retraction recorded, oldest first. No file: none. A line
/// that does not parse is skipped.
pub fn agent_log(data_dir: &Path) -> Result<Vec<AgentRetraction>> {
    let path = log_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// Append one agent retraction to the log (mode 0600).
pub fn append_agent_log(data_dir: &Path, entry: &AgentRetraction) -> Result<()> {
    let path = log_path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}

/// Posts `profile` retracted within the rolling window ending at `now`.
pub fn used(entries: &[AgentRetraction], profile: &str, now: u64) -> usize {
    let since = now.saturating_sub(AGENT_WINDOW_SECS);
    entries
        .iter()
        .filter(|e| e.profile == profile && e.at > since)
        .fold(0usize, |n, e| n.saturating_add(e.ids.len()))
}

/// Refuse when `wanted` more posts would take agent profile `profile` over
/// [`AGENT_LIMIT`] in the window. Fails closed: a log that cannot be read
/// refuses.
pub fn check_agent_quota(data_dir: &Path, profile: &str, wanted: usize, now: u64) -> Result<()> {
    let used = used(&agent_log(data_dir)?, profile, now);
    if used.saturating_add(wanted) > AGENT_LIMIT {
        bail!(
            "rate limit: an agent profile retracts at most {AGENT_LIMIT} posts per 24 hours \
             ({used} already, {wanted} asked). Ask your user: they can retract with \
             `wats retract`, with no limit"
        );
    }
    Ok(())
}

/// The agent retractions of the last 7 days, for `wats status`: how many
/// posts, their ids, by which profiles, and the latest. `None` when there
/// are none.
pub fn status_json(data_dir: &Path, now: u64) -> Option<Value> {
    let since = now.saturating_sub(STATUS_SECS);
    let recent: Vec<AgentRetraction> = agent_log(data_dir)
        .ok()?
        .into_iter()
        .filter(|e| e.at > since)
        .collect();
    let latest = recent.last()?;
    let ids: Vec<&str> = recent
        .iter()
        .flat_map(|e| e.ids.iter().map(String::as_str))
        .collect();
    let mut profiles: Vec<&str> = recent.iter().map(|e| e.profile.as_str()).collect();
    profiles.sort_unstable();
    profiles.dedup();
    Some(json!({
        "days": STATUS_SECS / (24 * 3600),
        "count": ids.len(),
        "ids": ids,
        "profiles": profiles,
        "latest": {
            "at": latest.at,
            "profile": latest.profile,
            "reason": latest.reason,
        },
        "see": "wats agent retracted",
    }))
}

/// One line for the human `wats status`.
pub fn status_line(status: &Value) -> String {
    let list = |key: &str| {
        status[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    format!(
        "agent retractions (last {} days): {} post(s) by {}: {} · see: wats agent retracted",
        status["days"],
        status["count"],
        list("profiles"),
        list("ids"),
    )
}

/// Where the audit copy of retracted post `id` lives.
pub fn audit_path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir
        .join("agent")
        .join(AUDIT_DIR)
        .join(format!("{id}.json"))
}

/// Save the full event `event` of post `id` as a local audit copy, mode
/// 0600 in a 0700 directory. Never served to peers nor through MCP. An
/// existing copy is kept (the first one holds the body).
pub fn save_audit_copy(data_dir: &Path, id: &str, event: &Value) -> Result<PathBuf> {
    crate::event::parse_hex::<32>(id, "id")?;
    let path = audit_path(data_dir, id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&serde_json::to_vec_pretty(event)?)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
    }
    Ok(path)
}

/// The audit copy of post `id`, if this node kept one.
pub fn audit_copy(data_dir: &Path, id: &str) -> Result<Option<Value>> {
    crate::event::parse_hex::<32>(id, "id")?;
    let path = audit_path(data_dir, id);
    match std::fs::read(&path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("{} is not an event", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    fn entry(at: u64, profile: &str, ids: usize) -> AgentRetraction {
        AgentRetraction {
            at,
            profile: profile.into(),
            ids: (0..ids).map(|i| format!("{i:064x}")).collect(),
            reason: DEFAULT_REASON.into(),
            events: vec![],
            run: None,
        }
    }

    #[test]
    fn policy_and_draft() {
        assert!(check_policy(None, None, &[]).is_err());
        let id = "ab".repeat(32);
        assert!(check_policy(Some(&id), Some(&id), std::slice::from_ref(&id)).is_err());
        check_policy(None, None, std::slice::from_ref(&id)).unwrap();
        let draft = draft("public", vec![id.clone(), id.clone()], None).unwrap();
        assert_eq!(draft.refs, std::slice::from_ref(&id));
        assert_eq!(draft.text, DEFAULT_REASON);
        assert_eq!(draft.kind, Some(Kind::Ext(KIND.into())));
        let event = draft
            .sign(&ed25519_dalek::SigningKey::from_bytes(&[1; 32]), 1, 0)
            .unwrap();
        assert_eq!(
            Retraction::from_event(&event).unwrap().refs,
            std::slice::from_ref(&id)
        );
        let mut tombstone = crate::wire::tombstone(&serde_json::to_value(&event).unwrap()).unwrap();
        assert!(Retraction::from_raw(&tombstone).is_none(), "unchecked");
        tombstone.verify_signature().unwrap();
        assert_eq!(
            Retraction::from_raw(&tombstone),
            Retraction::from_event(&event)
        );
        assert!(reason_text(Some(&"x".repeat(MAX_REASON_CHARS + 1))).is_err());
        assert_eq!(
            reason_text(Some("  leaked a key ")).unwrap(),
            "leaked a key"
        );
        assert!(super::draft("public", vec![], None).is_err());
        assert!(unretractable("rotate") && unretractable(crate::halt::KIND_HALT));
        assert!(!unretractable("note"));
    }

    #[test]
    fn the_quota_counts_posts_per_profile_in_a_rolling_day() {
        let now = 10 * AGENT_WINDOW_SECS;
        let log = vec![
            entry(now - AGENT_WINDOW_SECS - 1, "agent", 3),
            entry(now - 10, "agent", 2),
            entry(now - 5, "other", 3),
        ];
        assert_eq!(used(&log, "agent", now), 2);
        assert_eq!(used(&log, "other", now), 3);
        assert_eq!(used(&log, "nobody", now), 0);
    }

    #[test]
    fn the_log_survives_and_feeds_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(status_json(dir.path(), 1000).is_none());
        check_agent_quota(dir.path(), "agent", 3, 1000).unwrap();
        append_agent_log(dir.path(), &entry(900, "agent", 2)).unwrap();
        append_agent_log(dir.path(), &entry(950, "agent", 1)).unwrap();
        assert!(check_agent_quota(dir.path(), "agent", 1, 1000).is_err());
        check_agent_quota(dir.path(), "agent", 1, 900 + AGENT_WINDOW_SECS).unwrap();
        let status = status_json(dir.path(), 1000).unwrap();
        assert_eq!(status["count"], 3);
        assert_eq!(status["profiles"], json!(["agent"]));
        assert!(status_line(&status).contains("3 post(s) by agent"));
        assert!(status_json(dir.path(), 950 + STATUS_SECS).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(log_path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn audit_copies_are_private_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let id = "cd".repeat(32);
        assert!(audit_copy(dir.path(), &id).unwrap().is_none());
        let path = save_audit_copy(dir.path(), &id, &json!({"text": "first"})).unwrap();
        save_audit_copy(dir.path(), &id, &json!({"text": "second"})).unwrap();
        assert_eq!(
            audit_copy(dir.path(), &id).unwrap().unwrap()["text"],
            "first"
        );
        assert!(save_audit_copy(dir.path(), "../x", &json!({})).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
