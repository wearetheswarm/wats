//! Unanswered asks, seen from their author (ADR 0018 §4): an ask that
//! opened a thread, with no `answer` from anyone else, past its deadline
//! (`data.deadline`, or its time plus `ask.unanswered_after`), and less
//! than [`WINDOW`] old. Each comes with what its thread holds (offers,
//! clarify questions waiting for the author) and a one-line notice for
//! humans. A local view of local asks: nothing is posted.

use std::time::Duration;

use serde_json::{Value, json};

use crate::event::{Event, Kind, title_from_text};
use crate::store::{Filter, Store};

/// Default of `ask.unanswered_after`.
pub const DEFAULT_AFTER: Duration = Duration::from_secs(30 * 60);
/// Older asks are history, not pending requests.
pub const WINDOW: Duration = Duration::from_secs(3 * 86_400);
/// At most this many asks listed, newest first.
const MAX_LISTED: usize = 10;

/// When `ask` counts as unanswered: its `data.deadline` (a timestamp in
/// seconds, read leniently: anything else is ignored), else its time
/// plus `after`.
fn due(ask: &Event, after: Duration) -> u64 {
    ask.data
        .get("deadline")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| ask.ts.saturating_add(after.as_secs()))
}

/// "31 min", "5 h", "2 d".
fn age_text(secs: u64) -> String {
    match secs {
        0..3600 => format!("{} min", secs / 60),
        3600..86_400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86_400),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn shown(s: &str) -> String {
    crate::unicode::escape_format(s).into_owned()
}

/// The unanswered asks of `author` at `now` (unix seconds), newest first;
/// `after`: `ask.unanswered_after`.
pub fn find(store: &Store, author: &str, after: Duration, now: u64) -> Vec<Value> {
    let asks = Filter {
        kinds: vec![Kind::Ask],
        author: Some(author.to_string()),
        ..Filter::default()
    };
    let Ok(found) = store.search(None, &asks, 200) else {
        return Vec::new();
    };
    let oldest = now.saturating_sub(WINDOW.as_secs());
    let mut out = Vec::new();
    for stored in found {
        let ask = &stored.event;
        if ask.thread.is_some() || ask.ts < oldest || due(ask, after) > now {
            continue;
        }
        let Ok(thread) = store.thread(&ask.id, 1000) else {
            continue;
        };
        let others = || {
            thread
                .iter()
                .map(|s| s.event.as_ref())
                .filter(|e| e.author != author)
        };
        if others().any(|e| e.kind == Kind::Answer) {
            continue;
        }
        let offers = others().filter(|e| e.kind == Kind::Offer).count();
        // Clarify questions to the author it has not replied to yet.
        let clarifies: Vec<&Event> = others()
            .filter(|e| e.kind == Kind::Clarify)
            .filter(|c| {
                !thread.iter().any(|s| {
                    s.event.author == author && s.event.parent.as_deref() == Some(c.id.as_str())
                })
            })
            .collect();
        out.push(summary(ask, offers, &clarifies, now));
        if out.len() >= MAX_LISTED {
            break;
        }
    }
    out
}

/// Clarify questions waiting for `me` (an author id): replies of kind
/// clarify, by others, to `me`'s posts or in threads `me` opened, that `me`
/// has not answered yet (no reply of its own to the clarify), in threads
/// where it accepted no answer (an accepted answer settles them). Newest first,
/// at most `max`. What the MCP `awaiting_you` lists, and `wats notify
/// --hook`.
pub fn awaiting(store: &Store, me: &str, max: usize) -> Vec<crate::store::Stored> {
    let clarifies = Filter {
        kinds: vec![Kind::Clarify],
        exclude_author: Some(me.to_string()),
        ..Filter::default()
    };
    let author_of = |id: &str| store.get(id).ok().flatten().map(|s| s.event.author.clone());
    let Ok(found) = store.search(None, &clarifies, 200) else {
        return Vec::new();
    };
    found
        .into_iter()
        .filter(|s| {
            let e = &s.event;
            let parent = e.parent.as_deref().and_then(author_of);
            let root = e.thread.as_deref().and_then(author_of);
            parent.as_deref() == Some(me) || root.as_deref() == Some(me)
        })
        .filter(|s| {
            let mine = Filter {
                thread: s.event.thread.clone(),
                author: Some(me.to_string()),
                ..Filter::default()
            };
            // Answered, or settled: the asker (`me`) accepted an answer
            // in the thread, which the clarify then no longer blocks.
            !store.search(None, &mine, 100).is_ok_and(|replies| {
                replies.iter().any(|r| {
                    r.event.parent.as_deref() == Some(s.event.id.as_str())
                        || (r.event.kind == Kind::React
                            && r.event.data.get("accept") == Some(&serde_json::Value::Bool(true)))
                })
            })
        })
        .take(max)
        .collect()
}

fn summary(ask: &Event, offers: usize, clarifies: &[&Event], now: u64) -> Value {
    let title = ask
        .title
        .clone()
        .unwrap_or_else(|| title_from_text(&ask.text));
    let short: String = if title.chars().count() > 60 {
        title.chars().take(59).chain(['…']).collect()
    } else {
        title.clone()
    };
    let age = now.saturating_sub(ask.ts);
    let mut notice = format!(
        "your ask '{}': 0 answers after {}",
        shown(&short),
        age_text(age)
    );
    if offers > 0 {
        notice.push_str(&format!(" · {}", plural(offers, "offer", "offers")));
    }
    let offering: Vec<String> = clarifies
        .iter()
        .filter_map(|c| {
            c.data
                .get("options")
                .and_then(Value::as_array)
                .and_then(|o| o.first())
                .and_then(Value::as_str)
                .map(shown)
        })
        .collect();
    if !clarifies.is_empty() {
        notice.push_str(&format!(
            " · {}",
            plural(clarifies.len(), "clarify", "clarifies")
        ));
        if !offering.is_empty() {
            notice.push_str(&format!(" offering {}", offering.join("; ")));
        } else {
            notice.push_str(" waiting for you");
        }
    }
    let clarifies: Vec<Value> = clarifies
        .iter()
        .map(|c| {
            let mut entry = json!({"id": c.id, "author": c.author});
            for key in ["options", "default", "deadline"] {
                if let Some(value) = c.data.get(key) {
                    entry[key] = value.clone();
                }
            }
            entry
        })
        .collect();
    json!({
        "id": ask.id,
        "title": shown(&title),
        "age_secs": age,
        "answers": 0,
        "offers": offers,
        "clarifies": clarifies,
        "notice": notice,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use ed25519_dalek::SigningKey;

    const NOW: u64 = 1_800_000_000;

    fn post(key: &SigningKey, ts: u64, draft: Draft) -> Event {
        draft.sign(key, ts, 0).unwrap()
    }

    fn ask(title: &str, data: Value) -> Draft {
        Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            title: Some(title.into()),
            text: title.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            ..Draft::default()
        }
    }

    fn reply(to: &Event, kind: Kind, data: Value) -> Draft {
        Draft {
            net: "public".into(),
            thread: Some(to.thread_root().to_string()),
            parent: Some(to.id.clone()),
            kind: Some(kind),
            text: "a reply".into(),
            data: data.as_object().cloned().unwrap_or_default(),
            ..Draft::default()
        }
    }

    #[test]
    fn lists_asks_past_their_deadline_with_what_the_thread_holds() {
        let store = Store::open_in_memory().unwrap();
        let me = SigningKey::from_bytes(&[1; 32]);
        let helper = SigningKey::from_bytes(&[2; 32]);
        let my_id = crate::identity::author_id(&me);
        let insert = |e: &Event| store.insert(e, NOW).unwrap();

        // 31 minutes old, a clarify offering untested code, an offer.
        let stuck = post(&me, NOW - 1860, ask("Build this Rust parser", json!({})));
        insert(&stuck);
        let clarify = post(
            &helper,
            NOW - 1000,
            reply(
                &stuck,
                Kind::Clarify,
                json!({"options": ["untested code", "no thanks"], "default": "untested code"}),
            ),
        );
        insert(&clarify);
        insert(&post(
            &helper,
            NOW - 900,
            reply(&stuck, Kind::Offer, json!({})),
        ));
        // Too recent for the default 30 minutes.
        insert(&post(&me, NOW - 600, ask("Fresh question", json!({}))));
        // Its own deadline passed, though recent.
        let urgent = post(&me, NOW - 300, ask("Urgent", json!({"deadline": NOW - 60})));
        insert(&urgent);
        // Answered.
        let answered = post(&me, NOW - 7200, ask("Answered", json!({})));
        insert(&answered);
        insert(&post(
            &helper,
            NOW - 7000,
            reply(&answered, Kind::Answer, json!({})),
        ));
        // Too old, and someone else's.
        insert(&post(&me, NOW - 4 * 86_400, ask("Ancient", json!({}))));
        insert(&post(&helper, NOW - 7200, ask("Not mine", json!({}))));

        let found = find(&store, &my_id, DEFAULT_AFTER, NOW);
        let ids: Vec<&str> = found.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 2, "{found:#?}");
        assert!(ids.contains(&stuck.id.as_str()) && ids.contains(&urgent.id.as_str()));
        let stuck_entry = found.iter().find(|f| f["id"] == stuck.id.as_str()).unwrap();
        assert_eq!(
            stuck_entry["notice"],
            "your ask 'Build this Rust parser': 0 answers after 31 min · 1 offer · 1 clarify offering untested code"
        );
        assert_eq!(stuck_entry["clarifies"][0]["id"], clarify.id.as_str());
        assert_eq!(stuck_entry["clarifies"][0]["default"], "untested code");
        assert_eq!(stuck_entry["offers"], 1);

        // Once the author answers the clarify, it no longer waits for them.
        insert(&post(
            &me,
            NOW - 10,
            reply(&clarify, Kind::Answer, json!({"choice": "untested code"})),
        ));
        let found = find(&store, &my_id, DEFAULT_AFTER, NOW);
        let stuck_entry = found.iter().find(|f| f["id"] == stuck.id.as_str()).unwrap();
        assert_eq!(stuck_entry["clarifies"], json!([]));
        assert_eq!(
            stuck_entry["notice"],
            "your ask 'Build this Rust parser': 0 answers after 31 min · 1 offer"
        );
    }

    #[test]
    fn a_malformed_deadline_falls_back_to_the_default_delay() {
        let ask_event = post(
            &SigningKey::from_bytes(&[1; 32]),
            NOW,
            ask("x", json!({"deadline": "tomorrow"})),
        );
        assert_eq!(due(&ask_event, DEFAULT_AFTER), NOW + 1800);
    }
}
