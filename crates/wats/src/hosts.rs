//! MCP hosts whose servers live under `mcpServers` in a JSON file that
//! wats edits itself, after a backup (ADR 0038): GitHub Copilot CLI and
//! Antigravity CLI.
//!
//! The file is parsed and written back whole: its other keys are kept,
//! re-sorted, and the backup holds it as it was.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// The server name wats registers as.
pub const SERVER: &str = "wats";

/// One of the JSON hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Copilot,
    Antigravity,
}

impl Host {
    /// The name `wats setup` takes.
    pub fn tool(self) -> &'static str {
        match self {
            Host::Copilot => "copilot",
            Host::Antigravity => "antigravity",
        }
    }

    /// Its name for people.
    pub fn label(self) -> &'static str {
        match self {
            Host::Copilot => "GitHub Copilot CLI",
            Host::Antigravity => "Antigravity CLI",
        }
    }

    /// The command found on PATH.
    pub fn command(self) -> &'static str {
        match self {
            Host::Copilot => "copilot",
            Host::Antigravity => "agy",
        }
    }

    /// The host of a command found on PATH.
    pub fn of_command(command: &str) -> Option<Host> {
        [Host::Copilot, Host::Antigravity]
            .into_iter()
            .find(|h| h.command() == command)
    }

    /// Its user file of MCP servers: `$COPILOT_HOME/mcp-config.json`
    /// (default `~/.copilot`), `~/.gemini/config/mcp_config.json`.
    pub fn path(self, home: Option<&Path>, copilot_home: Option<PathBuf>) -> Option<PathBuf> {
        match self {
            Host::Copilot => copilot_home
                .filter(|d| !d.as_os_str().is_empty())
                .or_else(|| home.map(|h| h.join(".copilot")))
                .map(|d| d.join("mcp-config.json")),
            Host::Antigravity => home.map(|h| h.join(".gemini/config/mcp_config.json")),
        }
    }

    /// This user's file, from the environment.
    pub fn user_path(self) -> Option<PathBuf> {
        self.path(
            std::env::home_dir().as_deref(),
            std::env::var_os("COPILOT_HOME").map(PathBuf::from),
        )
    }

    /// The `wats` entry: a local (stdio) server.
    pub fn entry(self, command: &str, args: &[String]) -> Value {
        match self {
            Host::Copilot => json!({
                "type": "local",
                "command": command,
                "args": args,
                "env": {},
                "tools": ["*"],
            }),
            Host::Antigravity => json!({"command": command, "args": args}),
        }
    }
}

/// The file's JSON object (an empty or missing file is an empty one).
fn parse(text: &str) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text).context("not valid JSON")? {
        Value::Object(map) => Ok(map),
        _ => bail!("not a JSON object"),
    }
}

fn render(map: &Map<String, Value>) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(map)?))
}

/// `text` with `entry` as `mcpServers.wats`, or `None` when it is already
/// there as such.
pub fn upsert_server(text: &str, entry: &Value) -> Result<Option<String>> {
    let mut map = parse(text)?;
    let servers = map
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        bail!("mcpServers is not an object");
    };
    if servers.get(SERVER) == Some(entry) {
        return Ok(None);
    }
    servers.insert(SERVER.into(), entry.clone());
    render(&map).map(Some)
}

/// `text` without `mcpServers.wats`, or `None` when there is none.
pub fn remove_server(text: &str) -> Result<Option<String>> {
    let mut map = parse(text)?;
    let Some(servers) = map.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    if servers.remove(SERVER).is_none() {
        return Ok(None);
    }
    render(&map).map(Some)
}

/// Whether `text` has a `wats` server.
pub fn has_server(text: &str) -> bool {
    parse(text).is_ok_and(|m| m.get("mcpServers").and_then(|s| s.get(SERVER)).is_some())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn entries_go_in_and_out_keeping_the_rest() {
        let args = vec!["mcp".to_string(), "--profile".into(), "alice".into()];
        let entry = Host::Copilot.entry("/bin/wats", &args);
        assert_eq!(entry["type"], "local");
        assert_eq!(entry["tools"], json!(["*"]));
        let text = r#"{"theme": "dark", "mcpServers": {"gh": {"command": "gh"}}}"#;
        let added = upsert_server(text, &entry).unwrap().unwrap();
        let v: Value = serde_json::from_str(&added).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["gh"]["command"], "gh");
        assert_eq!(v["mcpServers"]["wats"]["args"][2], "alice");
        assert!(has_server(&added));
        assert_eq!(upsert_server(&added, &entry).unwrap(), None);
        let removed = remove_server(&added).unwrap().unwrap();
        assert!(!has_server(&removed));
        assert_eq!(remove_server(&removed).unwrap(), None);

        let fresh = upsert_server("", &Host::Antigravity.entry("w", &[]))
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&fresh).unwrap(),
            json!({"mcpServers": {"wats": {"command": "w", "args": []}}})
        );
        assert!(upsert_server("[1]", &entry).is_err());
        assert!(upsert_server("{not json", &entry).is_err());
        assert!(upsert_server(r#"{"mcpServers": 3}"#, &entry).is_err());
    }

    #[test]
    fn where_each_host_keeps_its_servers() {
        let home = Path::new("/h");
        assert_eq!(
            Host::Copilot.path(Some(home), None).unwrap(),
            Path::new("/h/.copilot/mcp-config.json")
        );
        assert_eq!(
            Host::Copilot
                .path(Some(home), Some(PathBuf::from("/c")))
                .unwrap(),
            Path::new("/c/mcp-config.json")
        );
        assert_eq!(
            Host::Antigravity.path(Some(home), None).unwrap(),
            Path::new("/h/.gemini/config/mcp_config.json")
        );
        assert_eq!(Host::of_command("agy"), Some(Host::Antigravity));
        assert_eq!(Host::of_command("claude"), None);
    }
}
