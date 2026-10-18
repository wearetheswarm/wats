//! What the headless agent may use besides the wats tools (`agent.access`,
//! ADR 0011): one knob, three levels, the same for every runtime.
//!
//! - **locked** (default): only the wats tools, in an empty directory.
//! - **workspace**: the agent runs in `agent.workdir`, with that
//!   directory's instructions, skills, settings and MCP servers, plus the
//!   built-in tools listed in `agent.tools`.
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
    #[default]
    Locked,
    Workspace,
    Inherit,
}

pub const LEVELS: [&str; 3] = ["locked", "workspace", "inherit"];

impl std::str::FromStr for Access {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "locked" => Ok(Access::Locked),
            "workspace" => Ok(Access::Workspace),
            "inherit" => Ok(Access::Inherit),
            _ => anyhow::bail!("access is one of: {}", LEVELS.join(", ")),
        }
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Access::Locked => "locked",
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
- Built-in tools are listed in `agent.tools` (`wats config set agent.tools
  Read,Grep`).

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
    /// Where the run starts (the workspace, or the locked directory).
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

/// Claude Code deny rules of a locked or workspace run: no edit nor write
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
    rules.extend(mentions.iter().map(|m| format!("Bash(*{m}*)")));
    // From a workspace inside the data directory, `..` leads to it.
    if run_dir.starts_with(data_dir) {
        rules.push("Bash(*..*)".into());
    }
    rules
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
        Access::Locked => {
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
            allowed.extend(
                run.workspace_servers
                    .iter()
                    .map(|name| format!("mcp__{name}__*")),
            );
            let rules: Vec<(String, String)> = run.tools.iter().map(|t| tool_rule(t)).collect();
            let mut bases: Vec<String> = rules.iter().map(|(b, _)| b.clone()).collect();
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
/// run's token), plus the workspace's servers. Returns their names.
pub fn write_claude_mcp_config(
    path: &Path,
    access: Access,
    workdir: &Path,
    wats_command: &Path,
    wats_args: &[String],
    runner_token: &str,
) -> Result<Vec<String>> {
    let mut servers = if access == Access::Workspace {
        workspace_claude_servers(workdir)
    } else {
        serde_json::Map::new()
    };
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
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({"mcpServers": servers}))?,
    )?;
    Ok(names)
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
    /// Features to disable (locked and workspace).
    pub disable: &'a [String],
    /// `-c` overrides for the workspace's MCP servers.
    pub workspace_overrides: &'a [String],
    pub extra: &'a [String],
}

/// `codex exec` for one run: JSON events, nothing persisted, the wats MCP
/// server with its tools approved and the run's token. Locked and
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
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
        .and_then(|d| std::fs::read_to_string(d.join("config.toml")).ok())
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("mcp_servers")
                .and_then(|s| s.as_table_like())
                .map(|t| t.len())
        })
        .unwrap_or(0)
}

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
    let (line, shell) = match d.access {
        // OpenClaw enforces its agent's tool policy itself (ADR 0017 §3).
        Access::Locked if d.runtime == "openclaw" => (
            "locked: the OpenClaw agent's tool policy (setup recommends allow \
             [\"wats-agent__*\"] only, sandboxed)"
                .to_string(),
            false,
        ),
        Access::Workspace if d.runtime == "openclaw" => (
            "workspace: the OpenClaw agent's tool policy, with the tools you added to it"
                .to_string(),
            false,
        ),
        Access::Inherit if d.runtime == "openclaw" => (
            "inherit: your own OpenClaw agent (agent.openclaw_agent), with all its tools"
                .to_string(),
            true,
        ),
        Access::Locked => ("locked: only the wats tools".to_string(), false),
        Access::Workspace => {
            let servers = if codex {
                workspace_codex_overrides(d.workdir)
                    .map(|(n, _)| n)
                    .unwrap_or_default()
            } else {
                workspace_claude_servers(d.workdir)
                    .keys()
                    .cloned()
                    .collect()
            };
            let shell = d
                .tools
                .iter()
                .any(|t| t == "Bash" || t.starts_with("Bash(") || t == "shell_tool");
            (
                format!(
                    "workspace {}: tools [{}], MCP servers [{}]",
                    d.workdir.display(),
                    d.tools.join(", "),
                    servers.join(", ")
                ),
                shell,
            )
        }
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
            (line, true)
        }
    };
    let mut warnings = Vec::new();
    if d.access != Access::Locked && d.allow.is_empty() && !d.review {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let locked = base(Access::Locked, &[], &[]);
        assert!(locked.contains("--strict-mcp-config --tools  --permission-mode dontAsk --setting-sources project --settings {\"permissions\":{\"deny\":[\"Edit(//d/config.toml)\""), "{locked}");
        assert!(locked.ends_with("--allowedTools mcp__wats__*"), "{locked}");
        let ws = base(
            Access::Workspace,
            &[
                "Read".into(),
                "Grep(/srv/docs/**)".into(),
                "WebFetch".into(),
            ],
            &["docs".into()],
        );
        assert!(ws.contains("--tools Read,Grep,WebFetch"), "{ws}");
        assert!(
            ws.contains(
                "--allowedTools mcp__wats__*,mcp__docs__*,Read(./**),Grep(/srv/docs/**),WebFetch"
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
            (Access::Locked, data.join("agent").join("locked")),
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

    /// Codex cannot deny a path: locked and workspace keep the sandbox
    /// read-only, so no directory, the data directory included, is
    /// writable.
    #[test]
    fn codex_data_dir_is_never_writable() {
        for access in [Access::Locked, Access::Workspace] {
            let args = codex_args(&CodexRun {
                access,
                wats_command: Path::new("/bin/wats"),
                wats_args: &["--dir".into(), "/d".into(), "mcp".into()],
                runner_token: "t",
                disable: &["shell_tool".into()],
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
        assert!(d(Access::Locked, false, &[]).is_empty());
        assert_eq!(d(Access::Workspace, false, &[]).len(), 1);
        assert!(d(Access::Workspace, true, &["Read".into()]).is_empty());
        assert_eq!(d(Access::Workspace, true, &["Bash".into()]).len(), 1);
        assert_eq!(d(Access::Inherit, true, &[]).len(), 1);
    }
}
