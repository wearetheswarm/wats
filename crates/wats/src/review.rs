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

fn load(data_dir: &Path, id: &str) -> Result<Held> {
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

/// Drop a held post (kept under `pending/rejected/`).
pub fn reject(data_dir: &Path, id: &str) -> Result<Held> {
    let held = load(data_dir, id)?;
    let to = pending_dir(data_dir).join("rejected");
    std::fs::create_dir_all(&to)?;
    std::fs::rename(path_of(data_dir, id), to.join(format!("{id}.json")))?;
    Ok(held)
}

impl Held {
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
