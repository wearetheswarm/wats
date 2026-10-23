//! What the headless agent may use besides the wats tools (`agent.access`,
//! ADR 0011): one knob, three levels, the same for every runtime.
//!
//! - **swarm-only**: the raw model plus the wats tools, nothing else, in an
//!   empty directory (`locked` before 0.4.0, still accepted).
//! - **workspace** (default): the agent runs in its own directory
//!   (`agent.workdir`, by default `<data dir>/agent/<name>/workspace`),
//!   with that directory's instructions, skills, settings and MCP servers,
//!   plus the tools listed in `agent.tools`. Not the user's environment.
//! - **inherit**: the user's own environment (MCP servers, settings,
//!   permission mode); wats only adds its server, budget and transcripts.
//!
//! The rule the docs repeat: whatever the agent can read can leak; whatever
//! it can do, a post can ask for.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The access level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Access {
    SwarmOnly,
    #[default]
    Workspace,
    Inherit,
}

pub const LEVELS: [&str; 3] = ["swarm-only", "workspace", "inherit"];

/// The name `swarm-only` had before 0.4.0: still accepted, deprecated.
pub const LOCKED: &str = "locked";

/// The one-line note shown when `locked` is given.
pub const LOCKED_NOTE: &str = "access `locked` is now called `swarm-only` (0.4.0): \
     same behaviour, the old name is deprecated";

/// The deprecation note of an access value, if it uses the old name.
pub fn deprecation(value: &str) -> Option<&'static str> {
    (value.trim() == LOCKED).then_some(LOCKED_NOTE)
}

/// The values `--access` accepts: the levels, and `locked` (hidden).
pub fn value_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        LEVELS
            .iter()
            .map(|l| clap::builder::PossibleValue::new(*l))
            .chain([clap::builder::PossibleValue::new(LOCKED).hide(true)]),
    )
}

/// An access value as set (config.toml, `--access`), else the default.
pub fn level(value: Option<&str>) -> Result<Access> {
    value.map_or(Ok(Access::default()), str::parse)
}

/// An agent's access level for its runtime: Mistral Vibe defaults to
/// `swarm-only` and has no `workspace` ([`crate::vibe::access`], ADR
/// 0033); the others as [`level`].
pub fn level_for(runtime: &str, value: Option<&str>) -> Result<Access> {
    match runtime {
        "vibe" => crate::vibe::access(value),
        "copilot" => crate::copilot::access(value),
        "antigravity" => crate::antigravity::access(value),
        // No level: wats cannot confine it (ADR 0039 §3).
        crate::custom::RUNTIME => match value {
            None | Some("inherit") => Ok(Access::Inherit),
            Some(other) => anyhow::bail!(
                "agent.access {other:?}: a custom agent has no access level, it is {}",
                crate::custom::UNCONFINED
            ),
        },
        _ => level(value),
    }
}

impl std::str::FromStr for Access {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "swarm-only" | LOCKED => Ok(Access::SwarmOnly),
            "workspace" => Ok(Access::Workspace),
            "inherit" => Ok(Access::Inherit),
            _ => anyhow::bail!("access is one of: {}", LEVELS.join(", ")),
        }
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Access::SwarmOnly => "swarm-only",
            Access::Workspace => "workspace",
            Access::Inherit => "inherit",
        })
    }
}

const WORKSPACE_README: &str = "\
# Agent workspace

This is the working directory of this node's WeAreTheSwarm agent
(`agent.access = \"workspace\"`). Write here who the agent is, what it
knows and how it should answer; it reads this file at every run.

- Claude Code: skills go in `.claude/skills/`, MCP servers in `.mcp.json`,
  settings in `.claude/settings.json`.
- Codex: MCP servers go in `.codex/config.toml` (`[mcp_servers.<name>]`).
- OpenCode: MCP servers go in `opencode.json` (`\"mcp\"`).
- Tools are listed in `agent.tools`: Claude Code tools (`wats config set
  agent.tools Read,Grep`), Codex features (`image_generation`) or OpenCode
  permission keys (`read,grep`).

Whatever the agent can read here may end up posted to the swarm, and
whatever it can do, a post can ask for.
";

/// Create the workspace with stub CLAUDE.md and AGENTS.md if missing.
pub fn ensure_workspace(workdir: &Path) -> Result<()> {
    std::fs::create_dir_all(workdir)
        .with_context(|| format!("create the agent workspace {}", workdir.display()))?;
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let path = workdir.join(name);
        if !path.exists() {
            std::fs::write(&path, WORKSPACE_README)?;
        }
    }
    Ok(())
}

/// The MCP servers of a Claude Code workspace (`<workdir>/.mcp.json`).
pub fn workspace_claude_servers(workdir: &Path) -> serde_json::Map<String, Value> {
    std::fs::read(workdir.join(".mcp.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v["mcpServers"].as_object().cloned())
        .unwrap_or_default()
}

/// The MCP servers of a Codex workspace (`<workdir>/.codex/config.toml`),
/// as `-c` overrides.
pub fn workspace_codex_overrides(workdir: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let path = workdir.join(".codex").join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("invalid {}", path.display()))?;
    let mut names = Vec::new();
    let mut overrides = Vec::new();
    if let Some(servers) = doc.get("mcp_servers").and_then(|s| s.as_table_like()) {
        for (name, server) in servers.iter() {
            if name == "wats" {
                continue;
            }
            let Some(server) = server.as_table_like() else {
                continue;
            };
            names.push(name.to_string());
            for (key, value) in server.iter() {
                if let Some(value) = value.as_value() {
                    overrides.push(format!(
                        "mcp_servers.{name}.{key}={}",
                        value.to_string().trim()
                    ));
                }
            }
        }
    }
    Ok((names, overrides))
}

/// Whether a flag is already among the extra arguments.
fn given(extra: &[String], flag: &str) -> bool {
    extra
        .iter()
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
}

/// Flags with values, skipping those the extra arguments set.
fn defaults(extra: &[String], pairs: &[(&str, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (flag, value) in pairs {
        if !given(extra, flag) {
            out.extend([flag.to_string(), value.clone()]);
        }
    }
    out
}

/// Claude Code tools that touch files: unscoped in `agent.tools`, they are
/// allowed only inside the workspace (`Read(./**)`), since a bare `Read`
/// rule allows reading anywhere on disk (checked on a real run).
const FILE_TOOLS: [&str; 6] = ["Read", "Glob", "Grep", "Edit", "Write", "NotebookEdit"];

/// `Read` → (`Read`, `Read(./**)`); `Read(/srv/docs/**)` stays as given;
/// `WebFetch` → (`WebFetch`, `WebFetch`).
fn tool_rule(tool: &str) -> (String, String) {
    let base = tool.split('(').next().unwrap_or(tool).trim().to_string();
    let rule = if tool.contains('(') || !FILE_TOOLS.contains(&base.as_str()) {
        tool.to_string()
    } else {
        format!("{base}(./**)")
    };
    (base, rule)
}

/// Inputs of one Claude Code run.
pub struct ClaudeRun<'a> {
    pub access: Access,
    pub mcp_config: &'a Path,
    /// MCP servers of the workspace (their tools are allowed).
    pub workspace_servers: &'a [String],
    pub tools: &'a [String],
    pub extra: &'a [String],
    /// The node's data directory, which the agent may not change.
    pub data_dir: &'a Path,
    /// Where the run starts (the workspace, or the swarm-only directory).
    pub run_dir: &'a Path,
}

/// The `wats` commands an agent's shell may not run: they change what the
/// node does, or acknowledge such a change.
pub const DENIED_WATS_COMMANDS: [&str; 4] = ["config", "resume", "guardian", "setup"];

/// A path as a Claude Code permission rule path: `//` starts an absolute
/// path (a single `/` is relative to the settings file).
fn rule_path(path: &Path) -> String {
    format!("/{}", path.display())
}

/// The path, and its canonical form when it differs (symlinks such as
/// macOS's /var → /private/var).
fn spellings(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(path)
        && canonical != path
    {
        out.push(canonical);
    }
    out
}

/// What the agent may not write: the data directory, except the run
/// directory when it lies inside (the default workspace is
/// `<data dir>/agent/workspace`). Then every entry along the way, but the
/// one leading to the run directory, is listed, with config.toml and
/// safety.json even before they exist.
fn protected_paths(data_dir: &Path, run_dir: &Path) -> Vec<String> {
    let Ok(inside) = run_dir.strip_prefix(data_dir) else {
        return vec![format!("{}/**", rule_path(data_dir))];
    };
    let mut out = Vec::new();
    let mut level = data_dir.to_path_buf();
    for next in inside.components() {
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&level)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok().map(|e| e.file_name()))
                    .collect()
            })
            .unwrap_or_default();
        if level == data_dir {
            for name in [crate::settings::FILE_NAME, crate::safety::FILE_NAME] {
                if !names.iter().any(|n| n == name) {
                    names.push(name.into());
                }
            }
        }
        names.sort();
        for name in names.iter().filter(|n| n.as_os_str() != next.as_os_str()) {
            let path = rule_path(&level.join(name));
            out.push(path.clone());
            out.push(format!("{path}/**"));
        }
        level.push(next);
    }
    out
}

/// Claude Code deny rules of a swarm-only or workspace run: no edit nor write
/// in the data directory (config.toml, safety.json, the budget, keys), nor
/// to the workspace's own settings and MCP servers (hooks and servers run
/// commands); no shell command naming the data directory, config.toml or
/// safety.json, nor running `wats config|resume|guardian|setup`. Deny
/// rules win over any allow rule, the workspace's included.
pub fn claude_deny_rules(data_dir: &Path, run_dir: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    for data in spellings(data_dir) {
        for run in spellings(run_dir) {
            paths.extend(protected_paths(&data, &run));
        }
    }
    for run in spellings(run_dir) {
        paths.push(format!("{}/.claude/**", rule_path(&run)));
        paths.push(rule_path(&run.join(".mcp.json")));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    let mut rules: Vec<String> = Vec::new();
    for tool in ["Edit", "Write"] {
        rules.extend(paths.iter().map(|p| format!("{tool}({p})")));
    }
    rules.extend(
        shell_deny_mentions(data_dir)
            .iter()
            .map(|m| format!("Bash(*{m}*)")),
    );
    // From a workspace inside the data directory, `..` leads to it.
    if run_dir.starts_with(data_dir) {
        rules.push("Bash(*..*)".into());
    }
    rules
}

/// What a shell command of a swarm-only or workspace run may not name:
/// the data directory (also as `~/…` and `$HOME/…`), config.toml,
/// safety.json, and `wats config|resume|guardian|setup`. Claude Code's
/// `Bash(*…*)` deny rules and OpenCode's `bash` patterns (ADR 0031) are
/// built from it.
pub fn shell_deny_mentions(data_dir: &Path) -> Vec<String> {
    let mut mentions: Vec<String> = spellings(data_dir)
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    if let Some(home) = std::env::home_dir()
        && let Ok(rel) = data_dir.strip_prefix(&home)
    {
        let rel = rel.display();
        mentions.extend([
            format!("~/{rel}"),
            format!("$HOME/{rel}"),
            format!("${{HOME}}/{rel}"),
        ]);
    }
    mentions.extend([
        crate::settings::FILE_NAME.to_string(),
        crate::safety::FILE_NAME.to_string(),
    ]);
    mentions.extend(DENIED_WATS_COMMANDS.iter().map(|c| format!("wats {c}")));
    mentions
}

/// `claude -p` arguments for one run, before the per-run limits and the
/// extra arguments.
pub fn claude_args(run: &ClaudeRun) -> Vec<String> {
    let mut out: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"]
        .map(str::to_string)
        .to_vec();
    out.extend(["--mcp-config".into(), run.mcp_config.display().to_string()]);
    let mut allowed = vec!["mcp__wats__*".to_string()];
    match run.access {
        Access::SwarmOnly => {
            out.push("--strict-mcp-config".into());
            out.extend(defaults(
                run.extra,
                &[
                    ("--tools", String::new()),
                    ("--permission-mode", "dontAsk".into()),
                    ("--setting-sources", "project".into()),
                ],
            ));
            out.extend(deny_settings(run));
        }
        Access::Workspace => {
            out.push("--strict-mcp-config".into());
            // A server some `agent.tools` entry names a tool of
            // (`mcp__crm__search`) is narrowed to the tools named.
            allowed.extend(
                run.workspace_servers
                    .iter()
                    .filter(|name| crate::share::server_tools(run.tools, name).is_empty())
                    .map(|name| format!("mcp__{name}__*")),
            );
            let rules: Vec<(String, String)> = run.tools.iter().map(|t| tool_rule(t)).collect();
            // `--tools` lists built-in tools; MCP tools (`mcp__docs__search`)
            // are only allowed.
            let mut bases: Vec<String> = rules
                .iter()
                .map(|(b, _)| b.clone())
                .filter(|b| !b.starts_with("mcp__"))
                .collect();
            bases.dedup();
            allowed.extend(rules.into_iter().map(|(_, r)| r));
            out.extend(defaults(
                run.extra,
                &[
                    ("--tools", bases.join(",")),
                    ("--permission-mode", "dontAsk".into()),
                    ("--setting-sources", "project".into()),
                ],
            ));
            out.extend(deny_settings(run));
        }
        // The user's MCP servers, settings and permission mode apply.
        Access::Inherit => {}
    }
    out.extend(["--allowedTools".into(), allowed.join(",")]);
    out
}

/// `--settings` with the deny rules ([`claude_deny_rules`]): flag
/// settings load whatever `--setting-sources` says.
fn deny_settings(run: &ClaudeRun) -> [String; 2] {
    let settings = json!({
        "permissions": {"deny": claude_deny_rules(run.data_dir, run.run_dir)},
    });
    ["--settings".into(), settings.to_string()]
}

/// Refuse a workspace that is the data directory or contains it: the
/// agent's tools are scoped to its workspace, which would then include
/// config.toml.
pub fn check_workdir(workdir: &Path, data_dir: &Path) -> Result<()> {
    for work in spellings(workdir) {
        for data in spellings(data_dir) {
            anyhow::ensure!(
                !data.starts_with(&work),
                "agent.workdir {} is or contains the data directory {}: choose a \
                 directory of its own (the default is {})",
                workdir.display(),
                data_dir.display(),
                data_dir.join("agent").join("workspace").display()
            );
        }
    }
    Ok(())
}

/// Write the MCP config of one Claude Code run: the wats server (with the
/// run's token), plus the workspace's servers and the lent ones (ADR
/// 0037), a lent one replacing a workspace one of the same name. Written
/// `0600`: it holds the runner token and the lent servers' secrets.
/// Returns the servers' names.
#[allow(clippy::too_many_arguments)]
pub fn write_claude_mcp_config(
    path: &Path,
    access: Access,
    workdir: &Path,
    lent: &[(String, Value)],
    wats_command: &Path,
    wats_args: &[String],
    runner_token: &str,
) -> Result<Vec<String>> {
    let mut servers = if access == Access::Workspace {
        workspace_claude_servers(workdir)
    } else {
        serde_json::Map::new()
    };
    if access == Access::Workspace {
        for (name, definition) in lent {
            servers.insert(name.clone(), definition.clone());
        }
    }
    servers.remove("wats");
    let names: Vec<String> = servers.keys().cloned().collect();
    servers.insert(
        "wats".into(),
        json!({
            "type": "stdio",
            "command": wats_command,
            "args": wats_args,
            "env": {"WATS_RUNNER_TOKEN": runner_token},
        }),
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(
        path,
        &serde_json::to_vec_pretty(&json!({"mcpServers": servers}))?,
    )?;
    Ok(names)
}

/// Write a file only its owner can read (`0600` on Unix), also when it
/// already exists with wider permissions.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Codex features that could reach beyond the wats tools: shell, browser,
/// computer use, connectors (apps), plugins, hooks, images, sub-agents.
/// Only those the installed Codex knows are disabled (an unknown name is
/// an error).
pub const CODEX_DENIED_FEATURES: &[&str] = &[
    "shell_tool",
    "apps",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "in_app_browser",
    "computer_use",
    "image_generation",
    "view_image",
    "multi_agent",
    "multi_agent_v2",
    "plugins",
    "remote_plugin",
    "plugin_sharing",
    "hooks",
    "goals",
    "memories",
    "skill_mcp_dependency_install",
    "skill_search",
    "tool_suggest",
    "standalone_web_search",
];

/// The Codex feature that generates images (`image_gen.imagegen`, saved to
/// `$CODEX_HOME/generated_images/<thread id>/<call id>.png`).
pub const CODEX_IMAGE_GENERATION: &str = "image_generation";

/// What a Codex run switches off and on, from `agent.tools` and the
/// features the installed Codex knows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodexFeatures {
    /// The [`CODEX_DENIED_FEATURES`] it knows, but those `agent.tools` lists.
    pub disable: Vec<String>,
    /// Features `agent.tools` lists (workspace access): enabled.
    pub enable: Vec<String>,
    /// Names in `agent.tools` that are no feature of this Codex.
    pub unknown: Vec<String>,
}

/// The features of a swarm-only or workspace Codex run. Only `workspace`
/// opens what `agent.tools` lists; `swarm-only` ignores it.
pub fn codex_features_for(access: Access, tools: &[String], known: &[String]) -> CodexFeatures {
    let listed: &[String] = if access == Access::Workspace {
        tools
    } else {
        &[]
    };
    let knows = |f: &str| known.iter().any(|k| k == f);
    CodexFeatures {
        disable: CODEX_DENIED_FEATURES
            .iter()
            .filter(|f| knows(f) && !listed.iter().any(|t| t == *f))
            .map(|f| f.to_string())
            .collect(),
        enable: listed.iter().filter(|t| knows(t)).cloned().collect(),
        unknown: listed.iter().filter(|t| !knows(t)).cloned().collect(),
    }
}

/// Whether a Codex run can generate images: `inherit` (the user's own
/// features), or `workspace` with `image_generation` in `agent.tools`.
pub fn codex_generates_images(runtime: &str, access: Access, tools: &[String]) -> bool {
    runtime == "codex"
        && match access {
            Access::SwarmOnly => false,
            Access::Workspace => tools.iter().any(|t| t == CODEX_IMAGE_GENERATION),
            Access::Inherit => true,
        }
}

/// Where Codex keeps its state: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
}

/// Where Codex saves the images of one thread:
/// `<codex home>/generated_images/<thread id>`, the id sanitized as Codex
/// does (`ext/image-generation/src/artifact.rs`).
pub fn codex_images_dir(codex_home: &Path, thread_id: &str) -> PathBuf {
    let mut id: String = thread_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() {
        id = "generated_image".into();
    }
    codex_home.join("generated_images").join(id)
}

/// The features the installed Codex knows (`codex features list`).
pub async fn codex_features(command: &Path) -> Vec<String> {
    let Ok(out) = tokio::process::Command::new(command)
        .args(["features", "list"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

/// Inputs of one Codex run.
pub struct CodexRun<'a> {
    pub access: Access,
    pub wats_command: &'a Path,
    pub wats_args: &'a [String],
    pub runner_token: &'a str,
    /// Features to disable (swarm-only and workspace).
    pub disable: &'a [String],
    /// Features `agent.tools` lists, enabled (workspace).
    pub enable: &'a [String],
    /// `-c` overrides for the workspace's MCP servers, then the lent ones
    /// ([`crate::share::codex_lent`]).
    pub workspace_overrides: &'a [String],
    pub extra: &'a [String],
}

/// `codex exec` for one run: JSON events, nothing persisted, the wats MCP
/// server with its tools approved and the run's token. Swarm-only and
/// workspace also skip the user's config and rules, sandbox read-only,
/// approvals never. The prompt comes on stdin.
pub fn codex_args(run: &CodexRun) -> Vec<String> {
    let toml = |v: &str| serde_json::to_string(v).unwrap_or_default();
    let mut out: Vec<String> = ["exec", "--json", "--ephemeral", "--skip-git-repo-check"]
        .map(str::to_string)
        .to_vec();
    if run.access != Access::Inherit {
        out.extend(
            [
                "--ignore-user-config",
                "--ignore-rules",
                "--sandbox",
                "read-only",
                "-c",
                "web_search=\"disabled\"",
            ]
            .map(str::to_string),
        );
    }
    let args: Vec<String> = run.wats_args.iter().map(|a| toml(a)).collect();
    let mut config = vec![
        "approval_policy=\"never\"".to_string(),
        // The user's ~/.codex/AGENTS.md still loads (no flag skips it): it
        // adds no tool, but may ask for another language.
        "developer_instructions=\"Write everything you post to WeAreTheSwarm in English.\""
            .to_string(),
        "mcp_servers.wats.default_tools_approval_mode=\"approve\"".to_string(),
        format!(
            "mcp_servers.wats.command={}",
            toml(&run.wats_command.to_string_lossy())
        ),
        format!("mcp_servers.wats.args=[{}]", args.join(",")),
        format!(
            "mcp_servers.wats.env.WATS_RUNNER_TOKEN={}",
            toml(run.runner_token)
        ),
    ];
    if run.access == Access::Workspace {
        config.extend(run.workspace_overrides.iter().cloned());
    }
    for c in config {
        out.extend(["-c".into(), c]);
    }
    if run.access != Access::Inherit {
        for feature in run.disable {
            out.extend(["--disable".into(), feature.clone()]);
        }
        if run.access == Access::Workspace {
            for feature in run.enable {
                out.extend(["--enable".into(), feature.clone()]);
            }
        }
    }
    out.extend(run.extra.iter().cloned());
    out.push("-".into());
    out
}

/// What the banner says about the agent's access, and its warnings.
pub struct Summary {
    pub line: String,
    pub warnings: Vec<String>,
}

fn home() -> Option<PathBuf> {
    std::env::home_dir()
}

/// The user's Claude Code environment: user MCP servers and the default
/// permission mode.
fn claude_environment() -> (usize, String) {
    let servers = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(home)
        .and_then(|d| std::fs::read(d.join(".claude.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["mcpServers"].as_object().map(|m| m.len()))
        .unwrap_or(0);
    let mode = home()
        .and_then(|h| std::fs::read(h.join(".claude/settings.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["permissions"]["defaultMode"].as_str().map(str::to_string))
        .unwrap_or_else(|| "default".into());
    (servers, mode)
}

/// The user's Codex MCP servers.
fn codex_servers() -> usize {
    codex_home()
        .and_then(|d| std::fs::read_to_string(d.join("config.toml")).ok())
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("mcp_servers")
                .and_then(|s| s.as_table_like())
                .map(|t| t.len())
        })
        .unwrap_or(0)
}

/// What holds a lent shell's commands, as the banner and `wats status`
/// say it: "not sandboxed" unless the runtime's own sandbox is on.
pub fn shell_sandbox(runtime: &str) -> &'static str {
    match runtime {
        "antigravity" => "sandboxed (its terminal sandbox: nothing outside the workspace)",
        "codex" => "read-only sandbox (it can still read your files and secrets)",
        _ => "not sandboxed",
    }
}

/// Whether an `agent.tools` entry is a shell: Claude Code's `Bash`,
/// Codex's `shell_tool`, OpenCode's `bash` (or `shell`), with or without
/// a pattern.
fn gives_shell(tool: &str) -> bool {
    let base = tool.split('(').next().unwrap_or(tool).trim();
    matches!(base, "Bash" | "shell_tool")
        || base.eq_ignore_ascii_case("bash")
        || base.eq_ignore_ascii_case("shell")
}

/// What a run can use, in one sentence for its prompt (`{access}`,
/// ADR 0018 §1): an agent that knows it cannot build or test offers what
/// it can do instead of failing or declining silently.
pub fn prompt_line(runtime: &str, access: Access, workdir: &Path, tools: &[String]) -> String {
    const NOTHING_TO_RUN: &str = "You cannot run, build or test anything in this run.";
    let shell = tools.iter().any(|t| gives_shell(t));
    let line = match access {
        Access::SwarmOnly if runtime == "openclaw" => format!(
            "Your OpenClaw agent's tool policy for wats runs, normally the wats tools only. \
             Unless you see other tools, assume this: {NOTHING_TO_RUN}"
        ),
        Access::SwarmOnly => format!(
            "Only the wats tools: no shell, no files, no compiler or interpreter. {NOTHING_TO_RUN}"
        ),
        Access::Workspace if runtime == "openclaw" => {
            "Your OpenClaw agent's tool policy, with the tools added to it.".to_string()
        }
        Access::Workspace if tools.is_empty() => format!(
            "Only the wats tools, in {}. {NOTHING_TO_RUN}",
            workdir.display()
        ),
        Access::Workspace => format!(
            "The wats tools plus {} in {}, nothing outside it.{}",
            tools.join(", "),
            workdir.display(),
            if shell {
                String::new()
            } else {
                format!(" No shell: {NOTHING_TO_RUN}")
            }
        ),
        Access::Inherit => "The tools of your runtime's own configuration.".to_string(),
    };
    let mut line = line;
    if access == Access::Workspace && runtime != "openclaw" {
        let (search, fetch) = web_abilities(runtime, tools);
        let yes = |b: bool| if b { "yes" } else { "no" };
        line.push_str(&format!(
            " You can search the web: {}. You can fetch a URL (read a page or an API): {}.",
            yes(search),
            yes(fetch)
        ));
        if runtime == "antigravity" && fetch {
            line.push_str(
                " To read a page or an API URL, use read_url_content; search_web returns \
                 snippets only.",
            );
        }
    }
    if codex_generates_images(runtime, access, tools) {
        format!("{line} {IMAGES_LINE}")
    } else {
        line
    }
}

/// Whether a `workspace` run can search the web, and fetch a URL, with
/// `agent.tools`: each runtime's own tools for it.
pub fn web_abilities(runtime: &str, tools: &[String]) -> (bool, bool) {
    let has = |names: &[&str]| {
        tools.iter().any(|t| {
            let base = t.split('(').next().unwrap_or(t).trim();
            names.iter().any(|n| base.eq_ignore_ascii_case(n))
        })
    };
    match runtime {
        "claude" => (has(&["WebSearch"]), has(&["WebFetch"])),
        "codex" => (has(&["web_search", "standalone_web_search"]), false),
        "opencode" => (has(&["websearch"]), has(&["webfetch"])),
        "copilot" => (false, has(&["url"])),
        "antigravity" => (has(&["web", "web-search"]), has(&["web", "web-fetch"])),
        _ => (false, false),
    }
}

/// What the prompt tells a Codex run that can generate images: the file
/// is attachable (its run's thread directory, [`codex_images_dir`]).
pub const IMAGES_LINE: &str = "Image generation saves each image to a file and its \
     reply gives the path: to answer with an image, attach that file through `blobs` \
     in your reply or post.";

/// Inputs of the banner summary.
pub struct Describe<'a> {
    pub runtime: &'a str,
    pub access: Access,
    pub workdir: &'a Path,
    pub tools: &'a [String],
    pub allow: &'a [String],
    pub review: bool,
}

pub fn describe(d: &Describe) -> Summary {
    let codex = d.runtime == "codex";
    // `reads`: whether the agent can read anything beyond its prompt and
    // instructions (a workspace without tools nor MCP servers cannot).
    let (line, shell, reads) = match d.access {
        // OpenClaw enforces its agent's tool policy itself (ADR 0017 §3).
        Access::SwarmOnly if d.runtime == "openclaw" => (
            "swarm-only: the OpenClaw agent's tool policy (setup recommends allow \
             [\"wats-agent__*\"] only, sandboxed)"
                .to_string(),
            false,
            false,
        ),
        Access::Workspace if d.runtime == "openclaw" => (
            "workspace: the OpenClaw agent's tool policy, with the tools you added to it"
                .to_string(),
            false,
            true,
        ),
        Access::Inherit if d.runtime == "openclaw" => (
            "inherit: your own OpenClaw agent (agent.openclaw_agent), with all its tools"
                .to_string(),
            true,
            true,
        ),
        Access::SwarmOnly => ("swarm-only: only the wats tools".to_string(), false, false),
        Access::Workspace => {
            let servers = if codex {
                workspace_codex_overrides(d.workdir)
                    .map(|(n, _)| n)
                    .unwrap_or_default()
            } else if d.runtime == "opencode" {
                crate::opencode::workspace_servers(d.workdir).unwrap_or_default()
            } else {
                workspace_claude_servers(d.workdir)
                    .keys()
                    .cloned()
                    .collect()
            };
            let shell = d.tools.iter().any(|t| gives_shell(t));
            (
                format!(
                    "workspace {}: tools [{}], MCP servers [{}]{}",
                    d.workdir.display(),
                    d.tools.join(", "),
                    servers.join(", "),
                    if shell {
                        format!(", shell: on, {}", shell_sandbox(d.runtime))
                    } else {
                        String::new()
                    }
                ),
                shell,
                !d.tools.is_empty() || !servers.is_empty(),
            )
        }
        Access::Inherit if d.runtime == "vibe" => (
            format!(
                "inherit: your full Mistral Vibe environment: your ~/.vibe config, MCP servers, \
                 agents and skills ({})",
                crate::vibe::INHERIT_WARNING
            ),
            true,
            true,
        ),
        Access::Inherit if d.runtime == crate::custom::RUNTIME => {
            (crate::custom::UNCONFINED.to_string(), true, true)
        }
        Access::Inherit if d.runtime == "opencode" => (
            format!(
                "inherit: your full OpenCode environment: your config, MCP servers, plugins and \
                 permissions ({})",
                crate::opencode::INHERIT_WARNING
            ),
            true,
            true,
        ),
        Access::Inherit => {
            let line = if codex {
                format!(
                    "inherit: your full Codex environment: {} MCP servers, your sandbox and features",
                    codex_servers()
                )
            } else {
                let (servers, mode) = claude_environment();
                format!(
                    "inherit: your full Claude Code environment: {servers} MCP servers, permission mode {mode}"
                )
            };
            (line, true, true)
        }
    };
    let mut warnings = Vec::new();
    if reads && d.allow.is_empty() && !d.review {
        warnings.push(
            "anything the agent can read may end up posted to the swarm \
             (set agent.allow or agent.review)"
                .to_string(),
        );
    }
    if d.review && shell {
        warnings.push(
            "review is not a boundary here: with a shell, the agent can post through \
             its own `wats mcp` under another profile (only a sandbox makes it hold)"
                .to_string(),
        );
    }
    Summary { line, warnings }
}

/// What gives an agent a capability it declares (`agent.caps`): a tool of
/// its runtime, or an MCP tool or server whose name contains `mcp`.
struct CapNeed {
    /// The caps (tags without `cap:`) it covers.
    caps: &'static [&'static str],
    /// The kind of tool, as the warning names it.
    what: &'static str,
    /// Claude Code tools (`Bash` also covers `Bash(cargo:*)`).
    claude: &'static [&'static str],
    /// Codex features.
    codex: &'static [&'static str],
    /// OpenCode permission keys.
    opencode: &'static [&'static str],
    /// A part of an MCP tool or server name, if any gives it.
    mcp: Option<&'static str>,
}

/// The caps the runner knows how to check. Any other cap is not checked:
/// a cap is an informative declaration, it grants nothing.
const CAP_NEEDS: &[CapNeed] = &[
    CapNeed {
        caps: &["image-gen"],
        what: "image",
        claude: &[],
        codex: &[CODEX_IMAGE_GENERATION],
        opencode: &[],
        mcp: Some("image"),
    },
    CapNeed {
        caps: &["code-exec", "shell"],
        what: "shell",
        claude: &["Bash"],
        codex: &["shell_tool"],
        opencode: &["bash", "shell"],
        mcp: None,
    },
    CapNeed {
        caps: &["web"],
        what: "web",
        claude: &["WebFetch", "WebSearch"],
        codex: &["web_search", "standalone_web_search"],
        opencode: &["webfetch", "websearch"],
        mcp: None,
    },
    CapNeed {
        caps: &["web-search"],
        what: "web search",
        claude: &["WebSearch"],
        codex: &["web_search", "standalone_web_search"],
        opencode: &["websearch"],
        mcp: None,
    },
    CapNeed {
        caps: &["web-fetch"],
        what: "URL fetch",
        claude: &["WebFetch"],
        codex: &[],
        opencode: &["webfetch"],
        mcp: None,
    },
];

/// Inputs of [`cap_warnings`].
pub struct CapCheck<'a> {
    /// The agent as the warning names it.
    pub agent: &'a str,
    pub runtime: &'a str,
    pub access: Access,
    pub workdir: &'a Path,
    pub tools: &'a [String],
    /// `agent.servers`: lent servers give caps as workspace ones do.
    pub servers: &'a [String],
    pub caps: &'a [String],
}

/// One warning per cap of `agent.caps` that no tool of the agent gives,
/// for the caps of [`CAP_NEEDS`]. Never a refusal: the agent still wakes
/// for such asks, and should abstain or clarify. `inherit` has the user's
/// whole environment, and OpenClaw its agent's tool policy: neither is
/// checked.
pub fn cap_warnings(c: &CapCheck) -> Vec<String> {
    if c.access == Access::Inherit || !matches!(c.runtime, "claude" | "codex" | "opencode" | "vibe")
    {
        return Vec::new();
    }
    // swarm-only ignores `agent.tools` and the workspace's MCP servers.
    let (tools, servers): (&[String], Vec<String>) = match c.access {
        Access::Workspace if c.runtime == "codex" => (
            c.tools,
            workspace_codex_overrides(c.workdir)
                .map(|(names, _)| names)
                .unwrap_or_default(),
        ),
        Access::Workspace if c.runtime == "opencode" => (
            c.tools,
            crate::opencode::workspace_servers(c.workdir).unwrap_or_default(),
        ),
        Access::Workspace => (
            c.tools,
            workspace_claude_servers(c.workdir)
                .keys()
                .cloned()
                .collect(),
        ),
        _ => (&[], Vec::new()),
    };
    let mut servers = servers;
    if c.access == Access::Workspace {
        servers.extend(c.servers.iter().cloned());
    }
    let runtime_tools = |need: &CapNeed| -> &'static [&'static str] {
        match c.runtime {
            "codex" => need.codex,
            "opencode" => need.opencode,
            _ => need.claude,
        }
    };
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for cap in c.caps {
        let cap = cap.trim().to_ascii_lowercase();
        let cap = cap.strip_prefix("cap:").unwrap_or(&cap).to_string();
        let Some(need) = CAP_NEEDS.iter().find(|n| n.caps.contains(&cap.as_str())) else {
            continue;
        };
        if seen.contains(&cap) {
            continue;
        }
        seen.push(cap.clone());
        let by_tool = tools.iter().any(|t| {
            let base = t.split('(').next().unwrap_or(t).trim();
            let base = if c.runtime == "opencode" {
                base.to_ascii_lowercase()
            } else {
                base.to_string()
            };
            runtime_tools(need).contains(&base.as_str())
        });
        let by_mcp = need.mcp.is_some_and(|part| {
            tools
                .iter()
                .filter(|t| t.starts_with("mcp__"))
                .chain(servers.iter())
                .any(|name| name.to_ascii_lowercase().contains(part))
        });
        if !by_tool && !by_mcp {
            out.push(format!(
                "agent {} declares cap:{cap} but no {} tool is enabled (agent.tools); \
                 it will wake for such asks and should abstain or clarify",
                c.agent, need.what
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {

    /// `workspace` is the default; `locked`, the old name of `swarm-only`,
    /// still parses, with a deprecation note.
    #[test]
    fn levels_and_the_old_name() {
        assert_eq!(level(None).unwrap(), Access::Workspace);
        assert_eq!(level(Some("locked")).unwrap(), Access::SwarmOnly);
        assert_eq!(level(Some("swarm-only")).unwrap(), Access::SwarmOnly);
        assert_eq!(Access::SwarmOnly.to_string(), "swarm-only");
        assert!(level(Some("open")).is_err());
        assert_eq!(deprecation("locked"), Some(LOCKED_NOTE));
        assert_eq!(deprecation("swarm-only"), None);
        for level in LEVELS {
            assert_eq!(level.parse::<Access>().unwrap().to_string(), level);
        }
    }

    /// A declared cap with no tool to back it is warned about, once; an
    /// unknown cap never is.
    #[test]
    fn caps_without_a_tool_warn() {
        let dir = tempfile::tempdir().unwrap();
        let w = |runtime, access, tools: &[&str], caps: &[&str]| {
            cap_warnings(&CapCheck {
                agent: "x",
                runtime,
                access,
                workdir: dir.path(),
                tools: &tools.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
                servers: &[],
                caps: &caps.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            })
        };
        let none = w("codex", Access::Workspace, &[], &["image-gen", "Image-Gen"]);
        assert_eq!(
            none,
            [
                "agent x declares cap:image-gen but no image tool is enabled (agent.tools); \
              it will wake for such asks and should abstain or clarify"
            ]
        );
        assert!(
            w(
                "codex",
                Access::Workspace,
                &["image_generation"],
                &["image-gen"]
            )
            .is_empty()
        );
        assert!(w("codex", Access::Inherit, &[], &["image-gen", "shell"]).is_empty());
        // swarm-only ignores the tools.
        assert_eq!(
            w(
                "codex",
                Access::SwarmOnly,
                &["image_generation"],
                &["image-gen"]
            )
            .len(),
            1
        );
        // Claude Code: an MCP tool or server whose name says image.
        assert_eq!(
            w("claude", Access::Workspace, &["Read"], &["image-gen"]).len(),
            1
        );
        assert!(
            w(
                "claude",
                Access::Workspace,
                &["mcp__imagegen__generate"],
                &["cap:image-gen"]
            )
            .is_empty()
        );
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers": {"image-tools": {"command": "x"}}}"#,
        )
        .unwrap();
        assert!(w("claude", Access::Workspace, &[], &["image-gen"]).is_empty());
        // Shell and web.
        let shell = w(
            "claude",
            Access::Workspace,
            &["Read"],
            &["code-exec", "web"],
        );
        assert_eq!(shell.len(), 2, "{shell:?}");
        assert!(
            shell[0].contains("cap:code-exec but no shell tool"),
            "{shell:?}"
        );
        assert!(shell[1].contains("cap:web but no web tool"), "{shell:?}");
        assert!(
            w(
                "claude",
                Access::Workspace,
                &["Bash(cargo:*)", "WebSearch"],
                &["code-exec", "web-search"]
            )
            .is_empty()
        );
        assert!(w("codex", Access::Workspace, &["shell_tool"], &["shell"]).is_empty());
        // Unknown caps, and OpenClaw (its own tool policy): no warning.
        assert!(
            w(
                "claude",
                Access::SwarmOnly,
                &[],
                &["pdf", "lang:fr", "review"]
            )
            .is_empty()
        );
        assert!(w("openclaw", Access::SwarmOnly, &[], &["image-gen"]).is_empty());
    }

    #[test]
    fn the_prompt_says_what_a_run_can_use() {
        let dir = Path::new("/w");
        let cannot = "You cannot run, build or test anything in this run.";
        let swarm_only = prompt_line("claude", Access::SwarmOnly, dir, &[]);
        assert!(
            swarm_only.starts_with("Only the wats tools: no shell"),
            "{swarm_only}"
        );
        assert!(swarm_only.ends_with(cannot), "{swarm_only}");
        assert!(prompt_line("openclaw", Access::SwarmOnly, dir, &[]).ends_with(cannot));
        let read_only = prompt_line("claude", Access::Workspace, dir, &["Read".into()]);
        let no_web = "You can search the web: no. You can fetch a URL (read a page or an API): no.";
        assert_eq!(
            read_only,
            format!(
                "The wats tools plus Read in /w, nothing outside it. No shell: {cannot} {no_web}"
            )
        );
        let web = prompt_line("claude", Access::Workspace, dir, &["WebSearch".into()]);
        assert!(
            web.ends_with(
                "You can search the web: yes. You can fetch a URL (read a page or an API): no."
            ),
            "{web}"
        );
        let agy = prompt_line("antigravity", Access::Workspace, dir, &["web-fetch".into()]);
        assert!(agy.contains("use read_url_content"), "{agy}");
        let shell = prompt_line(
            "claude",
            Access::Workspace,
            dir,
            &["Read".into(), "Bash(cargo:*)".into()],
        );
        assert_eq!(
            shell,
            format!("The wats tools plus Read, Bash(cargo:*) in /w, nothing outside it. {no_web}")
        );
        assert!(prompt_line("claude", Access::Workspace, dir, &[]).contains(cannot));
        assert_eq!(
            prompt_line("claude", Access::Inherit, dir, &[]),
            "The tools of your runtime's own configuration."
        );
        // Codex at inherit has its own image generation.
        assert_eq!(
            prompt_line("codex", Access::Inherit, dir, &[]),
            format!("The tools of your runtime's own configuration. {IMAGES_LINE}")
        );
    }

    use super::*;

    /// Lent servers (ADR 0037) join a workspace run's MCP config, written
    /// `0600`; a server `agent.tools` names a tool of is narrowed to it.
    #[test]
    fn claude_runs_get_the_lent_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let lent = vec![
            (
                "crm".to_string(),
                json!({"command": "crm", "env": {"KEY": "s"}}),
            ),
            (
                "docs".to_string(),
                json!({"type": "http", "url": "https://d"}),
            ),
        ];
        let names = write_claude_mcp_config(
            &path,
            Access::Workspace,
            dir.path(),
            &lent,
            Path::new("/bin/wats"),
            &[],
            "token",
        )
        .unwrap();
        assert_eq!(names, ["crm", "docs"]);
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["crm"]["env"]["KEY"], "s");
        assert!(v["mcpServers"]["wats"].is_object());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let swarm_only = write_claude_mcp_config(
            &path,
            Access::SwarmOnly,
            dir.path(),
            &lent,
            Path::new("/bin/wats"),
            &[],
            "token",
        )
        .unwrap();
        assert!(swarm_only.is_empty());

        let tools = vec!["mcp__crm__search".to_string()];
        let args = claude_args(&ClaudeRun {
            access: Access::Workspace,
            mcp_config: &path,
            workspace_servers: &names,
            tools: &tools,
            extra: &[],
            data_dir: Path::new("/data"),
            run_dir: dir.path(),
        });
        let allowed = &args[args.iter().position(|a| a == "--allowedTools").unwrap() + 1];
        assert!(allowed.contains("mcp__docs__*"), "{allowed}");
        assert!(allowed.contains("mcp__crm__search"), "{allowed}");
        assert!(!allowed.contains("mcp__crm__*"), "{allowed}");
    }

    #[test]
    fn claude_levels() {
        let base = |access, tools: &[String], servers: &[String]| {
            claude_args(&ClaudeRun {
                access,
                mcp_config: Path::new("/w/mcp.json"),
                workspace_servers: servers,
                tools,
                extra: &[],
                data_dir: Path::new("/d"),
                run_dir: Path::new("/d/agent/workspace"),
            })
            .join(" ")
        };
        let swarm_only = base(Access::SwarmOnly, &[], &[]);
        assert!(swarm_only.contains("--strict-mcp-config --tools  --permission-mode dontAsk --setting-sources project --settings {\"permissions\":{\"deny\":[\"Edit(//d/config.toml)\""), "{swarm_only}");
        assert!(
            swarm_only.ends_with("--allowedTools mcp__wats__*"),
            "{swarm_only}"
        );
        let ws = base(
            Access::Workspace,
            &[
                "Read".into(),
                "Grep(/srv/docs/**)".into(),
                "WebFetch".into(),
                "mcp__images__generate".into(),
            ],
            &["docs".into()],
        );
        assert!(ws.contains("--tools Read,Grep,WebFetch "), "{ws}");
        assert!(
            ws.contains(
                "--allowedTools mcp__wats__*,mcp__docs__*,Read(./**),Grep(/srv/docs/**),WebFetch,mcp__images__generate"
            ),
            "{ws}"
        );
        let inherit = base(Access::Inherit, &[], &[]);
        assert!(!inherit.contains("--strict-mcp-config"), "{inherit}");
        assert!(!inherit.contains("--tools"), "{inherit}");
        assert!(!inherit.contains("--permission-mode"), "{inherit}");
        assert!(!inherit.contains("--settings"), "{inherit}");
    }

    /// The deny rules of the generated settings, for one run.
    fn deny_of(access: Access, data: &Path, run: &Path) -> Vec<String> {
        let args = claude_args(&ClaudeRun {
            access,
            mcp_config: Path::new("/w/mcp.json"),
            workspace_servers: &[],
            tools: &["Edit".into(), "Bash".into()],
            extra: &[],
            data_dir: data,
            run_dir: run,
        });
        let at = args
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings");
        let settings: Value = serde_json::from_str(&args[at + 1]).unwrap();
        settings["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn claude_denies_the_data_dir_and_wats_commands() {
        let dir = tempfile::tempdir().unwrap();
        let data = std::fs::canonicalize(dir.path()).unwrap().join("d");
        let workspace = data.join("agent").join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(data.join("blobs")).unwrap();
        std::fs::write(data.join("secret.key"), "").unwrap();
        std::fs::write(data.join("agent").join("spend.jsonl"), "").unwrap();
        let d = data.display();
        for (access, run) in [
            (Access::SwarmOnly, data.join("agent").join("locked")),
            (Access::Workspace, workspace.clone()),
        ] {
            let deny = deny_of(access, &data, &run);
            let has = |rule: String| assert!(deny.contains(&rule), "{rule} not in {deny:?}");
            for tool in ["Edit", "Write"] {
                // config.toml and safety.json, even before they exist.
                has(format!("{tool}(/{d}/config.toml)"));
                has(format!("{tool}(/{d}/safety.json)"));
                has(format!("{tool}(/{d}/secret.key)"));
                has(format!("{tool}(/{d}/blobs/**)"));
                // The budget, next to the run directory.
                has(format!("{tool}(/{d}/agent/spend.jsonl)"));
                has(format!("{tool}(/{}/.claude/**)", run.display()));
                has(format!("{tool}(/{}/.mcp.json)", run.display()));
                // The way to the run directory stays open.
                assert!(
                    !deny.contains(&format!("{tool}(/{d}/agent/**)")),
                    "{deny:?}"
                );
                assert!(!deny.contains(&format!("{tool}(/{d}/**)")), "{deny:?}");
            }
            for command in ["config", "resume", "guardian", "setup"] {
                has(format!("Bash(*wats {command}*)"));
            }
            has(format!("Bash(*{d}*)"));
            has("Bash(*config.toml*)".into());
            has("Bash(*..*)".into());
        }
        // A workspace of its own: the whole data directory.
        let elsewhere = dir.path().join("work");
        let deny = deny_of(Access::Workspace, &data, &elsewhere);
        assert!(deny.contains(&format!("Edit(/{d}/**)")), "{deny:?}");
        assert!(deny.contains(&format!("Write(/{d}/**)")), "{deny:?}");
        assert!(!deny.contains(&"Bash(*..*)".to_string()), "{deny:?}");
    }

    #[test]
    fn workdir_never_holds_the_data_dir() {
        let data = Path::new("/home/a/.wats");
        assert!(check_workdir(data, data).is_err());
        assert!(check_workdir(Path::new("/home/a"), data).is_err());
        assert!(check_workdir(Path::new("/"), data).is_err());
        check_workdir(&data.join("agent/workspace"), data).unwrap();
        check_workdir(Path::new("/home/a/agent"), data).unwrap();
    }

    /// Codex cannot deny a path: swarm-only and workspace keep the sandbox
    /// read-only, so no directory, the data directory included, is
    /// writable.
    #[test]
    fn codex_data_dir_is_never_writable() {
        for access in [Access::SwarmOnly, Access::Workspace] {
            let args = codex_args(&CodexRun {
                access,
                wats_command: Path::new("/bin/wats"),
                wats_args: &["--dir".into(), "/d".into(), "mcp".into()],
                runner_token: "t",
                disable: &["shell_tool".into()],
                enable: &["image_generation".into()],
                workspace_overrides: &[],
                extra: &[],
            })
            .join(" ");
            assert!(args.contains("--sandbox read-only"), "{args}");
            assert!(args.contains("--ignore-user-config"), "{args}");
            assert!(!args.contains("workspace-write"), "{args}");
            assert!(!args.contains("danger-full-access"), "{args}");
            assert!(!args.contains("writable_roots"), "{args}");
        }
    }

    /// `agent.tools` with Codex, in workspace: a listed feature the
    /// installed Codex knows is enabled and not disabled, every other
    /// denied feature stays off, the sandbox stays read-only, and a name
    /// Codex does not know is reported. Swarm-only ignores the list.
    #[test]
    fn codex_tools_keep_listed_features_on() {
        let known: Vec<String> = [
            "shell_tool",
            "apps",
            "image_generation",
            "view_image",
            "goals",
        ]
        .map(str::to_string)
        .to_vec();
        let tools: Vec<String> = vec!["image_generation".into(), "Read".into()];
        let features = codex_features_for(Access::Workspace, &tools, &known);
        assert_eq!(features.enable, ["image_generation"]);
        assert_eq!(features.unknown, ["Read"]);
        assert_eq!(
            features.disable,
            ["shell_tool", "apps", "view_image", "goals"]
        );
        let args = codex_args(&CodexRun {
            access: Access::Workspace,
            wats_command: Path::new("/bin/wats"),
            wats_args: &[],
            runner_token: "t",
            disable: &features.disable,
            enable: &features.enable,
            workspace_overrides: &[],
            extra: &[],
        })
        .join(" ");
        assert!(args.contains("--enable image_generation"), "{args}");
        assert!(!args.contains("--disable image_generation"), "{args}");
        for off in ["shell_tool", "apps", "view_image", "goals"] {
            assert!(args.contains(&format!("--disable {off}")), "{args}");
        }
        assert!(args.contains("--sandbox read-only"), "{args}");
        assert!(!args.contains("workspace-write"), "{args}");
        assert!(!args.contains("writable_roots"), "{args}");
        let swarm_only = codex_features_for(Access::SwarmOnly, &tools, &known);
        assert!(swarm_only.enable.is_empty() && swarm_only.unknown.is_empty());
        assert!(swarm_only.disable.iter().any(|f| f == "image_generation"));
        let swarm_only_args = codex_args(&CodexRun {
            access: Access::SwarmOnly,
            wats_command: Path::new("/bin/wats"),
            wats_args: &[],
            runner_token: "t",
            disable: &swarm_only.disable,
            enable: &["image_generation".into()],
            workspace_overrides: &[],
            extra: &[],
        })
        .join(" ");
        assert!(!swarm_only_args.contains("--enable"), "{swarm_only_args}");
        assert!(
            swarm_only_args.contains("--disable image_generation"),
            "{swarm_only_args}"
        );
    }

    /// The image directory of a run is its thread's, the id sanitized as
    /// Codex does: no way out of `generated_images`.
    #[test]
    fn codex_images_dir_is_the_threads_own() {
        let home = Path::new("/h/.codex");
        assert_eq!(
            codex_images_dir(home, "0199aaaa-bbbb_c"),
            Path::new("/h/.codex/generated_images/0199aaaa-bbbb_c")
        );
        assert_eq!(
            codex_images_dir(home, "../../x"),
            Path::new("/h/.codex/generated_images/______x")
        );
        assert_eq!(
            codex_images_dir(home, ""),
            Path::new("/h/.codex/generated_images/generated_image")
        );
        assert!(codex_generates_images(
            "codex",
            Access::Workspace,
            &["image_generation".into()]
        ));
        assert!(codex_generates_images("codex", Access::Inherit, &[]));
        assert!(!codex_generates_images(
            "codex",
            Access::SwarmOnly,
            &["image_generation".into()]
        ));
        assert!(!codex_generates_images(
            "claude",
            Access::Workspace,
            &["image_generation".into()]
        ));
        let line = prompt_line(
            "codex",
            Access::Workspace,
            Path::new("/w"),
            &["image_generation".into()],
        );
        assert!(line.ends_with(IMAGES_LINE), "{line}");
        assert!(
            !prompt_line(
                "codex",
                Access::SwarmOnly,
                Path::new("/w"),
                &["image_generation".into()]
            )
            .contains("blobs")
        );
    }

    #[test]
    fn codex_workspace_servers_become_overrides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
        std::fs::write(
            dir.path().join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = \"docs-mcp\"\nargs = [\"--ro\"]\n[mcp_servers.wats]\ncommand = \"x\"\n",
        )
        .unwrap();
        let (names, overrides) = workspace_codex_overrides(dir.path()).unwrap();
        assert_eq!(names, ["docs"]);
        assert_eq!(
            overrides,
            [
                "mcp_servers.docs.command=\"docs-mcp\"",
                "mcp_servers.docs.args=[\"--ro\"]"
            ]
        );
    }

    #[test]
    fn warnings() {
        let d = |access, review, tools: &[String]| {
            describe(&Describe {
                runtime: "claude",
                access,
                workdir: Path::new("/nonexistent"),
                tools,
                allow: &[],
                review,
            })
            .warnings
        };
        assert!(d(Access::SwarmOnly, false, &[]).is_empty());
        // A workspace with nothing to read has nothing to leak.
        assert!(d(Access::Workspace, false, &[]).is_empty());
        assert_eq!(d(Access::Workspace, false, &["Read".into()]).len(), 1);
        assert!(d(Access::Workspace, true, &["Read".into()]).is_empty());
        assert_eq!(d(Access::Workspace, true, &["Bash".into()]).len(), 1);
        assert_eq!(d(Access::Inherit, true, &[]).len(), 1);
    }
}
