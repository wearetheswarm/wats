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
//!
//! It is also an agent runtime (`wats --agent vibe`, ADR 0033): the second
//! half of this module builds each run's command line, per-run config and
//! environment, and reads its run export.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::access::Access;
use crate::opencode::Version;

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

// ---------------------------------------------------------------------------
// Mistral Vibe as the node's agent (ADR 0033).
//
// Each run spawns, from the run directory:
//
//     vibe --prompt= --output=streaming --output-dir=<dir> --max-turns=<n>
//          [--max-price=<usd>] [--enabled-tools=wats_*] [extra args]   < prompt
//
// The prompt goes on stdin (an empty `--prompt` reads it there). At
// `swarm-only`, Vibe's home is the agent's own (`VIBE_HOME`), with a
// per-run `config.toml` that holds the wats MCP server only and enables
// its tools only; at `inherit`, the user's own Vibe runs, with the wats
// server and its tool approvals added through Vibe's `VIBE_*` environment
// layer. There is no `workspace` level: Vibe cannot keep its tools inside
// one directory.

/// The first Vibe whose headless mode wats was checked against (3.0.1's
/// source): the Rust CLI's run contract (`--output-dir` and its
/// `export.json`, the exit codes), MCP tools behind Vibe's permissions
/// (2.25.5) and `--enabled-tools` globs that cover them.
pub const MIN_VERSION: Version = Version(3, 0, 0);

/// The major version this integration was checked against. A later one
/// runs, with a warning.
pub const CHECKED_MAJOR: u64 = 3;

/// As OpenCode's (ADR 0031 open question 10): `inherit` needs
/// `agent.allow` and `agent.review`. `false` would only warn.
pub const INHERIT_NEEDS_GUARDRAILS: bool = true;

/// The USD budgets are enforced from the cost Vibe writes to its run
/// export (its estimate from the model's prices). `false` would count runs
/// only, as for Codex.
pub const ENFORCE_USD_BUDGET: bool = true;

/// The only tools of a `swarm-only` run: wats's (Vibe names MCP tools
/// `<server>_<tool>`).
pub const TOOL_PATTERN: &str = "wats_*";

/// The tools wats serves to a runner session (the review tools and
/// `start_daemon` are removed there). Each gets `permission = "always"`:
/// Vibe asks before an MCP tool by default, and a headless run denies
/// every question, so without it the agent could not call them.
pub const RUN_TOOLS: [&str; 10] = [
    "post", "reply", "digest", "retract", "read", "search", "browse", "who", "wait", "status",
];

/// Vibe's built-in tools (3.0.1), disabled at `swarm-only` on top of the
/// `wats_*` allow list: a second lock, in case one filter changed.
/// `search_replace` is `edit`'s old name.
pub const BUILTIN_TOOLS: [&str; 13] = [
    "bash",
    "powershell",
    "read_file",
    "write_file",
    "edit",
    "search_replace",
    "grep",
    "todo",
    "task",
    "skill",
    "web_search",
    "web_fetch",
    "ask_user_question",
];

/// Shown when `inherit` is chosen for Vibe (setup wizard, `wats setup
/// vibe --agent`).
pub const INHERIT_WARNING: &str =
    "Vibe approves its own tools: with inherit, a post can make it read or run anything you can";

/// Why `workspace` is refused for Vibe.
pub const WORKSPACE_REFUSED: &str = "agent.access = workspace is not available with Mistral \
     Vibe: Vibe cannot keep its tools inside one directory. Use swarm-only (the default: the \
     wats tools only) or inherit (your whole Vibe environment)";

/// Extra arguments refused at `swarm-only`: they would bring back Vibe's
/// own tools or approvals, another directory, or an old session.
pub const REFUSED_ARGS: [&str; 14] = [
    "--enabled-tools",
    "--agent",
    "--auto-approve",
    "--yolo",
    "--smart-approve",
    "--trust",
    "--add-dir",
    "--worktree",
    "--workdir",
    "--continue",
    "--resume",
    "--agent-socket",
    "--teleport",
    "--setup",
];

/// Extra arguments refused at every level: the runner sets them (the
/// prompt on stdin, the transcript, the run export).
pub const RUNNER_ARGS: [&str; 4] = ["--prompt", "--prompt-file", "--output", "--output-dir"];

/// The `VIBE_*` variables a `swarm-only` run keeps from wats's own
/// environment: how this install starts Vibe's app server, and the
/// colours. Every other one is removed, since Vibe reads `VIBE_<setting>`
/// as config, above its config files (`VIBE_ENABLED_TOOLS`,
/// `VIBE_MCP_SERVERS`, `VIBE_HOME`, `VIBE_CWD`…).
pub const KEPT_ENV: [&str; 3] = ["VIBE_APP_SERVER_BIN", "VIBE_APP_SERVER_CMD", "VIBE_THEME"];

/// The access level of a Vibe agent: `swarm-only` when unset, `workspace`
/// refused.
pub fn access(value: Option<&str>) -> Result<Access> {
    let Some(value) = value else {
        return Ok(Access::SwarmOnly);
    };
    let access: Access = value.trim().parse()?;
    if access == Access::Workspace {
        bail!("{WORKSPACE_REFUSED}");
    }
    Ok(access)
}

/// `inherit` hands swarm posts to the user's whole Vibe environment, whose
/// tools Vibe runs itself: refused without `agent.allow` and
/// `agent.review` ([`INHERIT_NEEDS_GUARDRAILS`]). `workspace` is refused.
pub fn check_access(access: Access, allow: &[String], review: bool) -> Result<()> {
    if access == Access::Workspace {
        bail!("{WORKSPACE_REFUSED}");
    }
    if INHERIT_NEEDS_GUARDRAILS && access == Access::Inherit && (allow.is_empty() || !review) {
        bail!(
            "agent.access = inherit with Mistral Vibe hands swarm posts to your own Vibe \
             environment ({INHERIT_WARNING}): set agent.allow (the authors who may wake it) \
             and agent.review = true first, or use swarm-only"
        );
    }
    Ok(())
}

/// The flag of an extra argument: `--flag` of `--flag=value`, `-x` of a
/// short option with its value attached (`-pTEXT`).
fn flag_of(arg: &str) -> &str {
    if arg.starts_with("--") {
        arg.split('=').next().unwrap_or(arg)
    } else if arg.starts_with('-') && arg.len() > 2 {
        &arg[..2]
    } else {
        arg
    }
}

/// Refuse extra arguments the runner sets ([`RUNNER_ARGS`], `-p`) and, at
/// `swarm-only`, those that would undo the level ([`REFUSED_ARGS`], `-c`).
pub fn check_extra_args(access: Access, extra: &[String]) -> Result<()> {
    for arg in extra {
        let flag = flag_of(arg);
        if RUNNER_ARGS.contains(&flag) || flag == "-p" {
            bail!(
                "agent.args: {flag} is set by the runner for a Mistral Vibe run (the prompt on \
                 stdin, the transcript, the run export): remove it"
            );
        }
        if access != Access::Inherit && (REFUSED_ARGS.contains(&flag) || flag == "-c") {
            bail!(
                "agent.args: {flag} would undo the {access} access level of a Mistral Vibe run \
                 (ADR 0033): remove it, or use access inherit"
            );
        }
    }
    Ok(())
}

/// Check `agent.model` for Vibe: a model alias (`active_model`), one word.
pub fn check_model(model: &str) -> Result<()> {
    if model.is_empty() || model.starts_with('-') || model.chars().any(char::is_whitespace) {
        bail!("agent.model {model:?}: Mistral Vibe takes a model alias, e.g. devstral-2");
    }
    Ok(())
}

/// The runner's check: refused below [`MIN_VERSION`] or without a
/// version; a warning above [`CHECKED_MAJOR`].
pub fn check_version(found: Option<Version>) -> Result<Option<String>> {
    let Some(v) = found else {
        bail!(
            "`vibe --version` gave no version: wats needs Mistral Vibe {MIN_VERSION} or later \
             to lock its agent down"
        );
    };
    if v < MIN_VERSION {
        bail!(
            "Mistral Vibe {v} is too old: wats needs {MIN_VERSION} or later (upgrade Vibe: \
             `vibe update`)"
        );
    }
    Ok((v.0 > CHECKED_MAJOR).then(|| {
        format!(
            "Mistral Vibe {v}: wats was checked against Vibe {CHECKED_MAJOR}.x only (ADR 0033); \
             its tool filters and run export may have changed"
        )
    }))
}

/// `vibe --version` (`vibe 3.0.1`), from the runner (at most 15 s).
pub async fn version(command: &Path) -> Option<Version> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(command)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    crate::opencode::parse_version(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| crate::opencode::parse_version(&String::from_utf8_lossy(&out.stderr)))
}

/// The agent's own Vibe home (`VIBE_HOME` of its `swarm-only` runs):
/// `<work dir>/vibe-home`. It holds the per-run `config.toml`, and what
/// Vibe writes for itself (caches, logs). wats never puts credentials
/// there.
pub fn agent_home(work_dir: &Path) -> PathBuf {
    work_dir.join("vibe-home")
}

/// Where a run's export goes (`--output-dir`): `<work dir>/vibe-run`,
/// emptied before each run and removed once the export is read.
pub fn export_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("vibe-run")
}

/// Inputs of one run's wats MCP server.
pub struct Server<'a> {
    /// The `wats mcp` command line of the run.
    pub wats_command: &'a Path,
    pub wats_args: &'a [String],
    pub runner_token: &'a str,
    /// The run's timeout: also each wats tool call's, so that a long
    /// `wait` is not cut at Vibe's default 60 s.
    pub timeout: std::time::Duration,
}

impl Server<'_> {
    fn tool_timeout(&self) -> u64 {
        self.timeout.as_secs().max(30)
    }

    /// The `[[mcp_servers]]` entry, as JSON (the same keys as in TOML).
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": SERVER,
            "transport": "stdio",
            "command": self.wats_command.to_string_lossy(),
            "args": self.wats_args,
            "env": {"WATS_RUNNER_TOKEN": self.runner_token},
            "startup_timeout_sec": 30,
            "tool_timeout_sec": self.tool_timeout(),
        })
    }
}

/// `{"wats_post": {"permission": "always"}, …}`: Vibe's per-tool settings
/// that let a headless run call the wats tools ([`RUN_TOOLS`]).
fn tool_permissions() -> serde_json::Map<String, serde_json::Value> {
    RUN_TOOLS
        .iter()
        .map(|tool| {
            (
                format!("{SERVER}_{tool}"),
                serde_json::json!({"permission": "always"}),
            )
        })
        .collect()
}

/// The per-run `config.toml` of a `swarm-only` run, in the agent's own
/// Vibe home: the wats tools only (allowed, and every built-in tool
/// disabled), no connectors, subagents or background processes, no
/// session log, no update check or telemetry, and the wats MCP server with
/// the run's runner token.
pub fn run_config(server: &Server) -> String {
    let strings = |items: &[&str]| -> toml_edit::Array { items.iter().copied().collect() };
    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix(
        "# This node's Mistral Vibe agent (wats, ADR 0033): written before every run, \
         do not edit.\n",
    );
    doc.insert("enabled_tools", value(strings(&[TOOL_PATTERN])));
    doc.insert("disabled_tools", value(strings(&BUILTIN_TOOLS)));
    for key in [
        "enable_connectors",
        "enable_subagents",
        "enable_background_processes",
        "enable_update_checks",
        "enable_auto_update",
        "enable_telemetry",
    ] {
        doc.insert(key, value(false));
    }
    let mut logging = Table::new();
    logging.insert("enabled", value(false));
    doc.insert("session_logging", Item::Table(logging));
    let mut tools = Table::new();
    tools.set_implicit(true);
    for (name, _) in tool_permissions() {
        let mut tool = Table::new();
        tool.insert("permission", value("always"));
        tools.insert(&name, Item::Table(tool));
    }
    doc.insert("tools", Item::Table(tools));
    let mut entry = Table::new();
    entry.insert("name", value(SERVER));
    entry.insert("transport", value("stdio"));
    entry.insert(
        "command",
        value(server.wats_command.to_string_lossy().as_ref()),
    );
    entry.insert(
        "args",
        value(
            server
                .wats_args
                .iter()
                .map(String::as_str)
                .collect::<toml_edit::Array>(),
        ),
    );
    entry.insert("startup_timeout_sec", value(30));
    entry.insert(
        "tool_timeout_sec",
        value(i64::try_from(server.tool_timeout()).unwrap_or(i64::MAX)),
    );
    let mut env = toml_edit::InlineTable::new();
    env.insert("WATS_RUNNER_TOKEN", server.runner_token.into());
    entry.insert("env", value(env));
    let mut servers = ArrayOfTables::new();
    servers.push(entry);
    doc.insert("mcp_servers", Item::ArrayOfTables(servers));
    doc.to_string()
}

/// Write the per-run config to `<home>/config.toml`, owner-only (it holds
/// the run's runner token), replacing the last run's.
pub fn write_run_config(home: &Path, text: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let path = home.join("config.toml");
    let tmp = home.join("config.toml.tmp");
    crate::background::write_file(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// The environment one run sets. Both levels: no session log (swarm runs
/// stay out of `vibe --resume`), no update check, the model if set. At
/// `swarm-only`: the agent's own Vibe home, and the harness's fallback
/// session store inside it. At `inherit`: the wats server and its tool
/// approvals as `VIBE_MCP_SERVERS` and `VIBE_TOOLS`, which Vibe merges
/// with the user's config (servers by name, so the run's `wats` entry
/// replaces a `wats` the user registered for the human's profile).
pub fn run_env(
    access: Access,
    work_dir: &Path,
    server: &Server,
    model: Option<&str>,
) -> Vec<(&'static str, OsString)> {
    let mut env: Vec<(&'static str, OsString)> = vec![
        ("VIBE_SESSION_LOGGING__ENABLED", "false".into()),
        ("VIBE_ENABLE_UPDATE_CHECKS", "false".into()),
        ("VIBE_ENABLE_AUTO_UPDATE", "false".into()),
    ];
    if let Some(model) = model {
        env.push(("VIBE_ACTIVE_MODEL", model.into()));
    }
    if access == Access::Inherit {
        env.push((
            "VIBE_MCP_SERVERS",
            serde_json::Value::Array(vec![server.json()])
                .to_string()
                .into(),
        ));
        env.push((
            "VIBE_TOOLS",
            serde_json::Value::Object(tool_permissions())
                .to_string()
                .into(),
        ));
    } else {
        let home = agent_home(work_dir);
        env.push((
            "MISTRAL_VIBE_SESSION_ROOT",
            home.join("logs").join("session").into_os_string(),
        ));
        env.push(("VIBE_HOME", home.into_os_string()));
    }
    env
}

/// The variables of `vars` a `swarm-only` run removes: every `VIBE_*` but
/// [`KEPT_ENV`], and `MISTRAL_VIBE_SESSION_ROOT`.
pub fn removed_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    vars.map(|(k, _)| k)
        .filter(|k| {
            let k = k.to_string_lossy();
            (k.starts_with("VIBE_") && !KEPT_ENV.contains(&k.as_ref()))
                || k == "MISTRAL_VIBE_SESSION_ROOT"
        })
        .collect()
}

/// Inputs of one run's command line.
pub struct Run<'a> {
    pub access: Access,
    /// `agent.max_turns`.
    pub max_turns: u32,
    /// What is left of the USD budget, if it applies: `--max-price`.
    pub max_price: Option<f64>,
    pub export_dir: &'a Path,
    pub extra: &'a [String],
}

/// The arguments of one run, before the extra arguments: headless with
/// the prompt on stdin, NDJSON on stdout, the run export, the limits
/// (unless the extra arguments set them), and at `swarm-only` the wats
/// tools only.
pub fn run_args(r: &Run) -> Vec<String> {
    let given = |flag: &str| r.extra.iter().any(|a| flag_of(a) == flag);
    let mut args = vec![
        "--prompt=".to_string(),
        "--output=streaming".to_string(),
        format!("--output-dir={}", r.export_dir.display()),
    ];
    if !given("--max-turns") {
        args.push(format!("--max-turns={}", r.max_turns.max(1)));
    }
    if let Some(price) = r.max_price.filter(|p| p.is_finite())
        && !given("--max-price")
    {
        args.push(format!("--max-price={:.2}", price.max(0.01)));
    }
    if r.access != Access::Inherit {
        args.push(format!("--enabled-tools={TOOL_PATTERN}"));
    }
    args
}

/// Empty the export directory before a run.
pub fn prepare_export_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("empty {}", dir.display())),
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))
}

/// The run's cost in its export: its own `cost_usd` plus its subagents'
/// (which the run's does not count). `None` without a positive cost.
pub fn export_cost(export: &serde_json::Value) -> Option<f64> {
    let own = export["cost_usd"].as_f64().unwrap_or(0.0);
    let subagents: f64 = export["subagents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["cost_usd"].as_f64())
        .sum();
    Some(own + subagents).filter(|c| *c > 0.0)
}

/// The transcript line of a run's export (`<dir>/export.json`): how it
/// ended, its LLM token usage and cost, its steps and Vibe's version, the
/// model; not the rest (the effective config). `None` without an export
/// (a killed run, an export Vibe could not write).
pub fn export_line(dir: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(dir.join("export.json")).ok()?;
    let export: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let model = export["config"]["active_model"]
        .as_str()
        .filter(|m| !m.is_empty());
    Some(serde_json::json!({
        "type": "vibe_export",
        "outcome": export["outcome"],
        "stop_reason": export["stop_reason"],
        "error": export["error"],
        "usage": export["usage"],
        "cost_usd": export_cost(&export),
        "steps": export["steps"],
        "exit_code": export["exit_code"],
        "vibe_version": export["vibe_version"],
        "model": model,
    }))
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

    // The runtime (ADR 0033).

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn server<'a>(args: &'a [String]) -> Server<'a> {
        Server {
            wats_command: Path::new("/opt/wats"),
            wats_args: args,
            runner_token: "tok-123",
            timeout: std::time::Duration::from_secs(600),
        }
    }

    /// Unset is swarm-only; `workspace` is refused; `inherit` and the old
    /// `locked` parse.
    #[test]
    fn levels_default_to_swarm_only_without_workspace() {
        assert_eq!(access(None).unwrap(), Access::SwarmOnly);
        assert_eq!(access(Some("locked")).unwrap(), Access::SwarmOnly);
        assert_eq!(access(Some("inherit")).unwrap(), Access::Inherit);
        let e = access(Some("workspace")).unwrap_err();
        assert!(format!("{e:#}").contains("cannot keep its tools"), "{e:#}");
        assert!(access(Some("everything")).is_err());
        assert_eq!(
            crate::access::level_for("vibe", None).unwrap(),
            Access::SwarmOnly
        );
        assert_eq!(
            crate::access::level_for("claude", None).unwrap(),
            Access::Workspace
        );
    }

    /// `inherit` needs `agent.allow` and `agent.review`, as for OpenCode.
    #[test]
    fn inherit_needs_allow_and_review() {
        let allow = s(&["ab"]);
        assert!(check_access(Access::SwarmOnly, &[], false).is_ok());
        assert!(check_access(Access::Workspace, &allow, true).is_err());
        assert!(check_access(Access::Inherit, &[], true).is_err());
        assert!(check_access(Access::Inherit, &allow, false).is_err());
        assert!(check_access(Access::Inherit, &allow, true).is_ok());
        let e = check_access(Access::Inherit, &[], false).unwrap_err();
        assert!(format!("{e:#}").contains(INHERIT_WARNING), "{e:#}");
    }

    #[test]
    fn arguments_that_undo_the_level_or_the_runner_are_refused() {
        for arg in [
            "--enabled-tools=bash",
            "--enabled-tools",
            "--agent",
            "--auto-approve",
            "--yolo",
            "--trust",
            "--add-dir=/",
            "--workdir",
            "-c",
            "--continue",
            "--resume=abc",
        ] {
            let e = check_extra_args(Access::SwarmOnly, &s(&[arg])).unwrap_err();
            assert!(format!("{e:#}").contains("would undo"), "{arg}: {e:#}");
            assert!(
                check_extra_args(Access::Inherit, &s(&[arg])).is_ok(),
                "{arg}"
            );
        }
        for arg in [
            "--prompt",
            "--prompt=hi",
            "-p",
            "-phi",
            "--prompt-file=/x",
            "--output=json",
            "--output-dir",
        ] {
            for level in [Access::SwarmOnly, Access::Inherit] {
                let e = check_extra_args(level, &s(&[arg])).unwrap_err();
                assert!(
                    format!("{e:#}").contains("set by the runner"),
                    "{arg}: {e:#}"
                );
            }
        }
        assert!(
            check_extra_args(
                Access::SwarmOnly,
                &s(&["--max-tokens=9000", "--time-limit", "60"])
            )
            .is_ok()
        );
    }

    #[test]
    fn versions_are_checked() {
        let v = crate::opencode::parse_version("vibe 3.0.1\n");
        assert_eq!(v, Some(Version(3, 0, 1)));
        assert_eq!(check_version(v).unwrap(), None);
        assert!(check_version(Some(Version(2, 26, 1))).is_err());
        assert!(check_version(None).is_err());
        assert!(
            check_version(Some(Version(4, 0, 0)))
                .unwrap()
                .unwrap()
                .contains("3.x")
        );
    }

    #[test]
    fn models_are_one_alias() {
        assert!(check_model("devstral-2").is_ok());
        assert!(check_model("mistral-medium-3.5").is_ok());
        for bad in ["", "-x", "two words"] {
            assert!(check_model(bad).is_err(), "{bad}");
        }
    }

    /// The per-run config of a `swarm-only` run: valid TOML, the wats tools
    /// only (enabled, approved), every built-in disabled, the wats server
    /// with the run's runner token, and Vibe's extras off.
    #[test]
    fn the_run_config_holds_the_wats_server_only() {
        let args = s(&["--dir", "/d", "mcp", "--profile", "alice-vibe"]);
        let text = run_config(&server(&args));
        let doc: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            doc["enabled_tools"],
            toml::Value::Array(vec!["wats_*".into()])
        );
        let disabled = doc["disabled_tools"].as_array().unwrap();
        for tool in [
            "bash",
            "read_file",
            "write_file",
            "edit",
            "web_fetch",
            "task",
        ] {
            assert!(disabled.contains(&tool.into()), "{tool}: {text}");
        }
        for key in [
            "enable_connectors",
            "enable_subagents",
            "enable_background_processes",
            "enable_update_checks",
            "enable_auto_update",
            "enable_telemetry",
        ] {
            assert_eq!(doc[key], toml::Value::Boolean(false), "{key}");
        }
        assert_eq!(
            doc["session_logging"]["enabled"],
            toml::Value::Boolean(false)
        );
        for tool in RUN_TOOLS {
            assert_eq!(
                doc["tools"][&format!("wats_{tool}")]["permission"].as_str(),
                Some("always"),
                "{tool}"
            );
        }
        let servers = doc["mcp_servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        let wats = &servers[0];
        assert_eq!(wats["name"].as_str(), Some("wats"));
        assert_eq!(wats["transport"].as_str(), Some("stdio"));
        assert_eq!(wats["command"].as_str(), Some("/opt/wats"));
        assert_eq!(
            wats["args"],
            toml::Value::Array(args.iter().map(|a| a.as_str().into()).collect())
        );
        assert_eq!(wats["env"]["WATS_RUNNER_TOKEN"].as_str(), Some("tok-123"));
        assert_eq!(wats["tool_timeout_sec"].as_integer(), Some(600));
        assert!(
            text.starts_with("# This node's Mistral Vibe agent"),
            "{text}"
        );
        // Owner-only, replaced on each run.
        let dir = tempfile::tempdir().unwrap();
        let path = write_run_config(&dir.path().join("vibe-home"), &text).unwrap();
        write_run_config(&dir.path().join("vibe-home"), &text).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn run_arguments_per_level() {
        let dir = Path::new("/w/vibe-run");
        let run = |access, max_price, extra: &[String]| {
            run_args(&Run {
                access,
                max_turns: 7,
                max_price,
                export_dir: dir,
                extra,
            })
        };
        assert_eq!(
            run(Access::SwarmOnly, Some(1.234), &[]),
            s(&[
                "--prompt=",
                "--output=streaming",
                "--output-dir=/w/vibe-run",
                "--max-turns=7",
                "--max-price=1.23",
                "--enabled-tools=wats_*",
            ])
        );
        let inherit = run(Access::Inherit, Some(0.0), &[]);
        assert!(
            inherit.contains(&"--max-price=0.01".to_string()),
            "{inherit:?}"
        );
        assert!(!inherit.iter().any(|a| a.starts_with("--enabled-tools")));
        // Limits in the extra arguments replace the runner's; no budget, no
        // --max-price.
        let given = run(
            Access::SwarmOnly,
            Some(f64::INFINITY),
            &s(&["--max-turns=3", "--max-price", "9"]),
        );
        assert!(
            !given.iter().any(|a| a.starts_with("--max-turns")),
            "{given:?}"
        );
        assert!(
            !given.iter().any(|a| a.starts_with("--max-price")),
            "{given:?}"
        );
        assert!(
            !run(Access::SwarmOnly, None, &[])
                .iter()
                .any(|a| a.starts_with("--max-price"))
        );
    }

    #[test]
    fn environment_per_level() {
        let args = s(&["mcp", "--profile", "alice-vibe"]);
        let server = server(&args);
        let work = Path::new("/w");
        let get = |env: &[(&str, OsString)], key: &str| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        let locked = run_env(Access::SwarmOnly, work, &server, Some("devstral-2"));
        assert_eq!(get(&locked, "VIBE_HOME").as_deref(), Some("/w/vibe-home"));
        assert_eq!(
            get(&locked, "MISTRAL_VIBE_SESSION_ROOT").as_deref(),
            Some("/w/vibe-home/logs/session")
        );
        assert_eq!(
            get(&locked, "VIBE_SESSION_LOGGING__ENABLED").as_deref(),
            Some("false")
        );
        assert_eq!(
            get(&locked, "VIBE_ACTIVE_MODEL").as_deref(),
            Some("devstral-2")
        );
        assert_eq!(get(&locked, "VIBE_MCP_SERVERS"), None);

        let inherit = run_env(Access::Inherit, work, &server, None);
        assert_eq!(get(&inherit, "VIBE_HOME"), None);
        assert_eq!(get(&inherit, "VIBE_ACTIVE_MODEL"), None);
        assert_eq!(
            get(&inherit, "VIBE_SESSION_LOGGING__ENABLED").as_deref(),
            Some("false")
        );
        let servers: serde_json::Value =
            serde_json::from_str(&get(&inherit, "VIBE_MCP_SERVERS").unwrap()).unwrap();
        assert_eq!(servers.as_array().unwrap().len(), 1);
        assert_eq!(servers[0]["name"], "wats");
        assert_eq!(servers[0]["command"], "/opt/wats");
        assert_eq!(servers[0]["env"]["WATS_RUNNER_TOKEN"], "tok-123");
        let tools: serde_json::Value =
            serde_json::from_str(&get(&inherit, "VIBE_TOOLS").unwrap()).unwrap();
        assert_eq!(tools.as_object().unwrap().len(), RUN_TOOLS.len());
        assert_eq!(tools["wats_reply"]["permission"], "always");

        let removed = removed_env(
            [
                "VIBE_ENABLED_TOOLS",
                "VIBE_MCP_SERVERS",
                "VIBE_HOME",
                "VIBE_CWD",
                "VIBE_APP_SERVER_BIN",
                "VIBE_THEME",
                "MISTRAL_VIBE_SESSION_ROOT",
                "MISTRAL_API_KEY",
                "PATH",
            ]
            .into_iter()
            .map(|k| (OsString::from(k), OsString::from("x"))),
        );
        let removed: Vec<String> = removed
            .iter()
            .map(|k| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            removed,
            s(&[
                "VIBE_ENABLED_TOOLS",
                "VIBE_MCP_SERVERS",
                "VIBE_HOME",
                "VIBE_CWD",
                "MISTRAL_VIBE_SESSION_ROOT",
            ])
        );
    }

    /// The export's line: the outcome, usage and cost (the run's plus its
    /// subagents'), not the effective config.
    #[test]
    fn the_run_export_is_summed_up() {
        let dir = tempfile::tempdir().unwrap();
        assert!(export_line(dir.path()).is_none());
        std::fs::write(
            dir.path().join("export.json"),
            r#"{"outcome":"turn_limit","stop_reason":"limit","error":null,"session_id":"s",
               "usage":{"input_tokens":10,"output_tokens":2,"cached_input_tokens":0,"total_tokens":12},
               "cost_usd":0.01,"steps":3,"config":{"active_model":"devstral-2","providers":[]},
               "schema_version":1,"vibe_version":"3.0.1","exit_code":3,
               "subagents":[{"cost_usd":0.005},{"cost_usd":null}]}"#,
        )
        .unwrap();
        let line = export_line(dir.path()).unwrap();
        assert_eq!(line["type"], "vibe_export");
        assert_eq!(line["outcome"], "turn_limit");
        assert!((line["cost_usd"].as_f64().unwrap() - 0.015).abs() < 1e-9);
        assert_eq!(line["steps"], 3);
        assert_eq!(line["model"], "devstral-2");
        assert_eq!(line["usage"]["total_tokens"], 12);
        assert!(line.get("config").is_none());
        assert_eq!(export_cost(&serde_json::json!({"cost_usd": 0.0})), None);
    }
}
