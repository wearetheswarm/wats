//! Brief lines (ADR 0051): one plain line per event a person watching the
//! node cares about, logged under the tracing target [`TARGET`], and how
//! `wats attach` shows them.
//!
//! - The node logs asks, answers to its posts, votes on them, and its
//!   agents' posts ([`watch`]); the runner logs its runs (`agent.rs`).
//! - `wats attach` on a terminal shows those lines and the warnings, as
//!   `hh:mm:ss  <line>` ([`render`]); `--full` shows the log as it is.

use serde_json::Value;
use tracing::info;

use crate::event::{Event, Kind};
use crate::node::Node;

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
    let announce = node
        .store()
        .latest_announces(&filter, 1)
        .ok()
        .and_then(|found| found.into_iter().next());
    let base = announce
        .as_ref()
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
        .or(local.clone())
        .unwrap_or_else(|| author.chars().take(8).collect());
    // Another node's agent, with the node it proves it runs on (ADR 0055
    // §3.2): `seed-postgres @ AmberHive#4f2a`.
    match announce {
        Some(a) if local.is_none() => {
            let grouping = crate::delivery::grouping(node, &a.event.net);
            match grouping.hive_label(&a.event.net, author) {
                Some(label) => format!("{base} @ {label}"),
                None => base,
            }
        }
        _ => base,
    }
}

/// What a person reads after an answer's author (ADR 0055 §2, §6): its
/// score, `(+12)`, and a warning when its node is flagged here.
fn standing(node: &Node, event: &Event) -> String {
    let grouping = crate::delivery::grouping(node, &event.net);
    let score = crate::scores::author_score(node.store(), &grouping, &event.net, &event.author);
    let mut out = format!(" ({})", crate::scores::shown(score));
    let flag = node.config().reputation.node_flag;
    if grouping.flagged(&event.net, &event.author, flag)
        && let Some(hive) = grouping.hive(&event.net, &event.author)
    {
        out.push_str(&format!(
            " · ⚠ a flagged node: {}; check it before you rely on it",
            crate::scores::flag_why(hive.contradicted)
        ));
    }
    out
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
            let grouping = crate::delivery::grouping(node, &event.net);
            let flagged = if !local && grouping.is_flagged(&event.net, &event.author) {
                let why = grouping
                    .hive(&event.net, &event.author)
                    .map(|h| crate::scores::flag_why(h.contradicted))
                    .unwrap_or_default();
                format!(" · ⚠ a flagged node ({why}): it wakes no agent here")
            } else {
                String::new()
            };
            Some(format!("ask from {who}: {}{tags}{flagged}", title(event)))
        }
        // This node's own vote while its key is new (ADR 0055 §1): other
        // nodes do not count it yet. Said, so that a new tester is not
        // puzzled.
        Kind::React
            if local
                && event.data.get("value").and_then(Value::as_i64).is_some()
                && crate::delivery::grouping(node, &event.net).young(
                    &event.net,
                    &event.author,
                    crate::node::now_secs(),
                ) =>
        {
            Some(format!(
                "{who} voted; on other nodes {}",
                crate::scores::NEW_KEY_NOTE
            ))
        }
        Kind::Answer if local && !parent_local => Some(format!("{who} answered: {}", title(event))),
        Kind::Answer if parent_local && !local => {
            let what = parent.as_ref().map_or("post", |p| p.kind.as_str());
            Some(format!(
                "answer from {who}{} to your {what}: {}{}",
                standing(node, event),
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
                    "voted +1 on"
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
                "voted +1 on"
            } else if data.get("value").and_then(Value::as_i64) == Some(-1) {
                "voted -1 on"
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

/// Log a brief line for each new live event of the node, until it is
/// gone.
pub async fn watch(node: std::sync::Weak<Node>) {
    let Some(mut events) = node.upgrade().map(|n| n.store().subscribe()) else {
        return;
    };
    loop {
        match events.recv().await {
            Ok(stored) => {
                let event = &stored.event;
                if crate::node::now_secs().saturating_sub(event.ts) > HISTORY_SECS {
                    continue;
                }
                let Some(node) = node.upgrade() else { break };
                if let Some(line) = line_for(&node, event) {
                    info!(target: TARGET, "{line}");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
