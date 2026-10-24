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
//          [--max-price=<usd>] [--enabled-tools=wats_*] [--agent=ask
//          --workdir=<workspace>] [extra args]   < prompt
//
// The prompt goes on stdin (an empty `--prompt` reads it there). At
// `swarm-only`, Vibe's home is the agent's own (`VIBE_HOME`), with a
// per-run `config.toml` that holds the wats MCP server only and enables
// its tools only; at `workspace`, the same plus Vibe's file tools
// ([`WORKSPACE_TOOLS`]), no shell, in the workspace (`--workdir`) with the
// built-in `ask` agent, so that a path outside it needs an approval that
// a headless run never gets; at `inherit`, the user's own Vibe runs, with
// the wats server and its tool approvals added through Vibe's `VIBE_*`
// environment layer.

/// The first Vibe whose headless mode wats was checked against (3.0.1's
/// source): the Rust CLI's run contract (`--output-dir` and its
/// `export.json`, the exit codes), MCP tools behind Vibe's permissions
/// (2.25.5) and `--enabled-tools` globs that cover them.
pub const MIN_VERSION: Version = Version(3, 0, 0);

/// The major version this integration was checked against. A later one
/// runs, with a warning.
pub const CHECKED_MAJOR: u64 = 3;

/// `inherit` needs a guardrail: `agent.review`, or an explicit
/// `agent.allow`. `false` would only warn.
pub const INHERIT_NEEDS_GUARDRAILS: bool = true;

/// The USD budgets are enforced from the cost Vibe writes to its run
/// export (its estimate from the model's prices). `false` would count runs
/// only, as for Codex.
pub const ENFORCE_USD_BUDGET: bool = true;

/// The tools wats serves to a runner session (the review tools and
/// `start_daemon` are removed there). Each gets `permission = "always"`:
/// Vibe asks before an MCP tool by default, and a headless run denies
/// every question, so without it the agent could not call them.
pub const RUN_TOOLS: [&str; 10] = [
    "post", "reply", "digest", "retract", "read", "search", "browse", "who", "wait", "status",
];

/// The only tools of a `swarm-only` run, by their exact names (Vibe names
/// MCP tools `<server>_<tool>`): `wats_post`, `wats_reply`… Never a
/// pattern, so that no other server's tool (`wats_x_…`) can match.
pub fn run_tool_names() -> Vec<String> {
    RUN_TOOLS.iter().map(|t| format!("{SERVER}_{t}")).collect()
}

/// The tools of a run at `access` (not `inherit`), by their exact names:
/// the wats tools, plus [`WORKSPACE_TOOLS`] at `workspace`.
pub fn level_tool_names(access: Access) -> Vec<String> {
    let mut names = run_tool_names();
    if access == Access::Workspace {
        names.extend(WORKSPACE_TOOLS.iter().map(|t| (*t).to_string()));
    }
    names
}

/// A Vibe `re:` pattern (case-insensitive, full match) for every tool name
/// but `names`: the last lock of `disabled_tools`, whatever new tool a
/// later Vibe adds.
pub fn others_pattern(names: &[String]) -> String {
    format!("re:(?!(?:{})$).*", names.join("|"))
}

/// Vibe's built-in and provided tools (3.0.1: `vibe/core/tools/builtins/`,
/// the app server's `cron`, `start_session` and scratchpad), disabled at
/// `swarm-only` on top of the allow list of the exact wats tools, and
/// followed by [`others_pattern`], which disables any other name a later
/// Vibe adds. `search_replace` is `edit`'s old name.
pub const BUILTIN_TOOLS: [&str; 30] = [
    "bash",
    "bash_output",
    "bash_stdin",
    "bash_sessions",
    "bash_log_file",
    "git_bash",
    "git_bash_output",
    "git_bash_stdin",
    "git_bash_sessions",
    "git_bash_log_file",
    "powershell",
    "powershell_output",
    "powershell_stdin",
    "powershell_sessions",
    "powershell_log_file",
    "shell",
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
    "cron",
    "start_session",
    "unified_harness_scratchpad",
];

/// Vibe's own cap on a prompt read from stdin (`MAX_STDIN_PROMPT_BYTES`):
/// a longer prompt is cut to fit, with a note ([`fit_prompt`]).
pub const MAX_PROMPT_BYTES: usize = 1024 * 1024;

/// At a timeout, the runner sends SIGTERM to Vibe's CLI and waits this
/// long before it kills the run's process group: on SIGTERM, headless Vibe
/// writes its run export and stops its app server, which runs in a process
/// group of its own and would outlive a SIGKILL to the CLI's.
pub const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// The same, when the node stops.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Shown when `inherit` is chosen for Vibe (setup wizard, `wats setup
/// vibe --agent`).
pub const INHERIT_WARNING: &str =
    "Vibe approves its own tools: with inherit, a post can make it read or run anything you can";

/// Vibe's own tools of a `workspace` run, on top of the wats tools: read,
/// search and edit files (`edit` is `search_replace`'s new name), never a
/// shell, the web or subagents. Each gets `permission = "always"` in the
/// run's config, which covers the workspace only: Vibe asks before a path
/// outside its working directory whatever the tool's permission (an
/// `OUTSIDE_DIRECTORY` requirement), and a headless run denies every
/// question. `agent.tools` does not change this list.
pub const WORKSPACE_TOOLS: [&str; 5] =
    ["read_file", "grep", "write_file", "edit", "search_replace"];

/// [`WORKSPACE_TOOLS`] as said to the user and in the prompt: without
/// `search_replace`, `edit`'s old name.
pub fn workspace_tools() -> Vec<String> {
    WORKSPACE_TOOLS
        .iter()
        .filter(|t| **t != "search_replace")
        .map(|t| (*t).to_string())
        .collect()
}

/// Said in a swarm-only or workspace run's prompt: two runs on Vincent's
/// Mac (2026-10-24) stopped after one turn with "I cannot use wats tools
/// in this run" and no tool call, while the tools were there.
pub fn tool_names_line() -> String {
    format!(
        " Your wats tools are MCP tools of this run, already available: call them by name \
         ({}); there is nothing to search for or install first.",
        run_tool_names().join(", ")
    )
}

/// The most of the workspace's `AGENTS.md` a `workspace` run's prompt
/// carries.
pub const MAX_INSTRUCTIONS_BYTES: usize = 16 * 1024;

/// The workspace's `AGENTS.md`, for a `workspace` run's prompt: Vibe loads
/// a project's `AGENTS.md` only in a trusted folder, and wats never
/// trusts the workspace (a trusted folder's `.vibe/config.toml`, which
/// the agent can write, would load too). A regular file only, never a
/// link (it could point outside the workspace), cut to
/// [`MAX_INSTRUCTIONS_BYTES`]. `None` without one, or when it is empty.
pub fn workspace_instructions(workdir: &Path) -> Option<String> {
    let path = workdir.join("AGENTS.md");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > MAX_INSTRUCTIONS_BYTES {
        let mut end = MAX_INSTRUCTIONS_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[wats: AGENTS.md was cut to 16 KiB]");
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The Vibe agent of a `workspace` run (`--agent`): the built-in `ask`,
/// which approves nothing itself. Vibe's default, `accept-edits`, approves
/// file edits, and `auto-approve` bypasses every permission.
pub const WORKSPACE_AGENT: &str = "ask";

/// Extra arguments refused at `swarm-only` and `workspace`: they would
/// bring back Vibe's own tools or approvals, another directory, or an old
/// session.
pub const REFUSED_ARGS: [&str; 13] = [
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
];

/// Extra arguments refused at every level: they are no headless run
/// (onboarding, an upgrade check).
pub const NEVER_ARGS: [&str; 2] = ["--setup", "--check-upgrade"];

/// Extra arguments refused at every level: the runner sets them (the
/// prompt on stdin, the transcript, the run export).
pub const RUNNER_ARGS: [&str; 4] = ["--prompt", "--prompt-file", "--output", "--output-dir"];

/// The `VIBE_*` variables a `swarm-only` run keeps from wats's own
/// environment: how this install starts Vibe's app server, and the
/// colours. Every other one is removed, since Vibe reads `VIBE_<setting>`
/// as config, above its config files (`VIBE_ENABLED_TOOLS`,
/// `VIBE_MCP_SERVERS`, `VIBE_HOME`, `VIBE_CWD`…).
pub const KEPT_ENV: [&str; 3] = ["VIBE_APP_SERVER_BIN", "VIBE_APP_SERVER_CMD", "VIBE_THEME"];

/// The access level of a Vibe agent: `swarm-only` when unset.
pub fn access(value: Option<&str>) -> Result<Access> {
    let Some(value) = value else {
        return Ok(Access::SwarmOnly);
    };
    value.trim().parse()
}

/// `inherit` hands swarm posts to the user's whole Vibe environment, whose
/// tools Vibe runs itself: refused without `agent.review` or an explicit
/// `agent.allow` ([`INHERIT_NEEDS_GUARDRAILS`], the same rule as
/// OpenCode's).
pub fn check_access(access: Access, allow: &[String], review: bool) -> Result<()> {
    if INHERIT_NEEDS_GUARDRAILS && access == Access::Inherit && allow.is_empty() && !review {
        bail!(
            "agent.access = inherit with Mistral Vibe hands swarm posts to your own Vibe \
             environment ({INHERIT_WARNING}): set agent.review = true (its posts wait for \
             you) or agent.allow (the authors who may wake it) first, or use swarm-only"
        );
    }
    Ok(())
}

/// The flag of an extra argument: `--flag` of `--flag=value`, `-x` of a
/// short option with its value attached (`-pTEXT`).
fn flag_of(arg: &str) -> &str {
    if arg.starts_with("--") {
        return arg.split('=').next().unwrap_or(arg);
    }
    match arg.char_indices().nth(2) {
        Some((end, _)) if arg.starts_with('-') => &arg[..end],
        _ => arg,
    }
}

/// Refuse extra arguments the runner sets ([`RUNNER_ARGS`], `-p`) and, at
/// `swarm-only` and `workspace`, those that would undo the level ([`REFUSED_ARGS`], `-c`).
pub fn check_extra_args(access: Access, extra: &[String]) -> Result<()> {
    for arg in extra {
        let flag = flag_of(arg);
        if NEVER_ARGS.contains(&flag) {
            bail!("agent.args: {flag} is no headless run of Mistral Vibe: remove it");
        }
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

/// The per-run `config.toml` of a `swarm-only` or `workspace` run, in the
/// agent's own Vibe home: the exact wats tools only (allowed; every
/// built-in tool and any other name disabled), plus [`WORKSPACE_TOOLS`] at
/// `workspace`, no skills (not even through `/<name>` in a prompt), no
/// connectors, subagents or background processes, no session log, no
/// update check or telemetry, and the wats MCP server with the run's
/// runner token.
pub fn run_config(server: &Server, access: Access) -> String {
    let strings = |items: &[&str]| -> toml_edit::Array { items.iter().copied().collect() };
    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix(
        "# This node's Mistral Vibe agent (wats, ADR 0033): written before every run, \
         do not edit.\n",
    );
    let owned = level_tool_names(access);
    let names: Vec<&str> = owned.iter().map(String::as_str).collect();
    doc.insert("enabled_tools", value(strings(&names)));
    let others = others_pattern(&owned);
    let mut disabled: Vec<&str> = BUILTIN_TOOLS
        .iter()
        .copied()
        .filter(|t| !names.contains(t))
        .collect();
    disabled.push(&others);
    doc.insert("disabled_tools", value(strings(&disabled)));
    // Skills: `enabled_skills` that matches nothing, and every skill
    // disabled too (Vibe ignores `disabled_skills` when `enabled_skills` is
    // set; either alone hides them). The app server would otherwise append
    // a skill's body for a `/<name>` in the prompt, skill tool or not, and
    // `~/.agents/skills` is found from HOME.
    doc.insert("enabled_skills", value(strings(&["re:(?!)"])));
    doc.insert("disabled_skills", value(strings(&["*"])));
    for key in [
        "experimental_enable_registry_skills",
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
    for name in &owned {
        let mut tool = Table::new();
        tool.insert("permission", value("always"));
        tools.insert(name, Item::Table(tool));
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
/// session store inside it. At `inherit`, see [`inherit_env`] too.
pub fn run_env(
    access: Access,
    work_dir: &Path,
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
    if access != Access::Inherit {
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
/// [`KEPT_ENV`], and `MISTRAL_VIBE_SESSION_ROOT`, whatever their case:
/// Vibe reads its settings from the environment case-insensitively
/// (`vibe_mcp_servers` is `VIBE_MCP_SERVERS`).
pub fn removed_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    vars.map(|(k, _)| k)
        .filter(|k| {
            let upper = k.to_string_lossy().to_ascii_uppercase();
            (upper.starts_with("VIBE_") && !KEPT_ENV.contains(&upper.as_str()))
                || upper == "MISTRAL_VIBE_SESSION_ROOT"
        })
        .collect()
}

/// The environment an `inherit` run adds to the user's: the wats server as
/// `VIBE_MCP_SERVERS` and its tool approvals as `VIBE_TOOLS`, which Vibe
/// merges with the user's config (servers by name, so the run's `wats`
/// entry replaces a `wats` the user registered for the human's profile).
/// The user's own values of those two variables (in any case) are merged
/// in, not overwritten: their servers but `wats`, their per-tool settings
/// but the wats tools'. Returns the variables to remove (other spellings),
/// and those to set.
pub fn inherit_env(
    server: &Server,
    vars: impl Iterator<Item = (OsString, OsString)>,
) -> (Vec<OsString>, Vec<(&'static str, String)>) {
    let mut servers: Vec<serde_json::Value> = Vec::new();
    let mut tools = serde_json::Map::new();
    let mut remove = Vec::new();
    for (key, value) in vars {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        let value = value.to_string_lossy().into_owned();
        match upper.as_str() {
            "VIBE_MCP_SERVERS" => {
                if let Ok(serde_json::Value::Array(list)) = serde_json::from_str(&value) {
                    servers.extend(list.into_iter().filter(|s| {
                        !s["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(SERVER))
                    }));
                }
            }
            "VIBE_TOOLS" => {
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&value) {
                    tools.extend(map);
                }
            }
            _ => continue,
        }
        remove.push(key);
    }
    servers.push(server.json());
    tools.extend(tool_permissions());
    (
        remove,
        vec![
            (
                "VIBE_MCP_SERVERS",
                serde_json::Value::Array(servers).to_string(),
            ),
            ("VIBE_TOOLS", serde_json::Value::Object(tools).to_string()),
        ],
    )
}

/// The `[[mcp_servers]]` of the user's Vibe config (`text`) other than
/// `wats` that run wats (their command is a `wats` binary): at `inherit`
/// they keep running as the human's profile, beside the run's own.
pub fn other_wats_servers(text: &str) -> Vec<String> {
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return Vec::new();
    };
    let Some(servers) = doc.get("mcp_servers").and_then(Item::as_array_of_tables) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter(|t| !is_wats(t))
        .filter(|t| {
            let command = match t.get("command") {
                Some(item) => match item.as_array() {
                    Some(list) => list.get(0).and_then(|v| v.as_str()).map(str::to_string),
                    None => item
                        .as_str()
                        .and_then(|c| c.split_whitespace().next().map(str::to_string)),
                },
                None => None,
            };
            command.is_some_and(|c| {
                Path::new(&c)
                    .file_name()
                    .is_some_and(|f| f == "wats" || f == "wats.exe")
            })
        })
        .filter_map(|t| t.get("name").and_then(Item::as_str).map(str::to_string))
        .collect()
}

/// `--time-limit` for a run of `timeout`: a little under it (a tenth, at
/// least 1 s and at most 15 s), so that Vibe stops by itself, with its run
/// export, before the runner has to.
pub fn time_limit(timeout: std::time::Duration) -> u64 {
    let secs = timeout.as_secs();
    let margin = (secs / 10).clamp(1, 15);
    secs.saturating_sub(margin).max(1)
}

/// The prompt as Vibe takes it on stdin: at most [`MAX_PROMPT_BYTES`]. A
/// longer one is cut at a character boundary, with a note at the end.
pub fn fit_prompt(prompt: String) -> String {
    const NOTE: &str = "\n\n[wats: this prompt was cut to fit Mistral Vibe's 1 MiB limit; \
                        read the events it lists with the wats tools]";
    if prompt.len() <= MAX_PROMPT_BYTES {
        return prompt;
    }
    let mut end = MAX_PROMPT_BYTES - NOTE.len();
    while !prompt.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{NOTE}", &prompt[..end])
}

/// What a run without a run export is charged (a run killed at its
/// timeout or by a stop: Vibe had no time to write its cost): the most it
/// was allowed to spend, its `--max-price` (from the extra arguments, else
/// what was left of the budget). `None` without a finite cap.
pub fn charge_without_export(extra: &[String], remaining_usd: f64) -> Option<f64> {
    let mut given = None;
    let mut args = extra.iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--max-price=") {
            given = value.parse::<f64>().ok();
        } else if arg == "--max-price" {
            given = args.next().and_then(|v| v.parse::<f64>().ok());
        }
    }
    given
        .or(Some(remaining_usd))
        .filter(|usd| usd.is_finite())
        .map(|usd| usd.max(0.01))
}

/// Inputs of one run's command line.
pub struct Run<'a> {
    pub access: Access,
    /// `agent.max_turns`.
    pub max_turns: u32,
    /// What is left of the USD budget, if it applies: `--max-price`.
    pub max_price: Option<f64>,
    /// What is left of the LLM token budget, if any: `--max-tokens` (ADR
    /// 0034).
    pub max_tokens: Option<u64>,
    pub export_dir: &'a Path,
    /// The workspace (`--workdir`), at `workspace`.
    pub workdir: &'a Path,
    /// `agent.timeout`: `--time-limit` a little under it.
    pub timeout: std::time::Duration,
    pub extra: &'a [String],
}

/// The arguments of one run, before the extra arguments: headless with
/// the prompt on stdin, NDJSON on stdout, the run export, the limits
/// (unless the extra arguments set them), at `swarm-only` the exact wats
/// tools only, and at `workspace` those plus [`WORKSPACE_TOOLS`], the
/// [`WORKSPACE_AGENT`] and the workspace as the working directory.
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
    if let Some(tokens) = r.max_tokens
        && !given("--max-tokens")
    {
        args.push(format!("--max-tokens={}", tokens.max(1)));
    }
    if !given("--time-limit") {
        args.push(format!("--time-limit={}", time_limit(r.timeout)));
    }
    if r.access != Access::Inherit {
        args.extend(
            level_tool_names(r.access)
                .into_iter()
                .map(|t| format!("--enabled-tools={t}")),
        );
    }
    if r.access == Access::Workspace {
        args.push(format!("--agent={WORKSPACE_AGENT}"));
        args.push(format!("--workdir={}", r.workdir.display()));
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

    /// Unset is swarm-only; `workspace`, `inherit` and the old `locked`
    /// parse.
    #[test]
    fn levels_default_to_swarm_only() {
        assert_eq!(access(None).unwrap(), Access::SwarmOnly);
        assert_eq!(access(Some("locked")).unwrap(), Access::SwarmOnly);
        assert_eq!(access(Some("inherit")).unwrap(), Access::Inherit);
        assert_eq!(access(Some("workspace")).unwrap(), Access::Workspace);
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

    /// `inherit` needs `agent.review` or an explicit `agent.allow` (either
    /// one, as for OpenCode).
    #[test]
    fn inherit_needs_review_or_allow() {
        let allow = s(&["ab"]);
        assert!(check_access(Access::SwarmOnly, &[], false).is_ok());
        assert!(check_access(Access::Workspace, &[], false).is_ok());
        assert!(check_access(Access::Inherit, &[], true).is_ok());
        assert!(check_access(Access::Inherit, &allow, false).is_ok());
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
            for level in [Access::SwarmOnly, Access::Workspace] {
                let e = check_extra_args(level, &s(&[arg])).unwrap_err();
                assert!(format!("{e:#}").contains("would undo"), "{arg}: {e:#}");
            }
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
        for arg in ["--setup", "--check-upgrade"] {
            for level in [Access::SwarmOnly, Access::Inherit] {
                let e = check_extra_args(level, &s(&[arg])).unwrap_err();
                assert!(format!("{e:#}").contains("no headless run"), "{arg}: {e:#}");
            }
        }
        // Non-ASCII arguments never panic, and are no flag of the runner's.
        assert!(check_extra_args(Access::SwarmOnly, &s(&["-é", "é", "-pé", "-"])).is_err());
        assert!(check_extra_args(Access::SwarmOnly, &s(&["-é", "é", "-"])).is_ok());
        assert_eq!(flag_of("-pé"), "-p");
        assert_eq!(flag_of("-é"), "-é");
        assert_eq!(flag_of("--max-price=é"), "--max-price");
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

    /// The per-run config of a `swarm-only` run: valid TOML, the exact wats
    /// tools only (enabled, approved), every built-in disabled and then any
    /// other name, no skills, the wats server with the run's runner token,
    /// and Vibe's extras off.
    #[test]
    fn the_run_config_holds_the_wats_server_only() {
        let args = s(&["--dir", "/d", "mcp", "--profile", "alice-vibe"]);
        let text = run_config(&server(&args), Access::SwarmOnly);
        let doc: toml::Value = toml::from_str(&text).unwrap();
        let names: Vec<toml::Value> = RUN_TOOLS
            .iter()
            .map(|t| format!("wats_{t}").into())
            .collect();
        assert_eq!(doc["enabled_tools"], toml::Value::Array(names));
        assert!(!text.contains("wats_*"), "{text}");
        let disabled = doc["disabled_tools"].as_array().unwrap();
        for tool in BUILTIN_TOOLS {
            assert!(disabled.contains(&tool.into()), "{tool}: {text}");
        }
        for tool in [
            "bash_output",
            "git_bash",
            "powershell_stdin",
            "cron",
            "start_session",
        ] {
            assert!(disabled.contains(&tool.into()), "{tool}: {text}");
        }
        // Last: every name but the exact wats tools.
        let others = disabled.last().unwrap().as_str().unwrap();
        assert_eq!(others, others_pattern(&run_tool_names()));
        assert!(
            others.starts_with("re:(?!(?:wats_post|wats_reply|") && others.ends_with(")$).*"),
            "{others}"
        );
        assert_eq!(
            doc["enabled_skills"],
            toml::Value::Array(vec!["re:(?!)".into()])
        );
        assert_eq!(doc["disabled_skills"], toml::Value::Array(vec!["*".into()]));
        for key in [
            "experimental_enable_registry_skills",
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

    /// The workspace's AGENTS.md for the prompt: a regular file only (a
    /// link could point outside the workspace), cut to 16 KiB.
    #[test]
    fn workspace_instructions_are_a_regular_file_cut_to_fit() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(workspace_instructions(dir.path()), None);
        std::fs::write(dir.path().join("AGENTS.md"), "  Be brief.\n").unwrap();
        assert_eq!(
            workspace_instructions(dir.path()).as_deref(),
            Some("Be brief.")
        );
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "é".repeat(MAX_INSTRUCTIONS_BYTES),
        )
        .unwrap();
        let cut = workspace_instructions(dir.path()).unwrap();
        assert!(cut.len() <= MAX_INSTRUCTIONS_BYTES + 64, "{}", cut.len());
        assert!(cut.ends_with("[wats: AGENTS.md was cut to 16 KiB]"));
        #[cfg(unix)]
        {
            let secret = dir.path().join("secret");
            std::fs::write(&secret, "key").unwrap();
            std::fs::remove_file(dir.path().join("AGENTS.md")).unwrap();
            std::os::unix::fs::symlink(&secret, dir.path().join("AGENTS.md")).unwrap();
            assert_eq!(workspace_instructions(dir.path()), None);
        }
    }

    /// The per-run config of a `workspace` run: the wats tools plus Vibe's
    /// file tools, each approved (Vibe still asks for a path outside the
    /// working directory), never a shell, the web or subagents.
    #[test]
    fn the_workspace_config_adds_the_file_tools_only() {
        let args = s(&["mcp"]);
        let text = run_config(&server(&args), Access::Workspace);
        let doc: toml::Value = toml::from_str(&text).unwrap();
        let enabled: Vec<&str> = doc["enabled_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(enabled.len(), RUN_TOOLS.len() + WORKSPACE_TOOLS.len());
        for tool in WORKSPACE_TOOLS {
            assert!(enabled.contains(&tool), "{tool}: {text}");
            assert_eq!(
                doc["tools"][tool]["permission"].as_str(),
                Some("always"),
                "{tool}"
            );
        }
        let disabled: Vec<&str> = doc["disabled_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        for tool in WORKSPACE_TOOLS {
            assert!(!disabled.contains(&tool), "{tool}: {text}");
        }
        for tool in [
            "bash",
            "git_bash",
            "powershell",
            "shell",
            "web_fetch",
            "task",
            "skill",
        ] {
            assert!(disabled.contains(&tool), "{tool}: {text}");
            assert!(!enabled.contains(&tool), "{tool}: {text}");
        }
        assert_eq!(
            disabled.last().copied(),
            Some(others_pattern(&level_tool_names(Access::Workspace)).as_str())
        );
        assert!(!text.contains("allowlist"), "{text}");
        assert_eq!(doc["enable_subagents"], toml::Value::Boolean(false));
    }

    /// The LLM tokens left in the budget (ADR 0034) cap the run, unless the
    /// extra arguments set `--max-tokens`.
    #[test]
    fn llm_tokens_left_cap_the_run() {
        let dir = Path::new("/w/vibe-run");
        let run = |max_tokens, extra: &[String]| {
            run_args(&Run {
                access: Access::SwarmOnly,
                max_turns: 7,
                max_price: None,
                max_tokens,
                export_dir: dir,
                workdir: Path::new("/w/workspace"),
                timeout: std::time::Duration::from_secs(600),
                extra,
            })
        };
        assert!(run(Some(1200), &[]).contains(&"--max-tokens=1200".to_string()));
        assert!(!run(None, &[]).iter().any(|a| a.starts_with("--max-tokens")));
        let mine = s(&["--max-tokens=50"]);
        assert!(
            !run(Some(1200), &mine)
                .iter()
                .any(|a| a.starts_with("--max-tokens"))
        );
    }

    #[test]
    fn run_arguments_per_level() {
        let dir = Path::new("/w/vibe-run");
        let run = |access, max_price, extra: &[String]| {
            run_args(&Run {
                access,
                max_turns: 7,
                max_price,
                max_tokens: None,
                export_dir: dir,
                workdir: Path::new("/w/workspace"),
                timeout: std::time::Duration::from_secs(600),
                extra,
            })
        };
        let mut expected = s(&[
            "--prompt=",
            "--output=streaming",
            "--output-dir=/w/vibe-run",
            "--max-turns=7",
            "--max-price=1.23",
            "--time-limit=585",
        ]);
        expected.extend(
            RUN_TOOLS
                .iter()
                .map(|t| format!("--enabled-tools=wats_{t}")),
        );
        assert_eq!(run(Access::SwarmOnly, Some(1.234), &[]), expected);
        // workspace: the file tools too, the `ask` agent, the workspace.
        let workspace = run(Access::Workspace, Some(1.234), &[]);
        let mut more: Vec<String> = WORKSPACE_TOOLS
            .iter()
            .map(|t| format!("--enabled-tools={t}"))
            .collect();
        more.extend(s(&["--agent=ask", "--workdir=/w/workspace"]));
        assert_eq!(workspace, [expected.clone(), more].concat());
        assert!(
            !workspace.iter().any(|a| a.contains("bash")),
            "{workspace:?}"
        );
        assert!(
            !run(Access::SwarmOnly, None, &[])
                .iter()
                .any(|a| a.starts_with("--agent") || a.starts_with("--workdir"))
        );
        let inherit = run(Access::Inherit, Some(0.0), &[]);
        assert!(
            inherit.contains(&"--max-price=0.01".to_string()),
            "{inherit:?}"
        );
        assert!(inherit.contains(&"--time-limit=585".to_string()));
        assert!(!inherit.iter().any(|a| a.starts_with("--enabled-tools")));
        // Limits in the extra arguments replace the runner's; no budget, no
        // --max-price.
        let given = run(
            Access::SwarmOnly,
            Some(f64::INFINITY),
            &s(&["--max-turns=3", "--max-price", "9", "--time-limit=30"]),
        );
        for flag in ["--max-turns", "--max-price", "--time-limit"] {
            assert!(
                !given.iter().any(|a| a.starts_with(flag)),
                "{flag}: {given:?}"
            );
        }
        assert!(
            !run(Access::SwarmOnly, None, &[])
                .iter()
                .any(|a| a.starts_with("--max-price"))
        );
        // A little under the timeout: a tenth, 1 to 15 s.
        let secs = std::time::Duration::from_secs;
        assert_eq!(time_limit(secs(600)), 585);
        assert_eq!(time_limit(secs(60)), 54);
        assert_eq!(time_limit(secs(10)), 9);
        assert_eq!(time_limit(secs(2)), 1);
        assert_eq!(time_limit(secs(1)), 1);
    }

    /// What a run killed before its export is charged: its --max-price.
    #[test]
    fn a_run_without_export_is_charged_its_cap() {
        assert_eq!(charge_without_export(&[], 1.5), Some(1.5));
        assert_eq!(charge_without_export(&[], 0.0), Some(0.01));
        assert_eq!(charge_without_export(&[], f64::INFINITY), None);
        assert_eq!(
            charge_without_export(&s(&["--max-price=0.4"]), 1.5),
            Some(0.4)
        );
        assert_eq!(
            charge_without_export(&s(&["--max-price", "3"]), 1.5),
            Some(3.0)
        );
    }

    /// A prompt over Vibe's 1 MiB is cut at a character boundary, with a
    /// note; a shorter one is kept as it is.
    #[test]
    fn long_prompts_are_cut_to_fit() {
        assert_eq!(fit_prompt("hi".into()), "hi");
        let long = "é".repeat(MAX_PROMPT_BYTES);
        let cut = fit_prompt(long);
        assert!(cut.len() <= MAX_PROMPT_BYTES, "{}", cut.len());
        assert!(cut.ends_with("read the events it lists with the wats tools]"));
        assert!(cut.starts_with("éé"));
    }

    /// Other servers of the user's Vibe config that run wats (at
    /// `inherit`, they keep posting as the human).
    #[test]
    fn other_wats_servers_are_found() {
        let text = "[[mcp_servers]]\nname = \"wats\"\ncommand = \"/usr/bin/wats\"\n\n\
                    [[mcp_servers]]\nname = \"swarm\"\ncommand = \"/opt/homebrew/bin/wats\"\n\
                    args = [\"mcp\"]\n\n\
                    [[mcp_servers]]\nname = \"line\"\ncommand = \"wats mcp --profile x\"\n\n\
                    [[mcp_servers]]\nname = \"fetch\"\ncommand = \"uvx\"\n";
        assert_eq!(other_wats_servers(text), s(&["swarm", "line"]));
        assert!(other_wats_servers("not [toml").is_empty());
        assert!(other_wats_servers("").is_empty());
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
        let locked = run_env(Access::SwarmOnly, work, Some("devstral-2"));
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

        let inherit = run_env(Access::Inherit, work, None);
        assert_eq!(get(&inherit, "VIBE_HOME"), None);
        assert_eq!(get(&inherit, "VIBE_ACTIVE_MODEL"), None);
        assert_eq!(
            get(&inherit, "VIBE_SESSION_LOGGING__ENABLED").as_deref(),
            Some("false")
        );

        // inherit: the wats server and approvals, alone…
        let none = std::iter::empty::<(OsString, OsString)>();
        let (remove, set) = inherit_env(&server, none);
        assert!(remove.is_empty());
        let servers: serde_json::Value = serde_json::from_str(&set[0].1).unwrap();
        assert_eq!(set[0].0, "VIBE_MCP_SERVERS");
        assert_eq!(servers.as_array().unwrap().len(), 1);
        assert_eq!(servers[0]["name"], "wats");
        assert_eq!(servers[0]["command"], "/opt/wats");
        assert_eq!(servers[0]["env"]["WATS_RUNNER_TOKEN"], "tok-123");
        let tools: serde_json::Value = serde_json::from_str(&set[1].1).unwrap();
        assert_eq!(set[1].0, "VIBE_TOOLS");
        assert_eq!(tools.as_object().unwrap().len(), RUN_TOOLS.len());
        assert_eq!(tools["wats_reply"]["permission"], "always");
        // …or merged into the user's own, whatever their case.
        let user = [
            (
                "vibe_mcp_servers",
                r#"[{"name":"fetch","command":"uvx"},{"name":"WATS","command":"/x"}]"#,
            ),
            ("VIBE_TOOLS", r#"{"bash":{"permission":"never"}}"#),
            ("PATH", "/bin"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let (remove, set) = inherit_env(&server, user.into_iter());
        assert_eq!(
            remove,
            [
                OsString::from("vibe_mcp_servers"),
                OsString::from("VIBE_TOOLS")
            ]
        );
        let servers: serde_json::Value = serde_json::from_str(&set[0].1).unwrap();
        let names: Vec<&str> = servers
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["fetch", "wats"]);
        assert_eq!(servers[1]["command"], "/opt/wats");
        let tools: serde_json::Value = serde_json::from_str(&set[1].1).unwrap();
        assert_eq!(tools["bash"]["permission"], "never");
        assert_eq!(tools["wats_post"]["permission"], "always");

        // swarm-only removes every VIBE_* but three, in any case.
        let removed = removed_env(
            [
                "VIBE_ENABLED_TOOLS",
                "VIBE_MCP_SERVERS",
                "vibe_mcp_servers",
                "vibe_tools",
                "Vibe_Bypass_Tool_Permissions",
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
                "vibe_mcp_servers",
                "vibe_tools",
                "Vibe_Bypass_Tool_Permissions",
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
