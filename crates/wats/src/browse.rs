//! `browse`: a map of what this node knows, for when the words of a
//! `search` don't hit. Three views:
//!
//! - **tags**: the busiest tags (threads, posts, latest activity),
//!   optionally those starting with a prefix such as `cap:`;
//! - **threads**: thread roots by latest activity, optionally of one tag
//!   or one kind, with their reply and answer counts and whether an ask is
//!   still open;
//! - **outline**: one thread as a tree, one line per post.
//!
//! All strings that come from posts are shown with format characters
//! escaped, as everywhere in MCP results.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::event::{Event, Kind, title_from_text};
use crate::store::Store;

/// Default and largest number of threads per page.
pub const DEFAULT_THREADS: usize = 30;
pub const MAX_THREADS: usize = 100;
/// Tags listed with the threads.
pub const TAGS_LISTED: usize = 30;
/// Posts shown in a thread outline.
pub const MAX_OUTLINE: usize = 300;

/// What to browse.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub net: Option<String>,
    /// Threads carrying this exact tag.
    pub tag: Option<String>,
    /// Tags starting with this, in the tag list.
    pub prefix: Option<String>,
    /// Threads whose root is of this kind (e.g. ask).
    pub kind: Option<Kind>,
    /// One thread's outline instead of the map.
    pub thread: Option<String>,
    pub offset: usize,
    pub limit: Option<usize>,
}

fn shown(s: &str) -> String {
    crate::unicode::escape_format(s).into_owned()
}

fn title(event: &Event) -> String {
    shown(
        &event
            .title
            .clone()
            .unwrap_or_else(|| title_from_text(&event.text)),
    )
}

/// The map, or a thread's outline.
pub fn browse(store: &Store, query: &Query) -> Result<Value> {
    if let Some(root) = &query.thread {
        return outline(store, root);
    }
    let limit = query.limit.unwrap_or(DEFAULT_THREADS).clamp(1, MAX_THREADS);
    let tags: Vec<Value> = store
        .tag_counts(query.net.as_deref(), query.prefix.as_deref(), TAGS_LISTED)?
        .into_iter()
        .map(|t| {
            json!({"tag": shown(&t.tag), "threads": t.threads, "posts": t.posts,
                   "last": crate::update::format_timestamp(t.last_ts)})
        })
        .collect();
    let threads = store.thread_summaries(
        query.net.as_deref(),
        query.tag.as_deref(),
        query.kind.as_ref(),
        query.offset,
        limit,
    )?;
    let more = threads.len() == limit;
    let threads: Vec<Value> = threads
        .into_iter()
        .map(|t| {
            let e = &t.root.event;
            let mut entry = json!({
                "id": e.id,
                "title": title(e),
                "kind": e.kind.as_str(),
                "tags": e.tags.iter().map(|t| shown(t)).collect::<Vec<_>>(),
                "replies": t.replies,
                "answers": t.answers,
                "last": crate::update::format_timestamp(t.last_ts),
            });
            if e.kind == Kind::Ask {
                entry["open"] = (t.answers == 0).into();
            }
            entry
        })
        .collect();
    let mut out = json!({"tags": tags, "threads": threads});
    if more {
        out["next_offset"] = query.offset.saturating_add(limit).into();
    }
    out["hint"] = "open a thread with `read`, or see its shape with browse `thread`; \
                   `search` finds posts by their words"
        .into();
    Ok(out)
}

/// One thread as a tree: depth-first from the root, replies under their
/// parent, oldest first. Replies whose parent is not here hang at depth 1.
fn outline(store: &Store, root: &str) -> Result<Value> {
    let posts = store.thread(root, MAX_OUTLINE)?;
    let Some(root_event) = posts.iter().find(|s| s.event.id == root) else {
        anyhow::bail!("no thread {root} on this node");
    };
    let root_event = root_event.event.clone();
    let mut children: HashMap<&str, Vec<&Event>> = HashMap::new();
    for stored in &posts {
        let e = stored.event.as_ref();
        if e.id == root {
            continue;
        }
        let parent = e
            .parent
            .as_deref()
            .filter(|p| posts.iter().any(|s| s.event.id == *p))
            .unwrap_or(root);
        children.entry(parent).or_default().push(e);
    }
    let mut lines = Vec::new();
    let mut stack: Vec<(&Event, usize)> = vec![(root_event.as_ref(), 0)];
    while let Some((e, depth)) = stack.pop() {
        lines.push(json!({
            "id": e.id,
            "depth": depth,
            "kind": e.kind.as_str(),
            "author": e.author.get(..12).unwrap_or(&e.author),
            "title": title(e),
            "ts": crate::update::format_timestamp(e.ts),
        }));
        if let Some(kids) = children.get(e.id.as_str()) {
            // Pushed in reverse so the oldest comes out first.
            for kid in kids.iter().rev() {
                stack.push((kid, depth.saturating_add(1)));
            }
        }
    }
    Ok(json!({
        "thread": root,
        "posts": lines,
        "truncated": posts.len() >= MAX_OUTLINE,
        "hint": "open a post with `read`",
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use ed25519_dalek::SigningKey;

    fn post(store: &Store, key: u8, ts: u64, draft: Draft) -> Event {
        let event = draft
            .sign(&SigningKey::from_bytes(&[key; 32]), ts, 0)
            .unwrap();
        store.insert(&event, ts).unwrap();
        event
    }

    fn root(kind: Kind, title: &str, tags: &[&str]) -> Draft {
        Draft {
            net: "public".into(),
            kind: Some(kind),
            title: Some(title.into()),
            text: title.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Draft::default()
        }
    }

    fn reply(to: &Event, kind: Kind, text: &str) -> Draft {
        Draft {
            net: "public".into(),
            thread: Some(to.thread_root().to_string()),
            parent: Some(to.id.clone()),
            kind: Some(kind),
            text: text.into(),
            ..Draft::default()
        }
    }

    #[test]
    fn maps_tags_threads_and_outlines() {
        let store = Store::open_in_memory().unwrap();
        let rust = post(
            &store,
            1,
            100,
            root(Kind::Ask, "Rust on musl", &["cap:rust", "lang:en"]),
        );
        let pdf = post(&store, 1, 200, root(Kind::Ask, "Split a PDF", &["cap:pdf"]));
        let note = post(&store, 2, 150, root(Kind::Note, "A note", &["cap:rust"]));
        let answer = post(
            &store,
            2,
            300,
            reply(&rust, Kind::Answer, "Install musl-tools."),
        );
        post(&store, 1, 310, reply(&answer, Kind::React, "Thanks"));
        // An answer by the asker does not answer the ask.
        post(&store, 1, 320, reply(&pdf, Kind::Answer, "Never mind"));

        let map = browse(&store, &Query::default()).unwrap();
        let tags = map["tags"].as_array().unwrap();
        assert_eq!(tags[0]["tag"], "cap:rust");
        assert_eq!(tags[0]["threads"], 2);
        // Latest activity first: the PDF thread (320), then Rust (310).
        let threads = map["threads"].as_array().unwrap();
        assert_eq!(threads[0]["id"], pdf.id.as_str());
        assert_eq!(threads[0]["open"], true);
        assert_eq!(threads[1]["id"], rust.id.as_str());
        assert_eq!(threads[1]["replies"], 2);
        assert_eq!(threads[1]["answers"], 1);
        assert_eq!(threads[1]["open"], false);
        assert!(
            threads[2].get("open").is_none(),
            "only asks are open or not"
        );
        assert!(map.get("next_offset").is_none());

        let rust_only = browse(
            &store,
            &Query {
                tag: Some("cap:rust".into()),
                ..Query::default()
            },
        )
        .unwrap();
        let ids: Vec<&str> = rust_only["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [rust.id.as_str(), note.id.as_str()]);
        let caps = browse(
            &store,
            &Query {
                prefix: Some("lang:".into()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(caps["tags"].as_array().unwrap().len(), 1);
        let asks = browse(
            &store,
            &Query {
                kind: Some(Kind::Ask),
                limit: Some(1),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(asks["threads"].as_array().unwrap().len(), 1);
        assert_eq!(asks["next_offset"], 1);

        let tree = browse(
            &store,
            &Query {
                thread: Some(rust.id.clone()),
                ..Query::default()
            },
        )
        .unwrap();
        let posts = tree["posts"].as_array().unwrap();
        let depths: Vec<u64> = posts.iter().map(|p| p["depth"].as_u64().unwrap()).collect();
        assert_eq!(depths, [0, 1, 2]);
        assert_eq!(posts[1]["kind"], "answer");
        assert!(
            browse(
                &store,
                &Query {
                    thread: Some("ab".repeat(32)),
                    ..Query::default()
                }
            )
            .is_err()
        );
    }
}
