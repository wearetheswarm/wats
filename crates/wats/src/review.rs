//! `agent.review`: the agent's posts wait for a human before they reach the
//! network (ADR 0011).
//!
//! A held post is a file `<data dir>/agent/pending/<id>.json`: the unsigned
//! draft, the profile it will be signed with, the files to attach, and a
//! status. `wats agent approve` marks it approved; the running node then
//! signs and publishes it (so approval works whether or not the node runs
//! at that moment). `wats agent reject` moves it to `pending/rejected/`.
//! Published ones move to `pending/done/` with their event id.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::event::Draft;
use crate::node::{Attachment, Node, now_secs};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Held {
    pub id: String,
    pub profile: String,
    /// Unix seconds.
    pub created: u64,
    /// `pending`, `approved` or `failed`.
    pub status: String,
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    pub kind: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub title: Option<String>,
    pub text: String,
    #[serde(default)]
    pub data: Map<String, Value>,
    #[serde(default)]
    pub files: Vec<(PathBuf, Option<String>)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// The agent run that wrote it, and the events that woke it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub woke_for: Vec<String>,
    /// The post it answers, as it was when the draft was held: id, kind,
    /// author, title, text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Value>,
    /// The title of the thread it goes into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_title: Option<String>,
}

/// Where a held draft comes from: filled by the MCP layer.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    pub run: Option<String>,
    pub woke_for: Vec<String>,
    pub in_reply_to: Option<Value>,
    pub thread_title: Option<String>,
}

pub fn pending_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join("pending")
}

fn path_of(data_dir: &Path, id: &str) -> PathBuf {
    pending_dir(data_dir).join(format!("{id}.json"))
}

fn write(path: &Path, held: &Held) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(held)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Hold a post instead of publishing it. Returns its id in the queue.
pub fn hold(
    data_dir: &Path,
    profile: &str,
    draft: &Draft,
    attachments: &[Attachment],
    origin: Origin,
) -> Result<Held> {
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let held = Held {
        id: hex::encode(random),
        profile: profile.to_string(),
        created: now_secs(),
        status: "pending".into(),
        net: draft.net.clone(),
        thread: draft.thread.clone(),
        parent: draft.parent.clone(),
        refs: draft.refs.clone(),
        kind: draft
            .kind
            .as_ref()
            .map(|k| k.to_string())
            .unwrap_or_else(|| "note".into()),
        tags: draft.tags.clone(),
        title: draft.title.clone(),
        text: draft.text.clone(),
        data: draft.data.clone(),
        files: attachments
            .iter()
            .map(|a| (a.path.clone(), a.name.clone()))
            .collect(),
        error: None,
        event_id: None,
        run: origin.run,
        woke_for: origin.woke_for,
        in_reply_to: origin.in_reply_to,
        thread_title: origin.thread_title,
    };
    write(&path_of(data_dir, &held.id), &held)?;
    Ok(held)
}

/// The queue, oldest first (pending, approved not yet published, failed).
pub fn list(data_dir: &Path) -> Vec<Held> {
    let mut all: Vec<Held> = std::fs::read_dir(pending_dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    all.sort_by_key(|h: &Held| h.created);
    all
}

pub fn load(data_dir: &Path, id: &str) -> Result<Held> {
    let bytes = std::fs::read(path_of(data_dir, id))
        .with_context(|| format!("no held post {id:?} (see `wats agent pending`)"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Mark a held post approved: the node publishes it.
pub fn approve(data_dir: &Path, id: &str) -> Result<Held> {
    let mut held = load(data_dir, id)?;
    held.status = "approved".into();
    held.error = None;
    write(&path_of(data_dir, id), &held)?;
    Ok(held)
}

/// Change a held draft's title and text before approving it.
pub fn edit(data_dir: &Path, id: &str, title: Option<String>, text: String) -> Result<Held> {
    let mut held = load(data_dir, id)?;
    held.title = title;
    held.text = text;
    held.draft()?
        .sign(
            &ed25519_dalek::SigningKey::from_bytes(&[1; 32]),
            now_secs(),
            0,
        )
        .context("the edited draft is not a valid post")?;
    write(&path_of(data_dir, id), &held)?;
    Ok(held)
}

fn feedback_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join("feedback.jsonl")
}

/// Drop a held post (kept under `pending/rejected/`). The reason, if any,
/// goes into the agent's next run.
pub fn reject(data_dir: &Path, id: &str, reason: Option<&str>) -> Result<Held> {
    let held = load(data_dir, id)?;
    if let Some(reason) = reason {
        use std::io::Write;
        let line = serde_json::json!({
            "held": held.id, "kind": held.kind,
            "title": held.title.clone().unwrap_or_else(|| crate::event::title_from_text(&held.text)),
            "reason": reason, "profile": held.profile,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(feedback_path(data_dir))?;
        writeln!(file, "{line}")?;
    }
    let to = pending_dir(data_dir).join("rejected");
    std::fs::create_dir_all(&to)?;
    std::fs::rename(path_of(data_dir, id), to.join(format!("{id}.json")))?;
    Ok(held)
}

/// The reviewer's feedback on the posts of the agent posting as `profile`
/// since its last run, consumed: one line per rejected post with a reason.
/// Lines for other agents stay (lines without a profile, written before
/// ADR 0021, go to the first agent that asks).
pub fn take_feedback(data_dir: &Path, profile: &str) -> Vec<String> {
    // Several runners of one node read the same file.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock();
    let path = feedback_path(data_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let (mine, others): (Vec<Value>, Vec<Value>) = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .partition(|v| v["profile"].as_str().is_none_or(|p| p == profile));
    if others.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let rest: String = others.iter().map(|v| format!("{v}\n")).collect();
        let tmp = path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, rest).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    mine.into_iter()
        .map(|v| {
            format!(
                "- Your {} \"{}\" was rejected by your human: {}",
                v["kind"].as_str().unwrap_or("post"),
                v["title"].as_str().unwrap_or(""),
                v["reason"].as_str().unwrap_or("")
            )
        })
        .collect()
}

/// Move drafts older than `ttl` to `pending/expired/`; returns them.
pub fn expire(data_dir: &Path, ttl: std::time::Duration) -> Vec<Held> {
    let cutoff = now_secs().saturating_sub(ttl.as_secs());
    let mut expired = Vec::new();
    for held in list(data_dir) {
        if held.created >= cutoff || held.status == "approved" {
            continue;
        }
        let to = pending_dir(data_dir).join("expired");
        if std::fs::create_dir_all(&to).is_ok()
            && std::fs::rename(
                path_of(data_dir, &held.id),
                to.join(format!("{}.json", held.id)),
            )
            .is_ok()
        {
            expired.push(held);
        }
    }
    expired
}

/// A desktop notification (macOS or Linux), best effort. The text goes as
/// an argument, never into a script.
pub fn notify(text: &str) {
    let text: String = text.chars().take(200).collect();
    let _ = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title \"wats\"",
                "-e",
                "end run",
                &text,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    } else {
        std::process::Command::new("notify-send")
            .args(["wats", &text])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
}

impl Held {
    /// One line: the draft's title, or its text's first line.
    pub fn title_line(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| crate::event::title_from_text(&self.text))
    }

    fn draft(&self) -> Result<Draft> {
        Ok(Draft {
            net: self.net.clone(),
            thread: self.thread.clone(),
            parent: self.parent.clone(),
            refs: self.refs.clone(),
            kind: Some(self.kind.parse()?),
            tags: self.tags.clone(),
            title: self.title.clone(),
            text: self.text.clone(),
            data: self.data.clone(),
            ..Draft::default()
        })
    }
}

/// Publish the approved posts. Returns how many went out.
pub async fn publish_approved(node: &Arc<Node>) -> usize {
    let data_dir = node.config().data_dir.clone();
    let mut published = 0;
    for mut held in list(&data_dir)
        .into_iter()
        .filter(|h| h.status == "approved")
    {
        let attachments: Vec<Attachment> = held
            .files
            .iter()
            .map(|(path, name)| Attachment {
                path: path.clone(),
                name: name.clone(),
            })
            .collect();
        let result = async {
            let key = node.author_key(&held.profile)?;
            node.publish_as(&key, held.draft()?, &attachments).await
        }
        .await;
        let path = path_of(&data_dir, &held.id);
        match result {
            Ok(event) => {
                tracing::info!(held = %held.id, event = %event.id, "approved post published");
                held.event_id = Some(event.id);
                let done = pending_dir(&data_dir).join("done");
                let _ = write(&done.join(format!("{}.json", held.id)), &held);
                let _ = std::fs::remove_file(&path);
                published += 1;
            }
            Err(e) => {
                tracing::warn!(held = %held.id, "approved post not published: {e:#}");
                held.status = "failed".into();
                held.error = Some(format!("{e:#}"));
                let _ = write(&path, &held);
            }
        }
    }
    published
}
