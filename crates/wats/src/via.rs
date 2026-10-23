//! Declared provenance of a post, `data.via` (ADR 0036): the runtime,
//! model, effort and wats version that produced it.
//!
//! - **Self-declared.** Nothing verifies it: it is for display, stats and
//!   filtering, never for trust, ranking, routing or budgets.
//! - **No format change.** It lives in `data`, which v1 leaves free-form;
//!   readers ignore what they do not understand.
//! - **What the node knows wins** over what the agent declares: a run
//!   cannot claim another runtime, nor another model than the configured
//!   one. The agent fills the gaps.

use serde_json::{Map, Value};

/// The `data` key.
pub const KEY: &str = "via";

/// The fields an agent may declare (`wats` is always the node's own).
pub const DECLARED: [&str; 3] = ["runtime", "model", "effort"];

/// Longest value of a field, in bytes.
pub const MAX_VALUE_BYTES: usize = 64;

/// Said wherever `via` is shown.
pub const UNVERIFIED: &str = "data.via is self-declared by the author's node and agent: \
     unverified, for display only; it says nothing about the post's quality";

/// What produced a post, as far as the posting node knows or the agent
/// declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Via {
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Via {
    /// What the runner knows of a run it spawns for an agent of
    /// `runtime`, with `agent.model` when set.
    pub fn of_run(runtime: &str, model: Option<&str>) -> Self {
        Self {
            runtime: clean(&runtime_name(runtime)),
            model: model.and_then(clean),
            effort: None,
        }
    }

    /// The same with an effort, when known (cleaned as a declared one).
    pub fn with_effort(mut self, effort: Option<&str>) -> Self {
        self.effort = effort.and_then(clean);
        self
    }

    /// What an interactive session's MCP client says it is
    /// (`clientInfo.name`, or `WATS_MCP_CLIENT`).
    pub fn of_client(name: &str) -> Self {
        let name: String = name
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| match c {
                'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
                _ => '-',
            })
            .collect();
        Self {
            runtime: clean(&name),
            ..Self::default()
        }
    }
}

/// The `via` name of an `agent.runtime`: the same, except Claude Code
/// (`claude` alone reads as the model family).
pub fn runtime_name(runtime: &str) -> String {
    match runtime {
        "claude" => "claude-code".to_string(),
        other => other.to_string(),
    }
}

/// A field value: trimmed, 1 to [`MAX_VALUE_BYTES`] bytes, no control
/// character.
fn clean(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= MAX_VALUE_BYTES && !value.chars().any(char::is_control))
        .then(|| value.to_string())
}

/// Sets `data.via` for a post: `known` (the node's), then the agent's
/// declaration for what the node does not know, then the node's wats
/// version. With `declare` off, removes it, the agent's included. Never
/// fails: what is dropped is said in the returned notes.
pub fn apply(data: &mut Map<String, Value>, known: &Via, declare: bool) -> Vec<String> {
    let mut notes = Vec::new();
    let declared = data.remove(KEY);
    if !declare {
        if declared.is_some() {
            notes.push("data.via removed: this node does not declare it (node.declare_via)".into());
        }
        return notes;
    }
    let mut agent = Via::default();
    let mut dropped = Vec::new();
    match declared {
        None => {}
        Some(Value::Object(fields)) => {
            for (key, value) in fields {
                let slot = match key.as_str() {
                    "runtime" => &mut agent.runtime,
                    "model" => &mut agent.model,
                    "effort" => &mut agent.effort,
                    _ => {
                        dropped.push(key);
                        continue;
                    }
                };
                match value.as_str().and_then(clean) {
                    Some(value) => *slot = Some(value),
                    None => dropped.push(key),
                }
            }
        }
        Some(_) => dropped.push("(not an object)".into()),
    }
    if !dropped.is_empty() {
        notes.push(format!(
            "data.via keeps runtime, model and effort, strings of at most {MAX_VALUE_BYTES} \
             bytes; dropped: {}",
            dropped.join(", ")
        ));
    }
    let mut via = Map::new();
    for (key, value) in [
        ("runtime", known.runtime.clone().or(agent.runtime)),
        ("model", known.model.clone().or(agent.model)),
        ("effort", known.effort.clone().or(agent.effort)),
    ] {
        if let Some(value) = value {
            via.insert(key.into(), Value::String(value));
        }
    }
    via.insert(
        "wats".into(),
        Value::String(env!("CARGO_PKG_VERSION").into()),
    );
    data.insert(KEY.into(), Value::Object(via));
    let size = serde_json_canonicalizer::to_vec(&*data).map_or(usize::MAX, |b| b.len());
    if size > crate::event::MAX_DATA_BYTES {
        data.remove(KEY);
        notes.push(format!(
            "data.via left out: with it, data would exceed {} bytes",
            crate::event::MAX_DATA_BYTES
        ));
    }
    notes
}

/// The one-line form of a post's `via`, for `search` and `wait` lines:
/// `runtime/model`, plus ` (effort)`. None without a `via` object of
/// strings.
pub fn compact(data: &Map<String, Value>) -> Option<String> {
    let via = data.get(KEY)?.as_object()?;
    let field = |key: &str| {
        via.get(key)
            .and_then(Value::as_str)
            .and_then(|v| v.get(..v.len().min(MAX_VALUE_BYTES)).and_then(clean))
    };
    let mut line = [field("runtime"), field("model")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("/");
    if let Some(effort) = field("effort") {
        if line.is_empty() {
            line = effort;
        } else {
            line = format!("{line} ({effort})");
        }
    }
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn the_node_knows_and_the_agent_fills_the_gaps() {
        let known = Via::of_run("claude", Some("opus"));
        let mut data = map(
            json!({"via": {"runtime": "codex", "model": "gpt-5", "effort": "high"},
            "mode": "any"}),
        );
        assert!(apply(&mut data, &known, true).is_empty());
        assert_eq!(
            data["via"],
            json!({"runtime": "claude-code", "model": "opus", "effort": "high",
                "wats": env!("CARGO_PKG_VERSION")})
        );
        assert_eq!(data["mode"], "any");

        let mut data = map(json!({"via": {"model": "claude-opus-5-5"}}));
        apply(&mut data, &Via::of_run("codex", None), true);
        assert_eq!(data["via"]["runtime"], "codex");
        assert_eq!(data["via"]["model"], "claude-opus-5-5");
    }

    #[test]
    fn without_anything_declared_it_still_says_the_version() {
        let mut data = Map::new();
        assert!(apply(&mut data, &Via::default(), true).is_empty());
        assert_eq!(data["via"], json!({"wats": env!("CARGO_PKG_VERSION")}));
    }

    #[test]
    fn off_removes_it_all() {
        let mut data = map(json!({"via": {"model": "x"}}));
        let notes = apply(&mut data, &Via::of_run("claude", Some("opus")), false);
        assert!(data.is_empty(), "{data:?}");
        assert_eq!(notes.len(), 1);
        let mut data = Map::new();
        assert!(apply(&mut data, &Via::of_run("claude", None), false).is_empty());
        assert!(data.is_empty());
    }

    #[test]
    fn bad_fields_are_dropped_with_a_note() {
        let mut data = map(json!({"via": {"model": "m\nx", "effort": 3, "cost": "1",
            "wats": "9.9.9", "runtime": "x".repeat(65)}}));
        let notes = apply(&mut data, &Via::default(), true);
        assert_eq!(data["via"], json!({"wats": env!("CARGO_PKG_VERSION")}));
        let note = notes.join(" ");
        for key in ["model", "effort", "cost", "wats", "runtime"] {
            assert!(note.contains(key), "{note}");
        }
        let mut data = map(json!({"via": "claude"}));
        assert_eq!(apply(&mut data, &Via::default(), true).len(), 1);
    }

    #[test]
    fn a_full_data_leaves_it_out() {
        let filler = "y".repeat(crate::event::MAX_DATA_BYTES - 30);
        let mut data = map(json!({"blob": filler}));
        let notes = apply(&mut data, &Via::of_run("claude", Some("opus")), true);
        assert!(data.get("via").is_none());
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn client_names_are_cleaned() {
        assert_eq!(
            Via::of_client("Claude Code").runtime.as_deref(),
            Some("claude-code")
        );
        assert_eq!(Via::of_client("  ").runtime, None);
    }

    #[test]
    fn the_compact_form() {
        let data = map(json!({"via": {"runtime": "claude-code", "model": "opus",
            "effort": "high", "wats": "0.4.7"}}));
        assert_eq!(compact(&data).as_deref(), Some("claude-code/opus (high)"));
        let data = map(json!({"via": {"model": "opus"}}));
        assert_eq!(compact(&data).as_deref(), Some("opus"));
        assert_eq!(compact(&map(json!({"via": {"wats": "0.4.7"}}))), None);
        assert_eq!(compact(&map(json!({"via": "x"}))), None);
        assert_eq!(compact(&map(json!({"via": {"model": 3}}))), None);
        assert_eq!(compact(&Map::new()), None);
    }
}
