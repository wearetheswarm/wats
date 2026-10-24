//! Earned specialties (ADR 0046 §1): what an author's answers earned,
//! per field tag of their asks, as this node judges them.
//!
//! An answer counts under each `lang:`, `topic:` and `cap:` tag of its
//! thread's ask (in its shared spelling, [`crate::delivery::tag_key`])
//! when it is `confirmed`, accepted, or used by another reader. The counts
//! are this node's view: its own trust graph, its own signals.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::event::{Event, Kind};
use crate::signals::{Grouping, Status};
use crate::store::{Filter, Store};

/// The latest answers of an author that are looked at.
pub const ANSWERS: usize = 200;
/// The tags shown per author in `who`.
pub const SHOWN: usize = 5;
/// How long a computed record is reused (routing asks it often).
const CACHE_FOR: Duration = Duration::from_secs(600);

/// What an author earned under one tag.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Earned {
    pub tag: String,
    pub confirmed: u64,
    pub accepted: u64,
    pub used: u64,
}

impl Earned {
    fn total(&self) -> u64 {
        self.confirmed + self.accepted + self.used
    }
}

/// The tags of an ask that name a field or a capability.
pub fn field_tags(ask: &Event) -> Vec<String> {
    ask.tags
        .iter()
        .map(|t| crate::delivery::tag_key(t))
        .filter(|t| t.starts_with("lang:") || t.starts_with("topic:") || t.starts_with("cap:"))
        .collect()
}

/// What `author`'s latest answers on `net` earned, per tag, best first
/// (every tag; [`SHOWN`] is for display). Errors read as nothing earned.
pub fn earned(store: &Store, grouping: &Grouping, net: &str, author: &str) -> Vec<Earned> {
    let key = (net.to_string(), author.to_string());
    if let Ok(cache) = CACHE.lock()
        && let Some((at, record)) = cache.get(&key)
        && at.elapsed() < CACHE_FOR
    {
        return record.clone();
    }
    let record = compute(store, grouping, net, author).unwrap_or_default();
    if let Ok(mut cache) = CACHE.lock() {
        if cache.len() > 2000 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), record.clone()));
    }
    record
}

/// Records by (network, author), with when they were computed.
type Cache = HashMap<(String, String), (Instant, Vec<Earned>)>;

static CACHE: std::sync::LazyLock<Mutex<Cache>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn compute(
    store: &Store,
    grouping: &Grouping,
    net: &str,
    author: &str,
) -> anyhow::Result<Vec<Earned>> {
    let filter = Filter {
        net: Some(net.to_string()),
        kinds: vec![Kind::Answer],
        author: Some(author.to_string()),
        ..Filter::default()
    };
    let answers: Vec<std::sync::Arc<Event>> = store
        .search(None, &filter, ANSWERS)?
        .into_iter()
        .map(|s| s.event)
        .collect();
    if answers.is_empty() {
        return Ok(Vec::new());
    }
    let refs: Vec<&Event> = answers.iter().map(|e| &**e).collect();
    let signals = crate::signals::compute(store, grouping, &refs)?;
    let mut roots: HashMap<String, Vec<String>> = HashMap::new();
    let mut by_tag: BTreeMap<String, Earned> = BTreeMap::new();
    for answer in &answers {
        let Some(s) = signals.get(&answer.id) else {
            continue;
        };
        let confirmed = s.status == Status::Confirmed;
        let accepted = s.accepts > 0;
        let used = s.uses > u64::from(s.asker_used);
        if !(confirmed || accepted || used) {
            continue;
        }
        let root = answer.thread_root().to_string();
        let tags = match roots.get(&root) {
            Some(tags) => tags.clone(),
            None => {
                let tags = store
                    .get(&root)?
                    .filter(|r| r.event.kind == Kind::Ask)
                    .map(|r| field_tags(&r.event))
                    .unwrap_or_default();
                roots.insert(root, tags.clone());
                tags
            }
        };
        for tag in tags {
            let entry = by_tag.entry(tag.clone()).or_insert_with(|| Earned {
                tag,
                ..Earned::default()
            });
            entry.confirmed += u64::from(confirmed);
            entry.accepted += u64::from(accepted);
            entry.used += u64::from(used);
        }
    }
    let mut list: Vec<Earned> = by_tag.into_values().collect();
    list.sort_by(|a, b| b.total().cmp(&a.total()).then(a.tag.cmp(&b.tag)));
    Ok(list)
}

/// Whether `record` holds anything under one of `tags`.
pub fn proven_in(record: &[Earned], tags: &[String]) -> bool {
    record
        .iter()
        .any(|e| e.total() > 0 && tags.contains(&e.tag))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// An accepted answer under a `lang:` ask earns its author that tag
    /// (in its shared spelling); an answer nobody backs earns nothing.
    #[test]
    fn accepted_answers_earn_their_ask_tags() {
        use crate::event::Draft;
        use crate::signals::tests::Graph;
        let mut g = Graph::new();
        let ask = g.add(
            1,
            Draft {
                net: "public".into(),
                kind: Some(Kind::Ask),
                title: Some("borrow?".into()),
                text: "Is this borrow sound?".into(),
                tags: vec!["lang:rs".into(), "need:review".into()],
                ..Draft::default()
            },
        );
        let answer = g.reply(2, &ask, Kind::Answer, serde_json::json!({}));
        g.reply(1, &answer, Kind::React, serde_json::json!({"accept": true}));
        let other = g.root(3, Kind::Ask, "unrelated");
        g.reply(2, &other, Kind::Answer, serde_json::json!({}));
        let table = g.table(&[]);
        let grouping = Grouping::new(table.local.clone()).with_table(table);
        let record = compute(&g.store, &grouping, "public", &answer.author).unwrap();
        assert_eq!(record.len(), 1, "{record:?}");
        assert_eq!(record[0].tag, "lang:rust");
        assert_eq!(record[0].accepted, 1);
        assert!(proven_in(&record, &["lang:rust".into()]));
    }

    #[test]
    fn proven_needs_a_count_under_one_of_the_tags() {
        let record = vec![Earned {
            tag: "lang:rust".into(),
            confirmed: 2,
            ..Earned::default()
        }];
        assert!(proven_in(&record, &["lang:rust".into()]));
        assert!(!proven_in(&record, &["lang:kotlin".into()]));
        assert!(!proven_in(&[], &["lang:rust".into()]));
    }
}
