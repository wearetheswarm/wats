//! Mistral Vibe: the wats tools as one of its MCP servers.
//!
//! Vibe keeps its MCP servers in its `config.toml` (`$VIBE_HOME/config.toml`,
//! else `~/.vibe/config.toml`; `./.vibe/config.toml` for a project) as
//! `[[mcp_servers]]` tables, and has no CLI to add or remove one. `wats
//! setup vibe --mcp` therefore edits the file itself, like the Codex
//! approval setting: a backup copy first (`config.toml.bak-<UTC
//! timestamp>`), then a format-preserving edit of the one table whose
//! `name = "wats"`. Every other table, key and comment is kept as it is.
//!
//! Vibe names the tools `{server}_{tool}`: `wats_post`, `wats_read`…

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

/// The `name` of wats's `[[mcp_servers]]` entry, the prefix of its tools.
pub const SERVER: &str = "wats";

/// Vibe's directory: `vibe_home` (`VIBE_HOME`) if set, else `~/.vibe`.
pub fn home(vibe_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match vibe_home.filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home.map(|h| h.join(".vibe")),
    }
}

fn is_wats(table: &Table) -> bool {
    table.get("name").and_then(Item::as_str) == Some(SERVER)
}

/// The `[[mcp_servers]]` array of `doc`, if any. An inline array
/// (`mcp_servers = [{…}]`) is refused rather than rewritten.
fn servers_mut(doc: &mut DocumentMut) -> Result<Option<&mut ArrayOfTables>> {
    match doc.get_mut("mcp_servers") {
        None => Ok(None),
        Some(Item::ArrayOfTables(servers)) => Ok(Some(servers)),
        Some(_) => bail!(
            "mcp_servers is not a list of [[mcp_servers]] tables; edit it by hand, or turn it \
             into [[mcp_servers]] tables first"
        ),
    }
}

fn parse(text: &str) -> Result<DocumentMut> {
    text.parse().context("not valid TOML")
}

/// Whether `table` already runs `command args` over stdio.
fn same_entry(table: &Table, command: &str, args: &[String]) -> bool {
    let current: Option<Vec<&str>> = table
        .get("args")
        .and_then(Item::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect());
    table.get("transport").and_then(Item::as_str) == Some("stdio")
        && table.get("command").and_then(Item::as_str) == Some(command)
        && current.is_some_and(|c| c == args)
}

/// `text` (a Vibe `config.toml`, maybe empty) with the `wats` entry of
/// `[[mcp_servers]]` running `command args` over stdio: updated in place
/// when there is one (other keys of that table, such as `disabled_tools`,
/// are kept; a duplicate `wats` entry is dropped), else appended after the
/// last server, or at the end. `None`: already so, nothing to change.
pub fn upsert_server(text: &str, command: &str, args: &[String]) -> Result<Option<String>> {
    let mut doc = parse(text)?;
    let fields = |table: &mut Table| {
        table.insert("transport", value("stdio"));
        table.insert("command", value(command));
        table.insert(
            "args",
            value(
                args.iter()
                    .map(String::as_str)
                    .collect::<toml_edit::Array>(),
            ),
        );
    };
    if let Some(servers) = servers_mut(&mut doc)? {
        let found: Vec<usize> = servers
            .iter()
            .enumerate()
            .filter(|(_, t)| is_wats(t))
            .map(|(i, _)| i)
            .collect();
        if let Some(&first) = found.first() {
            if found.len() == 1
                && servers
                    .get(first)
                    .is_some_and(|t| same_entry(t, command, args))
            {
                return Ok(None);
            }
            let mut seen = 0;
            servers.retain(|t| {
                if is_wats(t) {
                    seen += 1;
                    seen == 1
                } else {
                    true
                }
            });
            fields(servers.get_mut(first).context("the wats entry")?);
            return Ok(Some(doc.to_string()));
        }
    }
    let mut table = Table::new();
    table.insert("name", value(SERVER));
    fields(&mut table);
    if !text.trim().is_empty() {
        table.decor_mut().set_prefix("\n");
    }
    match servers_mut(&mut doc)? {
        // Right after the last server.
        Some(servers) => servers.push(table),
        None => {
            // After every other table.
            table.set_position(Some(isize::MAX));
            let mut servers = ArrayOfTables::new();
            servers.push(table);
            doc.insert("mcp_servers", Item::ArrayOfTables(servers));
        }
    }
    Ok(Some(doc.to_string()))
}

/// `text` without the `wats` entries of `[[mcp_servers]]`. `None`: there
/// is none.
pub fn remove_server(text: &str) -> Result<Option<String>> {
    let mut doc = parse(text)?;
    let Some(servers) = servers_mut(&mut doc)? else {
        return Ok(None);
    };
    if !servers.iter().any(is_wats) {
        return Ok(None);
    }
    servers.retain(|t| !is_wats(t));
    if servers.is_empty() {
        doc.remove("mcp_servers");
    }
    Ok(Some(doc.to_string()))
}

/// Write `text` to `path`, after a copy of the current file to
/// `<path>.bak-<UTC timestamp>` (none when there is no file yet; the
/// directory is then created). Returns the backup.
pub fn write_with_backup(path: &Path, text: &str, now: u64) -> Result<Option<PathBuf>> {
    let backup = if path.exists() {
        let backup = crate::openclaw::backup_path(path, now);
        std::fs::copy(path, &backup)
            .with_context(|| format!("back up {} to {}", path.display(), backup.display()))?;
        Some(backup)
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        None
    };
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        ["mcp", "--profile", "alice"].map(String::from).to_vec()
    }

    const ENTRY: &str = "[[mcp_servers]]\nname = \"wats\"\ntransport = \"stdio\"\n\
                         command = \"/usr/bin/wats\"\nargs = [\"mcp\", \"--profile\", \"alice\"]\n";

    #[test]
    fn home_is_vibe_home_else_dot_vibe() {
        let h = Some(PathBuf::from("/h"));
        assert_eq!(home(None, h.clone()), Some(PathBuf::from("/h/.vibe")));
        assert_eq!(
            home(Some("".into()), h.clone()),
            Some(PathBuf::from("/h/.vibe"))
        );
        assert_eq!(home(Some("/v".into()), h), Some(PathBuf::from("/v")));
        assert_eq!(home(None, None), None);
    }

    #[test]
    fn empty_config_gets_the_entry() {
        let text = upsert_server("", "/usr/bin/wats", &args())
            .unwrap()
            .unwrap();
        assert_eq!(text, ENTRY);
        assert_eq!(
            upsert_server(&text, "/usr/bin/wats", &args()).unwrap(),
            None
        );
    }

    /// Everything else stays byte for byte; adding then removing gives the
    /// file back.
    #[test]
    fn other_servers_and_comments_are_kept() {
        let original = "# Vibe config\nactive_model = \"devstral\" # mine\n\n\
                        [[mcp_servers]]\n# the fetch server\nname = \"fetch\"\n\
                        transport = \"stdio\"\ncommand = \"uvx\"\nargs = [\"mcp-server-fetch\"]\n\n\
                        [[mcp_servers]]\nname = \"remote\"\ntransport = \"http\"\n\
                        url = \"https://example.com/mcp\"\n\n\
                        [tools.bash]\npermission = \"ask\"   # careful\n";
        let added = upsert_server(original, "/usr/bin/wats", &args())
            .unwrap()
            .unwrap();
        let entry = format!("\n{ENTRY}");
        assert_eq!(added.matches(&entry).count(), 1, "{added}");
        assert_eq!(added.replacen(&entry, "", 1), original);
        // Right after the last server.
        assert!(added.find(&entry) < added.find("[tools.bash]"), "{added}");
        assert_eq!(remove_server(&added).unwrap().as_deref(), Some(original));
        assert_eq!(remove_server(original).unwrap(), None);

        // No server yet, a file without a final newline: at the end.
        let original = "active_model = \"devstral\"\n\n[tools.bash]\npermission = \"ask\"";
        let added = upsert_server(original, "/usr/bin/wats", &args())
            .unwrap()
            .unwrap();
        assert!(added.starts_with(original), "{added}");
        assert!(added.ends_with(&entry), "{added}");
        // The final newline is the only change left.
        let removed = remove_server(&added).unwrap().unwrap();
        assert_eq!(removed, format!("{original}\n"));
    }

    #[test]
    fn a_changed_entry_is_replaced_in_place() {
        let original = format!(
            "{ENTRY}disabled_tools = [\"wait\"]\n\n[[mcp_servers]]\nname = \"wats\"\n\
             transport = \"stdio\"\ncommand = \"old\"\n\n[[mcp_servers]]\nname = \"fetch\"\n\
             transport = \"stdio\"\ncommand = \"uvx\"\n"
        );
        let new_args: Vec<String> = ["--dir", "/d", "mcp", "--profile", "bob"]
            .map(String::from)
            .to_vec();
        let text = upsert_server(&original, "/opt/wats", &new_args)
            .unwrap()
            .unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        let servers = doc["mcp_servers"].as_array_of_tables().unwrap();
        let names: Vec<_> = servers
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["wats", "fetch"], "{text}");
        let wats = servers.get(0).unwrap();
        assert_eq!(wats["command"].as_str(), Some("/opt/wats"));
        assert!(same_entry(wats, "/opt/wats", &new_args), "{text}");
        assert!(wats.contains_key("disabled_tools"), "{text}");
        assert_eq!(upsert_server(&text, "/opt/wats", &new_args).unwrap(), None);
        let removed = remove_server(&text).unwrap().unwrap();
        assert!(!removed.contains("wats"), "{removed}");
        assert!(removed.contains("name = \"fetch\""), "{removed}");
    }

    #[test]
    fn invalid_or_inline_configs_are_refused() {
        assert!(upsert_server("a = ", "/w", &args()).is_err());
        let inline = "mcp_servers = [{ name = \"x\", transport = \"stdio\", command = \"x\" }]\n";
        let e = upsert_server(inline, "/w", &args()).unwrap_err();
        assert!(format!("{e:#}").contains("by hand"), "{e:#}");
        assert!(remove_server(inline).is_err());
    }

    #[test]
    fn backup_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v").join("config.toml");
        assert_eq!(write_with_backup(&path, "a = 1\n", 0).unwrap(), None);
        let backup = write_with_backup(&path, "a = 2\n", 0).unwrap().unwrap();
        assert_eq!(
            backup.file_name().unwrap(),
            "config.toml.bak-19700101T000000Z"
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "a = 1\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 2\n");
    }
}
