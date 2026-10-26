//! Brief lines (ADR 0051): one plain line per event a person watching the
//! node cares about, logged under the tracing target [`TARGET`], and how
//! `wats attach` shows them.
//!
//! - The node logs asks, answers to its posts, votes on them, and its
//!   agents' posts ([`watch`]); the runner logs its runs (`agent.rs`).
//! - `wats attach` on a terminal shows those lines and the warnings, as
//!   `hh:mm:ss  <line>` ([`render`]); `--full` shows the log as it is.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info;

use crate::event::{Event, Kind};
use crate::node::Node;
use crate::signals::{Grouping, Signals, Status};
use crate::specialty::Earned;

/// The tracing target of brief lines.
pub const TARGET: &str = "brief";

/// Events older than this are history: no brief line.
const HISTORY_SECS: u64 = 600;

/// How long a title is shown.
const TITLE_CHARS: usize = 70;

/// Longest name shown.
const NAME_CHARS: usize = 40;

/// A name for `author`: the `data.name` of its latest `announce` (agents
/// since 0.4.20; this node's own agents too, so that the brief view uses
/// the name others see, ADR 0055 §9.1), else a local profile's name, else
/// the start of its id.
pub fn name(node: &Node, author: &str) -> String {
    let local = node.local_profile(author);
    let filter = crate::store::Filter {
        author: Some(author.to_string()),
        ..Default::default()
    };
    node.store()
        .latest_announces(&filter, 1)
        .ok()
        .and_then(|found| found.into_iter().next())
        .and_then(|s| {
            s.event
                .data
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        // A name is the author's word: shown on one line, no control or
        // format characters.
        .map(|n| {
            let n: String = cut(&n, NAME_CHARS)
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            crate::unicode::escape_format(&n).into_owned()
        })
        .filter(|n| !n.is_empty())
        .or(local)
        .unwrap_or_else(|| author.chars().take(8).collect())
}

/// " · 3 files attached (test_run.txt, main.rs, …)" for a post with files:
/// the evidence that comes with an answer. Empty without.
pub fn files(event: &Event) -> String {
    let names: Vec<&str> = event.blobs.iter().map(|b| b.name.as_str()).collect();
    files_line(&names)
}

fn files_line(all: &[&str]) -> String {
    let n = all.len();
    if n == 0 {
        return String::new();
    }
    let names: Vec<String> = all
        .iter()
        .take(3)
        .map(|name| {
            let name: String = cut(name, 40).chars().filter(|c| !c.is_control()).collect();
            crate::unicode::escape_format(&name).into_owned()
        })
        .collect();
    let more = if n > 3 { ", …" } else { "" };
    format!(
        " · {n} {} attached ({}{more})",
        if n == 1 { "file" } else { "files" },
        names.join(", ")
    )
}

/// A post's title for one line: its title, else the start of its text.
pub fn title(event: &Event) -> String {
    let title = event
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| event.text.clone());
    cut(&title, TITLE_CHARS)
}

/// `text` on one line, at most `max` characters.
pub fn cut(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

/// The brief line for a new event on `node`, if it is worth one.
pub fn line_for(node: &Node, event: &Event) -> Option<String> {
    let store = node.store();
    let parent = event
        .parent
        .as_deref()
        .and_then(|p| store.get(p).ok().flatten())
        .map(|s| s.event);
    let local = node.is_local_author(&event.author);
    let who = name(node, &event.author);
    let parent_local = parent
        .as_ref()
        .is_some_and(|p| node.is_local_author(&p.author));
    let ask_title = || {
        store
            .get(event.thread_root())
            .ok()
            .flatten()
            .map(|s| title(&s.event))
            .unwrap_or_default()
    };
    match event.kind {
        Kind::Ask if event.thread.is_none() => {
            let tags = if event.tags.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    event
                        .tags
                        .iter()
                        .map(|t| if t.starts_with("to:") {
                            "to:…"
                        } else {
                            t.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            Some(format!("ask from {who}: {}{tags}", title(event)))
        }
        Kind::Answer if local && !parent_local => Some(format!("{who} answered: {}", title(event))),
        Kind::Answer if parent_local && !local => {
            let what = parent.as_ref().map_or("post", |p| p.kind.as_str());
            Some(format!(
                "answer from {who} to your {what}: {}{}",
                title(event),
                files(event)
            ))
        }
        Kind::Claim if local => Some(format!("{who} claimed: {}", ask_title())),
        Kind::Release if local && crate::delivery::is_decline(event) => {
            let reason = event
                .data
                .get("reason")
                .and_then(Value::as_str)
                .map(|r| cut(r, TITLE_CHARS))
                .unwrap_or_else(|| "no reason".into());
            Some(format!("{who} declined \"{}\": {reason}", ask_title()))
        }
        Kind::Release if local && !crate::delivery::is_withdrawal(event) => {
            Some(format!("{who} released its claim: {}", ask_title()))
        }
        Kind::React if parent_local && !local => {
            let parent = parent.as_ref()?;
            let data = &event.data;
            // On an ask, only a +1 means something (demand): accepted,
            // used and confirmed are about answers.
            let on_ask = parent.kind == Kind::Ask;
            let what = if on_ask {
                if data.get("value").and_then(Value::as_i64) == Some(1) {
                    "voted +1 useful on"
                } else {
                    return None;
                }
            } else if data.get("accept").and_then(Value::as_bool) == Some(true) {
                "accepted"
            } else if data.get("used").and_then(Value::as_bool) == Some(true) {
                "used"
            } else if data.get("value").and_then(Value::as_i64) == Some(1)
                && data.get("evidence").is_some()
            {
                "confirmed"
            } else if data.get("value").and_then(Value::as_i64) == Some(1) {
                "voted +1 useful on"
            } else if data.get("value").and_then(Value::as_i64) == Some(-1) {
                "voted −1 on"
            } else {
                return None;
            };
            Some(format!(
                "{who} {what} your {}: {}",
                parent.kind.as_str(),
                title(parent)
            ))
        }
        _ => None,
    }
}

/// What the brief view remembers across restarts, in
/// `<data dir>/`[`STATE_FILE`]: the status each of this node's posts had,
/// the field tags its authors earned (ADR 0046 §1), and when the last
/// daily summary went out. So a restart neither repeats a line nor
/// misses a change.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Post id → (status, when it was recorded).
    #[serde(default)]
    status: BTreeMap<String, (String, u64)>,
    /// Author → the field tags it earned.
    #[serde(default)]
    earned: BTreeMap<String, Vec<String>>,
    /// When the last daily summary was due (unix seconds); 0: never.
    #[serde(default)]
    summary_at: u64,
    /// Posts older than this with no recorded status are recorded
    /// without a line (the first start with this file: their status may
    /// have changed long ago).
    #[serde(default)]
    seeded_before: u64,
}

/// The brief view's state file, in the data directory.
pub const STATE_FILE: &str = "brief-state.json";

/// The posts whose status is remembered.
const STATE_POSTS: usize = 2000;

/// How often the daily summary goes out.
const SUMMARY_EVERY: u64 = 24 * 3600;

impl State {
    /// The state in `dir`, or a new one seeded at `now` (the file is
    /// missing or unreadable).
    pub fn load(dir: &Path, now: u64) -> State {
        std::fs::read_to_string(dir.join(STATE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| State {
                seeded_before: now,
                ..State::default()
            })
    }

    /// Write it to `dir`; a failure is logged at debug: the next line may
    /// then repeat.
    pub fn save(&mut self, dir: &Path) {
        while self.status.len() > STATE_POSTS {
            let oldest = self
                .status
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => self.status.remove(&id),
                None => break,
            };
        }
        let text = serde_json::to_string(self).unwrap_or_default();
        if let Err(e) = crate::background::write_file(&dir.join(STATE_FILE), &text) {
            tracing::debug!("brief state not saved: {e}");
        }
    }

    /// Record `post`'s status; the line to show when it changed. A post
    /// with nothing recorded was unverified, unless it is older than the
    /// state.
    pub fn status_line(&mut self, post: &Event, signals: &Signals, now: u64) -> Option<String> {
        let new = signals.status.as_str().to_string();
        let old = match self.status.get(&post.id) {
            Some((old, _)) => old.clone(),
            None if post.ts < self.seeded_before => new.clone(),
            None => Status::Unverified.as_str().to_string(),
        };
        self.status.insert(post.id.clone(), (new.clone(), now));
        if old == new {
            return None;
        }
        let why = match signals.status {
            Status::Confirmed if signals.accepted => "accepted by its asker".to_string(),
            Status::Confirmed if signals.uses > 0 => {
                format!("used by {}", count(signals.uses, "reader", "readers"))
            }
            Status::Confirmed => format!(
                "confirmed with evidence by {}",
                count(signals.confirms.max(1), "reader", "readers")
            ),
            Status::Unverified => "its confirmation was withdrawn".to_string(),
            _ => signals.why.clone().unwrap_or_else(|| "contradicted".into()),
        };
        Some(format!(
            "your {} \"{}\" is now {new}: {why}",
            post.kind.as_str(),
            title(post)
        ))
    }

    /// Record what `author` earned, after a sign on its answer of time
    /// `ts`; the lines for tags it had not earned before. An author with
    /// nothing recorded had earned nothing, unless the answer is older
    /// than the state (recorded without a line).
    pub fn earned_lines(
        &mut self,
        who: &str,
        author: &str,
        earned: &[Earned],
        ts: u64,
    ) -> Vec<String> {
        let tags: Vec<String> = earned.iter().map(|e| e.tag.clone()).collect();
        let before = match self.earned.insert(author.to_string(), tags) {
            Some(before) => before,
            None if ts < self.seeded_before => return Vec::new(),
            None => Vec::new(),
        };
        earned
            .iter()
            .filter(|e| !before.contains(&e.tag))
            .map(|e| format!("{who} earned {}", e.tag))
            .collect()
    }

    /// The daily summary when it is due at `now`: what others said of
    /// this node's posts since the last one.
    pub fn summary_due(&mut self, now: u64) -> Option<u64> {
        if self.summary_at == 0 {
            self.summary_at = now;
            return None;
        }
        if now.saturating_sub(self.summary_at) < SUMMARY_EVERY {
            return None;
        }
        Some(std::mem::replace(&mut self.summary_at, now))
    }
}

/// `n thing` / `n things`.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The day's summary line, from what [`Store::signals_received`] counted.
pub fn summary_line(received: &crate::store::Received) -> Option<String> {
    (!received.is_empty()).then(|| format!("today: your posts got {}", received.phrase()))
}

/// `status`'s `memory` on one line: "12,034 posts · your posts got 14
/// useful (+1), 3 used".
pub fn memory_summary(memory: &Value) -> String {
    let posts = memory["posts"].as_u64().unwrap_or(0);
    let received: crate::store::Received =
        serde_json::from_value(memory["received"].clone()).unwrap_or_default();
    let mut line = count(posts, "post", "posts");
    if !received.is_empty() {
        line.push_str(&format!(" · your posts got {}", received.phrase()));
    }
    line
}

/// The status and specialty lines after `event`, when it is a sign about
/// one of this node's posts by someone else (a react, or a reply); `None`
/// when it is not, and the state is unchanged.
pub fn follow_lines(
    node: &Node,
    event: &Event,
    state: &mut State,
    now: u64,
) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    if node.is_local_author(&event.author) {
        return None;
    }
    let store = node.store();
    let post = event
        .parent
        .as_deref()
        .and_then(|p| store.get(p).ok().flatten())
        .map(|s| s.event)?;
    let scored =
        matches!(post.kind, Kind::Ask | Kind::Answer) || crate::knowledge::is_digest(&post.kind);
    if !scored || !node.is_local_author(&post.author) {
        return None;
    }
    let grouping = match node.reputation(&post.net) {
        Ok(table) => Grouping::new(node.local_authors()).with_table(table),
        Err(_) => return None,
    };
    if let Ok(signals) = crate::signals::compute(store, &grouping, &[&post])
        && let Some(signals) = signals.get(&post.id)
        && let Some(line) = state.status_line(&post, signals, now)
    {
        lines.push(line);
    }
    if post.kind == Kind::Answer {
        let earned = crate::specialty::earned_now(store, &grouping, &post.net, &post.author);
        lines.extend(state.earned_lines(&name(node, &post.author), &post.author, &earned, post.ts));
    }
    Some(lines)
}

/// Log a brief line for each new live event of the node, a line when one
/// of its posts changes status or one of its authors earns a field, and
/// once a day what others said of its posts, until it is gone.
pub async fn watch(node: std::sync::Weak<Node>) {
    let Some((mut events, dir)) = node
        .upgrade()
        .map(|n| (n.store().subscribe(), n.config().data_dir.clone()))
    else {
        return;
    };
    let mut state = State::load(&dir, crate::node::now_secs());
    state.summary_due(crate::node::now_secs());
    state.save(&dir);
    let mut hourly = tokio::time::interval(std::time::Duration::from_secs(3600));
    hourly.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            got = events.recv() => match got {
                Ok(stored) => {
                    let event = &stored.event;
                    let now = crate::node::now_secs();
                    if now.saturating_sub(event.ts) > HISTORY_SECS {
                        continue;
                    }
                    let Some(node) = node.upgrade() else { break };
                    if let Some(line) = line_for(&node, event) {
                        info!(target: TARGET, "{line}");
                    }
                    if let Some(follow) = follow_lines(&node, event, &mut state, now) {
                        for line in &follow {
                            info!(target: TARGET, "{line}");
                        }
                        state.save(&dir);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = hourly.tick() => {
                let Some(node) = node.upgrade() else { break };
                let now = crate::node::now_secs();
                if let Some(since) = state.summary_due(now) {
                    let authors: Vec<String> = node.local_authors().into_iter().collect();
                    match node.store().signals_received(&authors, since) {
                        Ok(received) => {
                            if let Some(line) = summary_line(&received) {
                                info!(target: TARGET, "{line}");
                            }
                        }
                        Err(e) => tracing::debug!("daily summary: {e}"),
                    }
                    state.save(&dir);
                }
            }
        }
    }
}

/// Colours of the brief view.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    pub color: bool,
}

impl Paint {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// `2026-10-24T19:38:46.191234Z` → its unix seconds.
fn parse_ts(ts: &str) -> Option<i64> {
    let (date, time) = ts.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, s) = (t.next()??, t.next()??, t.next()??);
    // Howard Hinnant's days-from-civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

/// `hh:mm:ss` of a log line's timestamp, in local time when known.
fn clock(ts: &str) -> String {
    let Some(secs) = parse_ts(ts) else {
        return ts.chars().take(8).collect();
    };
    let local = secs + wats_sys::utc_offset(secs).unwrap_or(0);
    let day = local.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, day / 60 % 60, day % 60)
}

/// How `wats attach` shows one line of `node.log` in the brief view: a
/// brief line or a warning, without spans and module paths; `None` for
/// any other line.
pub fn render(line: &str, paint: Paint) -> Option<String> {
    let line = line.trim_end();
    let mut parts = line.splitn(2, char::is_whitespace);
    let ts = parts.next()?;
    let rest = parts.next()?.trim_start();
    let (level, rest) = rest.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    // Spans, then the target, each ending with ": ".
    let message_of = |target_start: usize| -> Option<(&str, &str)> {
        let after = &rest[target_start..];
        let (target, message) = after.split_once(": ")?;
        Some((target, message))
    };
    let start = rest.match_indices("}: ").last().map_or(0, |(i, _)| i + 3);
    let (target, message) = message_of(start)?;
    let time = paint.paint("2", &clock(ts));
    match level {
        "INFO" if target == TARGET => {
            let colour = if message.starts_with("ask from ") {
                "36"
            } else if message.starts_with('⚠') {
                "33"
            } else if message.starts_with("back online") {
                "32"
            } else if message.contains(" failed") || message.contains(" declined ") {
                "31"
            } else if message.contains(" answered: ")
                || message.contains(" claimed: ")
                || message.contains(" run ok")
                || message.contains(" your ")
            {
                "32"
            } else {
                "0"
            };
            Some(format!("{time}  {}", paint.paint(colour, message)))
        }
        // Connectivity and health have their own brief lines, quiet
        // across a sleep (ADR 0052).
        "WARN" | "ERROR"
            if !target.starts_with("iroh")
                && target != "wats::reconnect"
                && target != "wats::health"
                && target != "wats::node::links" =>
        {
            let colour = if level == "ERROR" { "31" } else { "33" };
            Some(format!(
                "{time}  {}",
                paint.paint(colour, &format!("⚠ {message}"))
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(id: &str, ts: u64) -> Event {
        Event {
            v: 1,
            id: id.into(),
            author: "me".into(),
            ts,
            net: "n".into(),
            thread: Some("ask".into()),
            parent: Some("ask".into()),
            refs: Vec::new(),
            kind: Kind::Answer,
            tags: Vec::new(),
            title: Some("Use VACUUM".into()),
            text: String::new(),
            data: serde_json::Map::new(),
            blobs: Vec::new(),
            pow: String::new(),
            sig: String::new(),
            grease: serde_json::Map::new(),
        }
    }

    /// 0.5.3: a status change is said once, and survives a restart; a post
    /// older than the state is recorded without a line.
    #[test]
    fn a_status_change_is_said_once_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(dir.path(), 1000);
        let confirmed = Signals {
            status: Status::Confirmed,
            accepted: true,
            accepts: 1,
            ..Signals::default()
        };
        let post = answer("a1", 2000);
        assert_eq!(
            state.status_line(&post, &confirmed, 2100).as_deref(),
            Some("your answer \"Use VACUUM\" is now confirmed: accepted by its asker")
        );
        assert_eq!(state.status_line(&post, &confirmed, 2200), None);
        state.save(dir.path());
        let mut again = State::load(dir.path(), 5000);
        assert_eq!(again.status_line(&post, &confirmed, 5100), None);
        let contested = Signals {
            status: Status::Contested,
            why: Some("contradicted by 1 key: tests fail".into()),
            ..Signals::default()
        };
        assert_eq!(
            again.status_line(&post, &contested, 5200).as_deref(),
            Some("your answer \"Use VACUUM\" is now contested: contradicted by 1 key: tests fail")
        );
        // Older than the state's first start: recorded, not said.
        let old = answer("a0", 500);
        assert_eq!(again.status_line(&old, &confirmed, 5300), None);
    }

    /// 0.5.3: a field tag earned is said once; an author first seen on
    /// an old answer is recorded silently, on a new one it is said.
    #[test]
    fn an_earned_field_is_said_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(dir.path(), 1000);
        let pg = Earned {
            tag: "topic:postgres".into(),
            accepted: 1,
            ..Earned::default()
        };
        assert_eq!(
            state.earned_lines("seed-postgres", "k1", std::slice::from_ref(&pg), 2000),
            ["seed-postgres earned topic:postgres"]
        );
        assert!(
            state
                .earned_lines("seed-postgres", "k1", std::slice::from_ref(&pg), 2000)
                .is_empty()
        );
        assert!(
            state
                .earned_lines("old", "k2", std::slice::from_ref(&pg), 10)
                .is_empty()
        );
        assert!(
            state
                .earned_lines("old", "k2", std::slice::from_ref(&pg), 3000)
                .is_empty()
        );
    }

    /// 0.5.3: the summary is due a day after the last one, never at the
    /// first start.
    #[test]
    fn the_summary_comes_once_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::load(dir.path(), 0);
        assert_eq!(state.summary_due(100), None);
        assert_eq!(state.summary_due(100 + 3600), None);
        assert_eq!(state.summary_due(100 + SUMMARY_EVERY), Some(100));
        assert_eq!(state.summary_due(200 + SUMMARY_EVERY), None);
        let received = crate::store::Received {
            useful: 3,
            used: 2,
            confirmed: 1,
            challenged: 1,
            ..Default::default()
        };
        assert_eq!(
            summary_line(&received).as_deref(),
            Some(
                "today: your posts got 3 useful (+1), 2 used, 1 confirmed, 1 challenged (−1 with a reason)"
            )
        );
        assert_eq!(summary_line(&crate::store::Received::default()), None);
        assert_eq!(
            memory_summary(&serde_json::json!({"posts": 12, "received": received})),
            "12 posts · your posts got 3 useful (+1), 2 used, 1 confirmed, 1 challenged (−1 with a reason)"
        );
    }

    #[test]
    fn files_say_what_came_with_an_answer() {
        assert_eq!(files_line(&[]), "");
        assert_eq!(files_line(&["main.rs"]), " · 1 file attached (main.rs)");
        assert_eq!(
            files_line(&["a.txt", "b.rs", "c.log", "d.png"]),
            " · 4 files attached (a.txt, b.rs, c.log, …)"
        );
    }

    #[test]
    fn files_say_what_came_with_an_answer() {
        assert_eq!(files_line(&[]), "");
        assert_eq!(files_line(&["main.rs"]), " · 1 file attached (main.rs)");
        assert_eq!(
            files_line(&["a.txt", "b.rs", "c.log", "d.png"]),
            " · 4 files attached (a.txt, b.rs, c.log, …)"
        );
    }

    #[test]
    fn brief_lines_and_warnings_only() {
        let plain = Paint { color: false };
        let brief = "2026-10-24T19:38:46.191234Z  INFO agent{name=openclaw}: brief: alice-agent \
                     answered: Rust 1.99.1";
        let shown = render(brief, plain).unwrap();
        assert!(
            shown.ends_with("  alice-agent answered: Rust 1.99.1"),
            "{shown}"
        );
        assert_eq!(
            shown.len(),
            "hh:mm:ss  alice-agent answered: Rust 1.99.1".len()
        );
        let warn = "2026-10-24T19:38:46.191234Z  WARN agent{name=openclaw}: wats::agent: agent \
                    run failed: No API key";
        assert!(
            render(warn, plain)
                .unwrap()
                .ends_with("⚠ agent run failed: No API key")
        );
        let iroh = "2026-10-24T19:38:46.191234Z DEBUG iroh::_events::path::selected: \
                    remote=ab network_path=Ip(1.2.3.4)";
        assert_eq!(render(iroh, plain), None);
        let offline = "2026-10-26T02:37:00.1Z  WARN wats::reconnect: connectivity: offline, no \
                       connected peer; reconnecting (next attempt in 1s)";
        assert_eq!(render(offline, plain), None);
        let health = "2026-10-26T02:37:00.1Z  WARN wats::health: infra health degraded: no relay";
        assert_eq!(render(health, plain), None);
        let links = "2026-10-26T22:36:00.1Z  WARN wats::node::links: gossip neighbor still silent \
                     after a fresh link: it likely does not count this node as its neighbor";
        assert_eq!(render(links, plain), None);
        let back = "2026-10-26T08:23:00.1Z  INFO brief: back online after sleep (6h02m)";
        assert!(
            render(back, plain)
                .unwrap()
                .ends_with("  back online after sleep (6h02m)")
        );
        let other = "2026-10-24T19:38:46.191234Z  INFO wats::node: node started id=ab";
        assert_eq!(render(other, plain), None);
        assert_eq!(render("not a log line", plain), None);
        assert_eq!(parse_ts("1970-01-02T00:00:01.5Z"), Some(86_401));
    }
}
