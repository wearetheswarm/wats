//! OpenCode as an agent runtime (ADR 0031).
//!
//! Each run spawns `opencode run --format json --agent wats`, with the
//! prompt on stdin, and everything wats decides for the run in its
//! environment:
//!
//! - `OPENCODE_CONFIG_CONTENT`: the per-run config ([`run_config`]): the
//!   `wats` MCP server with the run's runner token, and a dedicated `wats`
//!   agent with `steps` and, at `swarm-only` and `workspace`, a permission
//!   set that denies every known tool and allows `wats_*` last. The same
//!   rules go at the top level, so that a fallback to OpenCode's `build`
//!   agent is locked too.
//! - `XDG_CONFIG_HOME`: the agent's own OpenCode directory
//!   (`<work dir>/opencode-config`), so that the user's global config
//!   (`~/.config/opencode`: MCP servers, plugins, agents, permissions) does
//!   not load. OpenCode's own credential store (`XDG_DATA_HOME`) is left
//!   alone, and wats never reads it.
//! - `OPENCODE_DB=:memory:`: nothing lands in the user's session history.
//!
//! OpenCode's permissions are checks inside its own process, not a
//! sandbox: what they do not cover is listed in ADR 0031 §4.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::access::Access;

/// The runner's MCP server and its tools' prefix (`wats_post`…).
pub const SERVER: &str = "wats";

/// The dedicated OpenCode agent of every run.
pub const AGENT: &str = "wats";

/// The first OpenCode with `opencode mcp add <name> -- <command>`
/// (non-interactive, PR #31054), and the oldest the runner accepts.
pub const MIN_VERSION: Version = Version(1, 17, 0);

/// The major version this integration was checked against (v1.19.3's
/// source). A later one runs, with a warning.
pub const CHECKED_MAJOR: u64 = 1;

/// ADR 0031 open question 1, default pending confirmation: the agent's
/// provider settings live in its own OpenCode directory, filled by the
/// user, and the user's global config never loads at `swarm-only` and
/// `workspace`. `false` would leave `XDG_CONFIG_HOME` alone (the user's
/// MCP servers and plugins would run with every swarm run).
pub const ISOLATE_GLOBAL_CONFIG: bool = true;

/// ADR 0031 open question 10, default pending confirmation: `inherit`
/// needs `agent.allow` and `agent.review`, as for OpenClaw. `false` would
/// only warn, as for Claude Code and Codex.
pub const INHERIT_NEEDS_GUARDRAILS: bool = true;

/// ADR 0031 open question 11, default pending confirmation: the USD
/// budgets are enforced from OpenCode's cost estimate (`step_finish`
/// costs). `false` would count runs only, as for Codex.
pub const ENFORCE_USD_BUDGET: bool = true;

/// The permission keys of OpenCode 1.x (`v1/config/permission.ts`), plus
/// `shell` (2.0's name for `bash`) and `codesearch`: each is set
/// explicitly, since OpenCode merges configs key by key and keeps an
/// earlier key where it was (ADR 0031 §4).
pub const KNOWN_KEYS: [&str; 17] = [
    "read",
    "edit",
    "glob",
    "grep",
    "list",
    "bash",
    "shell",
    "task",
    "skill",
    "lsp",
    "todowrite",
    "webfetch",
    "websearch",
    "codesearch",
    "external_directory",
    "doom_loop",
    "question",
];

/// Arguments that would undo a `swarm-only` or `workspace` run: another
/// agent, auto-approval, another server or directory, an old session.
pub const REFUSED_ARGS: [&str; 12] = [
    "--agent",
    "--auto",
    "--yolo",
    "--dangerously-skip-permissions",
    "--attach",
    "--dir",
    "--continue",
    "-c",
    "--session",
    "-s",
    "--fork",
    "--command",
];

/// Built-in permissions that are no tool of [`KNOWN_KEYS`] but that a
/// workspace MCP server's `<name>_*` rule could match (OpenCode 1.19.3:
/// workflow approval, plan mode): denied after those rules too.
pub const OTHER_BUILTIN_KEYS: [&str; 3] = ["workflow_tool_approval", "plan_enter", "plan_exit"];

/// The `OPENCODE_*` variables a `swarm-only` or `workspace` run keeps
/// from wats's own environment: those that only turn something off, and
/// Windows' Git Bash path. Every other one is removed (it could add
/// config, permissions, tools, or move OpenCode's home or managed config:
/// `OPENCODE_TEST_HOME`, `OPENCODE_TEST_MANAGED_CONFIG_DIR`…); wats then
/// sets its own.
pub const KEPT_ENV_PREFIX: &str = "OPENCODE_DISABLE_";
pub const KEPT_ENV: [&str; 1] = ["OPENCODE_GIT_BASH_PATH"];

/// The agent's own OpenCode config home: `XDG_CONFIG_HOME` of its runs.
pub fn config_home(work_dir: &Path) -> PathBuf {
    work_dir.join("opencode-config")
}

/// The agent's OpenCode directory, where its provider settings go:
/// `<work dir>/opencode-config/opencode`.
pub fn agent_dir(work_dir: &Path) -> PathBuf {
    config_home(work_dir).join("opencode")
}

const AGENT_DIR_STUB: &str = r#"// This node's OpenCode agent (wats, ADR 0031): its model settings.
// Runs read this directory instead of ~/.config/opencode, which never
// loads for them. Put here what the agent's model needs, e.g. a custom
// provider ("provider": {...}) and "model": "provider/model"; see
// https://opencode.ai/docs/providers/. Azure OpenAI, Amazon Bedrock and
// Google Vertex AI need nothing here: your `opencode auth login` and
// their environment variables are used as they are.
// Whatever else you add here (plugins, MCP servers) also runs with every
// swarm run.
{
  "$schema": "https://opencode.ai/config.json"
}
"#;

/// Create the agent's OpenCode directory, with a stub `opencode.jsonc`
/// when it has no config file yet. Returns the directory.
pub fn ensure_agent_dir(work_dir: &Path) -> Result<PathBuf> {
    let dir = agent_dir(work_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    if !["opencode.json", "opencode.jsonc", "config.json"]
        .iter()
        .any(|f| dir.join(f).exists())
    {
        let path = dir.join("opencode.jsonc");
        std::fs::write(&path, AGENT_DIR_STUB)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(dir)
}

/// An OpenCode version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The first `x.y.z` (or `vx.y.z`) in `opencode --version`'s output.
pub fn parse_version(text: &str) -> Option<Version> {
    text.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .find_map(|word| {
            let word = word.strip_prefix('v').unwrap_or(word);
            let mut parts = word.split(['.', '-', '+']);
            let a = parts.next()?.parse().ok()?;
            let b = parts.next()?.parse().ok()?;
            let c = parts.next()?.parse().ok()?;
            Some(Version(a, b, c))
        })
}

/// The runner's check: refused below [`MIN_VERSION`] or without a
/// version; a warning above [`CHECKED_MAJOR`].
pub fn check_version(found: Option<Version>) -> Result<Option<String>> {
    let Some(v) = found else {
        bail!(
            "`opencode --version` gave no version: wats needs OpenCode {MIN_VERSION} or later \
             to lock its agent down"
        );
    };
    if v < MIN_VERSION {
        bail!("OpenCode {v} is too old: wats needs {MIN_VERSION} or later (upgrade OpenCode)");
    }
    Ok((v.0 > CHECKED_MAJOR).then(|| {
        format!(
            "OpenCode {v}: wats was checked against OpenCode {CHECKED_MAJOR}.x only (ADR 0031); \
             its permission and config levers may have changed"
        )
    }))
}

/// `opencode --version`, from the runner (at most 15 s).
pub async fn version(command: &Path) -> Option<Version> {
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(command)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_version(&String::from_utf8_lossy(&out.stderr)))
}

/// `opencode --version`, from setup.
pub fn version_blocking(command: &Path) -> Option<Version> {
    let out = std::process::Command::new(command)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_version(&String::from_utf8_lossy(&out.stderr)))
}

/// Refuse extra arguments that would undo a `swarm-only` or `workspace`
/// run ([`REFUSED_ARGS`]). `inherit` takes any.
pub fn check_extra_args(access: Access, extra: &[String]) -> Result<()> {
    if access == Access::Inherit {
        return Ok(());
    }
    for arg in extra {
        let flag = arg.split('=').next().unwrap_or(arg);
        if REFUSED_ARGS.contains(&flag) {
            bail!(
                "agent.args: {flag} would undo the {access} access level of an OpenCode run \
                 (ADR 0031): remove it, or use access inherit"
            );
        }
    }
    Ok(())
}

/// `inherit` hands swarm posts to the user's whole OpenCode environment,
/// which allows every tool by default: refused without `agent.allow` and
/// `agent.review` ([`INHERIT_NEEDS_GUARDRAILS`]).
pub fn check_access(access: Access, allow: &[String], review: bool) -> Result<()> {
    if INHERIT_NEEDS_GUARDRAILS && access == Access::Inherit && (allow.is_empty() || !review) {
        bail!(
            "agent.access = inherit with OpenCode hands swarm posts to your own OpenCode \
             environment, which allows every tool by default (shell, edits, web): set \
             agent.allow (the authors who may wake it) and agent.review = true first, or use \
             swarm-only or workspace"
        );
    }
    Ok(())
}

/// One entry of `agent.tools`: an OpenCode permission key, with the
/// patterns it allows (`bash(git status*)`; none: every pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub key: String,
    pub patterns: Vec<String>,
}

/// `agent.tools` as OpenCode permission keys: known keys lowercased,
/// `bash` and `shell` the same tool, `key(pattern)` entries of one key
/// merged. `task` is refused (a subagent has OpenCode's default rules, not
/// the wats agent's), and so is a bare `external_directory` (it would let
/// the file tools out of the workspace). Also returns the names that are
/// no OpenCode 1.x key (passed through, with a warning).
pub fn tools(entries: &[String]) -> Result<(Vec<Tool>, Vec<String>)> {
    let mut out: Vec<Tool> = Vec::new();
    let mut unknown = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, pattern) = match entry.split_once('(') {
            Some((name, rest)) => (
                name.trim(),
                Some(rest.strip_suffix(')').unwrap_or(rest).trim().to_string()),
            ),
            None => (entry, None),
        };
        // OpenCode matches a rule's key as a wildcard: `t*sk` would be
        // `task`, `*` everything.
        if name.contains(['*', '?']) {
            bail!(
                "agent.tools: `{name}`: wildcards are refused in OpenCode tool names (a pattern \
                 would also match tools wats denies, such as task); list each tool"
            );
        }
        let lower = name.to_ascii_lowercase();
        let key = if KNOWN_KEYS.contains(&lower.as_str()) {
            if lower == "shell" {
                "bash".to_string()
            } else {
                lower
            }
        } else {
            unknown.push(name.to_string());
            name.to_string()
        };
        if key == "task" {
            bail!(
                "agent.tools: `task` is refused for OpenCode: a subagent runs with OpenCode's \
                 default rules (a shell included), not the wats agent's (ADR 0031)"
            );
        }
        if key == "external_directory" {
            bail!(
                "agent.tools: `external_directory` is refused for OpenCode: the agent's file \
                 tools stay in its workspace (put what it may read there, or use inherit)"
            );
        }
        let pattern = pattern.filter(|p| !p.is_empty());
        match out.iter_mut().find(|t| t.key == key) {
            Some(tool) => {
                // A bare entry allows every pattern.
                if let Some(p) = pattern {
                    if !tool.patterns.is_empty() && !tool.patterns.contains(&p) {
                        tool.patterns.push(p);
                    }
                } else {
                    tool.patterns.clear();
                }
            }
            None => out.push(Tool {
                key,
                patterns: pattern.into_iter().collect(),
            }),
        }
    }
    Ok((out, unknown))
}

/// `value` as a JSON string literal.
fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

/// A JSON object from `(key, raw JSON value)` pairs, in this order (a
/// later duplicate key is dropped). OpenCode matches permission rules in
/// the order of the file, the last match winning, so the order is part of
/// the meaning: no sorted map.
fn object(pairs: &[(String, String)]) -> String {
    let mut seen = std::collections::HashSet::new();
    let body: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| seen.insert(k.clone()))
        .map(|(k, v)| format!("{}:{v}", quote(k)))
        .collect();
    format!("{{{}}}", body.join(","))
}

const ALLOW: &str = "\"allow\"";
const DENY: &str = "\"deny\"";

/// MCP tool names as OpenCode builds them (`mcp/catalog.ts`): characters
/// other than `[A-Za-z0-9_-]` become `_`.
pub fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Inputs of one run's permission set.
pub struct Permissions<'a> {
    pub access: Access,
    /// `agent.tools` ([`tools`]), used at `workspace` only.
    pub tools: &'a [Tool],
    /// The workspace's MCP servers, allowed at `workspace` only.
    pub servers: &'a [String],
    /// The node's data directory, which `bash` may not name.
    pub data_dir: &'a Path,
    /// Where the run starts.
    pub run_dir: &'a Path,
}

/// The permission object of a `swarm-only` or `workspace` run: `"*"`
/// denied; the workspace's MCP servers (`<name>_*`, `workspace`); then
/// every [`KNOWN_KEYS`] denied unless `agent.tools` lists it, and the
/// [`OTHER_BUILTIN_KEYS`] denied, after the servers (a server named
/// `external` would otherwise allow `external_directory`, the last match
/// winning); the other listed names; and `wats_*` allowed last. `None`
/// at `inherit`: the user's own rules apply.
pub fn permission(p: &Permissions) -> Option<String> {
    if p.access == Access::Inherit {
        return None;
    }
    let listed: &[Tool] = if p.access == Access::Workspace {
        p.tools
    } else {
        &[]
    };
    let find = |key: &str| {
        let key = if key == "shell" { "bash" } else { key };
        listed.iter().find(|t| t.key == key)
    };
    let mut pairs: Vec<(String, String)> = vec![("*".into(), DENY.into())];
    if p.access == Access::Workspace {
        for server in p.servers {
            pairs.push((format!("{}_*", sanitize(server)), ALLOW.into()));
        }
    }
    for key in KNOWN_KEYS {
        let rule = match find(key) {
            Some(tool) => rule(key, tool, p),
            None => DENY.to_string(),
        };
        pairs.push((key.to_string(), rule));
    }
    for key in OTHER_BUILTIN_KEYS {
        pairs.push((key.to_string(), DENY.into()));
    }
    for tool in listed {
        if !KNOWN_KEYS.contains(&tool.key.as_str()) {
            pairs.push((tool.key.clone(), rule(&tool.key, tool, p)));
        }
    }
    // Last: it must win over "*".
    pairs.retain(|(k, _)| k != &format!("{SERVER}_*"));
    pairs.push((format!("{SERVER}_*"), ALLOW.into()));
    Some(object(&pairs))
}

/// The rule of one listed tool: `allow`, or its patterns allowed; `bash`
/// then denies what [`crate::access::shell_deny_mentions`] lists, and
/// `edit` the workspace's OpenCode config (plugins and MCP servers there
/// run code).
fn rule(key: &str, tool: &Tool, p: &Permissions) -> String {
    let allowed: Vec<String> = if tool.patterns.is_empty() {
        vec!["*".into()]
    } else {
        tool.patterns.clone()
    };
    let mut pairs: Vec<(String, String)> = allowed
        .iter()
        .map(|pattern| (pattern.clone(), ALLOW.to_string()))
        .collect();
    match key {
        "bash" | "shell" => {
            for mention in crate::access::shell_deny_mentions(p.data_dir) {
                pairs.push((format!("*{mention}*"), DENY.into()));
            }
            if p.run_dir.starts_with(p.data_dir) {
                pairs.push(("*..*".into(), DENY.into()));
            }
        }
        "edit" => {
            for pattern in ["*opencode.json*", "*.opencode/*"] {
                pairs.push((pattern.into(), DENY.into()));
            }
        }
        _ => {
            if tool.patterns.is_empty() {
                return ALLOW.into();
            }
        }
    }
    object(&pairs)
}

/// Inputs of one run's config.
pub struct RunConfig<'a> {
    pub access: Access,
    /// The `wats mcp` command line of the run.
    pub wats_command: &'a Path,
    pub wats_args: &'a [String],
    pub runner_token: &'a str,
    /// The run's timeout: also the MCP server's, so that a long `wait`
    /// is not cut at the MCP SDK's 60 s.
    pub timeout: Duration,
    /// `agent.max_turns`: the agent's `steps`.
    pub steps: u32,
    /// [`permission`], `None` at `inherit`.
    pub permission: Option<String>,
}

/// The per-run config (`OPENCODE_CONFIG_CONTENT`, ADR 0031 §4): no
/// autoupdate, the `wats` MCP server with the run's runner token, the
/// dedicated `wats` agent, and at `swarm-only` and `workspace` no
/// snapshots and the same permission set at the top level and on the
/// agent.
pub fn run_config(c: &RunConfig) -> String {
    let mut command = vec![quote(&c.wats_command.to_string_lossy())];
    command.extend(c.wats_args.iter().map(|a| quote(a)));
    let timeout_ms = c.timeout.as_millis().clamp(30_000, i32::MAX as u128);
    let server = object(&[
        ("type".into(), quote("local")),
        ("command".into(), format!("[{}]", command.join(","))),
        (
            "environment".into(),
            object(&[("WATS_RUNNER_TOKEN".into(), quote(c.runner_token))]),
        ),
        ("enabled".into(), "true".into()),
        ("timeout".into(), timeout_ms.to_string()),
    ]);
    let mut agent = vec![
        (
            "description".into(),
            quote("This node's WeAreTheSwarm agent (wats)"),
        ),
        ("mode".into(), quote("primary")),
        ("steps".into(), c.steps.max(1).to_string()),
    ];
    let mut top = vec![
        ("$schema".into(), quote("https://opencode.ai/config.json")),
        ("autoupdate".into(), "false".into()),
    ];
    if let Some(permission) = &c.permission {
        agent.push(("permission".into(), permission.clone()));
        top.push(("snapshot".into(), "false".into()));
        top.push(("permission".into(), permission.clone()));
    }
    top.push(("default_agent".into(), quote(AGENT)));
    top.push(("mcp".into(), object(&[(SERVER.into(), server)])));
    top.push(("agent".into(), object(&[(AGENT.into(), object(&agent))])));
    object(&top)
}

/// `opencode run` arguments, before the model and the extra arguments.
pub fn run_args() -> Vec<String> {
    ["run", "--format", "json", "--agent", AGENT]
        .map(str::to_string)
        .to_vec()
}

/// The environment one run sets (ADR 0031 §5).
pub fn run_env(access: Access, work_dir: &Path, config: &str) -> Vec<(&'static str, OsString)> {
    let mut env: Vec<(&'static str, OsString)> = vec![
        ("OPENCODE_CONFIG_CONTENT", config.into()),
        ("OPENCODE_DB", ":memory:".into()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".into()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1".into()),
    ];
    if access != Access::Inherit {
        if ISOLATE_GLOBAL_CONFIG {
            env.push(("XDG_CONFIG_HOME", config_home(work_dir).into_os_string()));
        }
        env.push(("OPENCODE_DISABLE_CLAUDE_CODE", "1".into()));
    }
    if access == Access::SwarmOnly {
        env.push(("OPENCODE_DISABLE_PROJECT_CONFIG", "1".into()));
    }
    env
}

/// The variables of `vars` a `swarm-only` or `workspace` run removes:
/// every `OPENCODE_*` but [`KEPT_ENV_PREFIX`] and [`KEPT_ENV`].
pub fn removed_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    vars.map(|(k, _)| k)
        .filter(|k| {
            let k = k.to_string_lossy();
            k.starts_with("OPENCODE_")
                && !k.starts_with(KEPT_ENV_PREFIX)
                && !KEPT_ENV.contains(&k.as_ref())
        })
        .collect()
}

/// Check `agent.model` for OpenCode: `provider/model`, passed as
/// `--model=<value>`.
pub fn check_model(model: &str) -> Result<()> {
    let ok = !model.starts_with('-')
        && !model.chars().any(char::is_whitespace)
        && model
            .split_once('/')
            .is_some_and(|(provider, name)| !provider.is_empty() && !name.is_empty());
    if !ok {
        bail!("agent.model {model:?}: OpenCode takes provider/model, e.g. azure/gpt-5");
    }
    Ok(())
}

/// The agent definitions OpenCode merges into an agent's config, by key
/// (`mode` is merged into `agent` after `OPENCODE_CONFIG_CONTENT`;
/// `agents` is the v2 spelling).
const AGENT_KEYS: [&str; 3] = ["agent", "agents", "mode"];

/// The directories of markdown agent files, in a config directory.
const AGENT_DIRS: [&str; 4] = ["agent", "agents", "mode", "modes"];

/// Whether a permission rule value allows anything (`"allow"`, or an
/// object with an `"allow"`).
fn allows(value: &Value) -> bool {
    match value {
        Value::String(s) => s == "allow",
        Value::Object(map) => map.values().any(|v| v == "allow"),
        _ => false,
    }
}

/// A rule of a config's `permission` (or legacy `tools`) block that
/// allows by wildcard: OpenCode keeps such a key where the config put it,
/// so it can come after wats's denies and win. `None`: none.
fn wildcard_allow(config: &Value) -> Option<String> {
    let wild = |k: &str| k.contains(['*', '?']);
    match &config["permission"] {
        Value::String(s) if s == "allow" => return Some("permission = \"allow\"".into()),
        Value::Object(map) => {
            if let Some((k, _)) = map.iter().find(|(k, v)| wild(k) && allows(v)) {
                return Some(format!("permission.{k}"));
            }
        }
        _ => {}
    }
    config["tools"]
        .as_object()?
        .iter()
        .find(|(k, v)| wild(k) && **v == Value::Bool(true))
        .map(|(k, _)| format!("tools.{k}"))
}

/// Refuse a config that could change the run's agent or out-order its
/// rules: an `agent`, `agents` or `mode` entry named `wats`, or a
/// wildcard allow in `permission` or `tools`.
fn check_config_file(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let config: Value = serde_json::from_str(&strip_jsonc(&text))
        .with_context(|| format!("{}: not valid JSON(C)", path.display()))?;
    for key in AGENT_KEYS {
        if config[key].get(AGENT).is_some() {
            bail!(
                "{}: defines {key}.{AGENT}, which OpenCode would merge into the agent wats gives \
                 every run: rename it (ADR 0031)",
                path.display()
            );
        }
    }
    if let Some(rule) = wildcard_allow(&config) {
        bail!(
            "{}: {rule} allows by wildcard, which could win over the rules wats gives an \
             OpenCode run: list the tools it allows instead (ADR 0031)",
            path.display()
        );
    }
    Ok(())
}

/// Refuse a markdown agent named `wats` in a config directory
/// (`<dir>/{agent,agents,mode,modes}/wats.md`).
fn check_agent_files(dir: &Path) -> Result<()> {
    for sub in AGENT_DIRS {
        let path = dir.join(sub).join(format!("{AGENT}.md"));
        if path.exists() {
            bail!(
                "{} defines an OpenCode agent named {AGENT}, the agent wats gives every run: \
                 rename it (ADR 0031)",
                path.display()
            );
        }
    }
    Ok(())
}

/// Before every run (ADR 0031 §4): the config sources an OpenCode run
/// reads and wats can check. The agent's own OpenCode directory at
/// `swarm-only` and `workspace`; at `workspace` also the project config
/// of the run directory and of every directory above it, as OpenCode
/// walks them outside a git repository (`opencode.json(c)` and
/// `.opencode/`). `~/.opencode/` is not read (a documented gap, ADR 0031
/// §4); `inherit` checks nothing.
pub fn check_config_sources(
    access: Access,
    run_dir: &Path,
    work_dir: &Path,
    home: Option<&Path>,
) -> Result<()> {
    if access == Access::Inherit {
        return Ok(());
    }
    if ISOLATE_GLOBAL_CONFIG {
        let dir = agent_dir(work_dir);
        for name in ["config.json", "opencode.json", "opencode.jsonc"] {
            let path = dir.join(name);
            if path.exists() {
                check_config_file(&path)?;
            }
        }
        check_agent_files(&dir)?;
    }
    if access != Access::Workspace {
        return Ok(());
    }
    for dir in run_dir.ancestors() {
        for name in ["opencode.json", "opencode.jsonc"] {
            let path = dir.join(name);
            if path.exists() {
                check_config_file(&path)?;
            }
        }
        if Some(dir) == home {
            continue;
        }
        let dot = dir.join(".opencode");
        for name in ["opencode.json", "opencode.jsonc"] {
            let path = dot.join(name);
            if path.exists() {
                check_config_file(&path)?;
            }
        }
        check_agent_files(&dot)?;
    }
    Ok(())
}

/// JSONC as plain JSON: comments and trailing commas removed, strings
/// left as they are.
pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            (',', _) => {
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The workspace's OpenCode config files that exist: `opencode.json(c)`
/// and `.opencode/opencode.json(c)`.
pub fn workspace_files(workdir: &Path) -> Vec<PathBuf> {
    [
        workdir.join("opencode.json"),
        workdir.join("opencode.jsonc"),
        workdir.join(".opencode").join("opencode.json"),
        workdir.join(".opencode").join("opencode.jsonc"),
    ]
    .into_iter()
    .filter(|p| p.exists())
    .collect()
}

/// The MCP servers of the workspace's OpenCode config (their tools are
/// allowed at `workspace`). Refuses a config that is not JSONC; what else
/// a config may not hold is [`check_config_sources`]'.
pub fn workspace_servers(workdir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for path in workspace_files(workdir) {
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: Value = serde_json::from_str(&strip_jsonc(&text))
            .with_context(|| format!("{}: not valid JSON(C)", path.display()))?;
        for name in config["mcp"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            if name != SERVER && !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    Ok(names)
}

/// What a run loads that wats does not control (ADR 0031 §4), as start
/// warnings: `~/.opencode/` at every level, and at `workspace` the
/// OpenCode config of the run directory's ancestors. Existence checks
/// only: nothing is read.
pub fn start_warnings(access: Access, run_dir: &Path, home: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(home) = home
        && home.join(".opencode").exists()
    {
        out.push(format!(
            "{} exists: OpenCode loads it for every run (its config, agents and plugins), \
             whatever the access level",
            home.join(".opencode").display()
        ));
    }
    if access == Access::Workspace {
        for dir in run_dir.ancestors().skip(1) {
            for name in ["opencode.json", "opencode.jsonc", ".opencode"] {
                let path = dir.join(name);
                if path.exists() && Some(dir) != home.filter(|_| name == ".opencode") {
                    out.push(format!(
                        "{} exists above the agent's workspace: OpenCode loads it as project \
                         config for workspace runs",
                        path.display()
                    ));
                }
            }
        }
    }
    out
}

/// The global config file `opencode mcp add` writes, as OpenCode picks
/// it: `opencode.json`, or `opencode.jsonc` when only that one exists,
/// under `$XDG_CONFIG_HOME/opencode` (else `~/.config/opencode`).
pub fn global_config_file(
    xdg_config_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = match xdg_config_home.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home?.join(".config"),
    };
    let dir = base.join("opencode");
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    Some(if !json.exists() && jsonc.exists() {
        jsonc
    } else {
        json
    })
}

/// The block to add by hand to OpenCode's global config, when its
/// `mcp add` cannot (older than [`MIN_VERSION`]).
pub fn paste_block(command: &str, args: &[String]) -> String {
    let mut all = vec![quote(command)];
    all.extend(args.iter().map(|a| quote(a)));
    format!(
        "{{\n  \"mcp\": {{\n    \"{SERVER}\": {{\n      \"type\": \"local\",\n      \"command\": [{}]\n    }}\n  }}\n}}",
        all.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// The keys of a JSON object, in the order of the text.
    fn keys_in_order(json: &str) -> Vec<String> {
        // serde_json without preserve_order sorts: walk the text instead.
        let v: Value = serde_json::from_str(json).unwrap();
        let mut keys: Vec<(usize, String)> = v
            .as_object()
            .unwrap()
            .keys()
            .map(|k| (json.find(&format!("{}:", quote(k))).unwrap(), k.clone()))
            .collect();
        keys.sort();
        keys.into_iter().map(|(_, k)| k).collect()
    }

    fn perms(access: Access, tools: &[Tool], servers: &[String]) -> Option<String> {
        permission(&Permissions {
            access,
            tools,
            servers,
            data_dir: Path::new("/data"),
            run_dir: Path::new("/data/agent/opencode/workspace"),
        })
    }

    #[test]
    fn versions_are_read_and_checked() {
        assert_eq!(parse_version("1.19.3\n"), Some(Version(1, 19, 3)));
        assert_eq!(parse_version("opencode v1.17.0"), Some(Version(1, 17, 0)));
        assert_eq!(parse_version("2.0.0-beta.1"), Some(Version(2, 0, 0)));
        assert_eq!(parse_version("nothing"), None);
        assert!(check_version(None).is_err());
        assert!(check_version(Some(Version(1, 16, 9))).is_err());
        assert_eq!(check_version(Some(Version(1, 17, 0))).unwrap(), None);
        assert!(check_version(Some(Version(2, 0, 0))).unwrap().is_some());
    }

    #[test]
    fn swarm_only_denies_everything_but_wats_last() {
        let json = perms(Access::SwarmOnly, &[], &["docs".into()]).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        let keys = keys_in_order(&json);
        assert_eq!(keys.first().map(String::as_str), Some("*"));
        assert_eq!(keys.last().map(String::as_str), Some("wats_*"));
        for key in KNOWN_KEYS {
            assert_eq!(v[key], "deny", "{key}");
        }
        assert_eq!(v["wats_*"], "allow");
        // swarm-only ignores the workspace's servers and agent.tools.
        assert!(v.get("docs_*").is_none(), "{json}");
        let (tools, _) = tools(&s(&["read"])).unwrap();
        let json = perms(Access::SwarmOnly, &tools, &[]).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap()["read"],
            "deny"
        );
        assert_eq!(perms(Access::Inherit, &tools, &[]), None);
    }

    #[test]
    fn workspace_opens_what_is_listed() {
        let (tools, unknown) = tools(&s(&[
            "Read",
            "bash(git status*)",
            "bash(cargo *)",
            "edit",
            "docs_search",
        ]))
        .unwrap();
        assert_eq!(unknown, ["docs_search"]);
        let json = perms(Access::Workspace, &tools, &["my docs".into()]).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["read"], "allow");
        assert_eq!(v["grep"], "deny");
        assert_eq!(v["task"], "deny");
        assert_eq!(v["docs_search"], "allow");
        assert_eq!(v["my_docs_*"], "allow");
        assert_eq!(v["wats_*"], "allow");
        assert_eq!(
            keys_in_order(&json).last().map(String::as_str),
            Some("wats_*")
        );
        // bash: the listed patterns, then the denies; shell the same.
        let bash = v["bash"].as_object().unwrap();
        assert_eq!(bash["git status*"], "allow");
        assert_eq!(bash["cargo *"], "allow");
        assert_eq!(bash["*wats config*"], "deny");
        assert_eq!(bash["*config.toml*"], "deny");
        assert_eq!(bash["*/data*"], "deny");
        assert_eq!(bash["*..*"], "deny");
        assert_eq!(v["shell"], v["bash"]);
        let bash_json = json.split("\"bash\":").nth(1).unwrap();
        assert!(
            bash_json.find("git status*") < bash_json.find("*wats config*"),
            "denies come after the allows: {bash_json}"
        );
        let edit = v["edit"].as_object().unwrap();
        assert_eq!(edit["*"], "allow");
        assert_eq!(edit["*opencode.json*"], "deny");
        assert_eq!(edit["*.opencode/*"], "deny");
        assert_eq!(v["external_directory"], "deny");
    }

    /// A workspace server's `<name>_*` rule comes before the built-in
    /// denies: a server named `external` (or `doom`, `workflow`) cannot
    /// allow `external_directory`, the last match winning.
    #[test]
    fn server_rules_never_open_built_in_keys() {
        let json = perms(
            Access::Workspace,
            &[],
            &["external".into(), "doom".into(), "workflow".into()],
        )
        .unwrap();
        let keys = keys_in_order(&json);
        let at = |k: &str| keys.iter().position(|x| x == k).unwrap();
        for (server, builtin) in [
            ("external_*", "external_directory"),
            ("doom_*", "doom_loop"),
            ("workflow_*", "workflow_tool_approval"),
        ] {
            assert!(
                at(server) < at(builtin),
                "{server} before {builtin}: {json}"
            );
            let v: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v[builtin], "deny");
            assert_eq!(v[server], "allow");
        }
        // As OpenCode evaluates: the last rule whose key matches wins.
        let last = |tool: &str| {
            keys.iter()
                .rev()
                .find(|k| wildcard_match(tool, k))
                .cloned()
                .unwrap()
        };
        assert_eq!(last("external_directory"), "external_directory");
        assert_eq!(last("external_search"), "external_*");
        assert_eq!(last("wats_post"), "wats_*");
        assert_eq!(last("bash"), "bash");
    }

    /// OpenCode's wildcard: `*` any run of characters, `?` one.
    fn wildcard_match(text: &str, pattern: &str) -> bool {
        fn go(t: &[char], p: &[char]) -> bool {
            match p.split_first() {
                None => t.is_empty(),
                Some(('*', rest)) => (0..=t.len()).any(|i| go(&t[i..], rest)),
                Some(('?', rest)) => !t.is_empty() && go(&t[1..], rest),
                Some((c, rest)) => t.first() == Some(c) && go(&t[1..], rest),
            }
        }
        go(
            &text.chars().collect::<Vec<_>>(),
            &pattern.chars().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn task_external_directory_and_wildcards_are_refused() {
        assert!(tools(&s(&["task"])).is_err());
        assert!(tools(&s(&["Task(general)"])).is_err());
        assert!(tools(&s(&["external_directory"])).is_err());
        assert!(tools(&s(&["external_directory(/srv/docs/**)"])).is_err());
        // A wildcard in a name would match task or external_directory.
        for name in ["t*sk", "ta?k", "*", "ext*(/srv/**)", "Task*"] {
            assert!(tools(&s(&[name])).is_err(), "{name}");
        }
        // Wildcards in a pattern are fine.
        assert!(tools(&s(&["bash(git *)"])).is_ok());
        // A bare entry wins over patterns of the same key.
        let (t, _) = tools(&s(&["bash(ls)", "shell"])).unwrap();
        assert_eq!(
            t,
            [Tool {
                key: "bash".into(),
                patterns: vec![]
            }]
        );
    }

    #[test]
    fn the_run_config_locks_the_agent_and_the_fallback() {
        let permission = perms(Access::SwarmOnly, &[], &[]);
        let json = run_config(&RunConfig {
            access: Access::SwarmOnly,
            wats_command: Path::new("/opt/wats"),
            wats_args: &s(&["--dir", "/d", "mcp", "--profile", "agent"]),
            runner_token: "tok",
            timeout: Duration::from_secs(600),
            steps: 7,
            permission: permission.clone(),
        });
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["autoupdate"], false);
        assert_eq!(v["snapshot"], false);
        assert_eq!(v["default_agent"], "wats");
        let server = &v["mcp"]["wats"];
        assert_eq!(server["type"], "local");
        assert_eq!(
            server["command"],
            serde_json::json!(["/opt/wats", "--dir", "/d", "mcp", "--profile", "agent"])
        );
        assert_eq!(server["environment"]["WATS_RUNNER_TOKEN"], "tok");
        assert_eq!(server["timeout"], 600_000);
        let agent = &v["agent"]["wats"];
        assert_eq!(agent["mode"], "primary");
        assert_eq!(agent["steps"], 7);
        let expected: Value = serde_json::from_str(permission.as_deref().unwrap()).unwrap();
        assert_eq!(agent["permission"], expected);
        assert_eq!(v["permission"], expected);

        let inherit = run_config(&RunConfig {
            access: Access::Inherit,
            wats_command: Path::new("/opt/wats"),
            wats_args: &[],
            runner_token: "tok",
            timeout: Duration::from_secs(1),
            steps: 0,
            permission: None,
        });
        let v: Value = serde_json::from_str(&inherit).unwrap();
        assert!(v.get("permission").is_none(), "{inherit}");
        assert!(v["agent"]["wats"].get("permission").is_none());
        assert!(v.get("snapshot").is_none());
        assert_eq!(v["agent"]["wats"]["steps"], 1);
        assert_eq!(v["mcp"]["wats"]["timeout"], 30_000);
    }

    #[test]
    fn arguments_that_undo_the_level_are_refused() {
        for bad in [
            "--agent",
            "--agent=build",
            "--auto",
            "--dangerously-skip-permissions",
            "--attach",
            "-c",
            "--session=abc",
            "--command",
        ] {
            assert!(
                check_extra_args(Access::SwarmOnly, &s(&[bad])).is_err(),
                "{bad}"
            );
            assert!(check_extra_args(Access::Inherit, &s(&[bad])).is_ok());
        }
        assert!(
            check_extra_args(Access::Workspace, &s(&["--model", "a/b", "--variant=high"])).is_ok()
        );
    }

    #[test]
    fn inherit_needs_allow_and_review() {
        let allow = s(&["ab"]);
        assert!(check_access(Access::Inherit, &[], true).is_err());
        assert!(check_access(Access::Inherit, &allow, false).is_err());
        assert!(check_access(Access::Inherit, &allow, true).is_ok());
        assert!(check_access(Access::SwarmOnly, &[], false).is_ok());
    }

    #[test]
    fn environment_per_level() {
        let work = Path::new("/w");
        let get = |access| {
            run_env(access, work, "{}")
                .into_iter()
                .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let swarm = get(Access::SwarmOnly);
        assert_eq!(swarm["XDG_CONFIG_HOME"], "/w/opencode-config");
        assert_eq!(swarm["OPENCODE_DB"], ":memory:");
        assert_eq!(swarm["OPENCODE_DISABLE_PROJECT_CONFIG"], "1");
        assert_eq!(swarm["OPENCODE_DISABLE_CLAUDE_CODE"], "1");
        assert_eq!(swarm["OPENCODE_DISABLE_AUTOUPDATE"], "1");
        let workspace = get(Access::Workspace);
        assert!(workspace.contains_key("XDG_CONFIG_HOME"));
        assert!(!workspace.contains_key("OPENCODE_DISABLE_PROJECT_CONFIG"));
        let inherit = get(Access::Inherit);
        assert!(!inherit.contains_key("XDG_CONFIG_HOME"));
        assert_eq!(inherit["OPENCODE_DB"], ":memory:");
        let removed = removed_env(
            [
                ("OPENCODE_CONFIG", "x"),
                ("OPENCODE_EXPERIMENTAL_LSP_TOOL", "1"),
                ("OPENCODE_TEST_HOME", "/elsewhere"),
                ("OPENCODE_TEST_MANAGED_CONFIG_DIR", "/m"),
                ("OPENCODE_ENABLE_EXA", "1"),
                ("OPENCODE_DISABLE_PRUNE", "1"),
                ("OPENCODE_GIT_BASH_PATH", "C:/git/bash.exe"),
                ("PATH", "/bin"),
            ]
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        assert_eq!(
            removed,
            [
                "OPENCODE_CONFIG",
                "OPENCODE_EXPERIMENTAL_LSP_TOOL",
                "OPENCODE_TEST_HOME",
                "OPENCODE_TEST_MANAGED_CONFIG_DIR",
                "OPENCODE_ENABLE_EXA",
            ]
        );
    }

    #[test]
    fn models_are_provider_slash_model() {
        assert!(check_model("azure/gpt-5").is_ok());
        assert!(check_model("amazon-bedrock/anthropic.claude-x").is_ok());
        for bad in ["gpt-5", "-x/y", "--agent=build", "a/", "/b", "a /b", ""] {
            assert!(check_model(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn jsonc_and_the_workspace_config() {
        let text = "{\n  // servers\n  \"mcp\": { \"docs\": {\"type\": \"local\", \"command\": [\"x // y\"]}, /* c */ \"wats\": {},},\n}\n";
        let v: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(v["mcp"]["docs"]["command"][0], "x // y");
        let dir = tempfile::tempdir().unwrap();
        assert!(workspace_servers(dir.path()).unwrap().is_empty());
        std::fs::write(dir.path().join("opencode.jsonc"), text).unwrap();
        assert_eq!(workspace_servers(dir.path()).unwrap(), ["docs"]);
        std::fs::create_dir_all(dir.path().join(".opencode")).unwrap();
        std::fs::write(dir.path().join(".opencode/opencode.json"), "{oops").unwrap();
        assert!(workspace_servers(dir.path()).is_err());
    }

    /// What could change the run's agent or out-order its rules is
    /// refused, in the workspace, above it, and in the agent's own
    /// directory: `agent.wats`, `agents.wats`, `mode.wats`, a markdown
    /// agent `wats.md`, a wildcard allow in `permission` or `tools`.
    #[test]
    fn config_sources_that_could_change_the_agent_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("data/agent/opencode");
        let workspace = work.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        ensure_agent_dir(&work).unwrap();
        let check = |access| check_config_sources(access, &workspace, &work, Some(&home));
        assert!(check(Access::Workspace).is_ok());
        let refused = |path: &Path, text: &str, what: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            let e = check(Access::Workspace).unwrap_err();
            assert!(format!("{e:#}").contains(what), "{what}: {e:#}");
            std::fs::remove_file(path).unwrap();
            assert!(check(Access::Workspace).is_ok(), "{what}");
        };
        let file = workspace.join("opencode.json");
        refused(&file, r#"{"agent": {"wats": {}}}"#, "agent.wats");
        refused(
            &file,
            r#"{"agents": {"wats": {"disabled": true}}}"#,
            "agents.wats",
        );
        refused(
            &file,
            r#"{"mode": {"wats": {"permission": {"bash": "allow"}}}}"#,
            "mode.wats",
        );
        refused(&file, r#"{"permission": {"b*": "allow"}}"#, "permission.b*");
        refused(
            &file,
            r#"{"permission": {"*": {"x": "allow"}}}"#,
            "permission.*",
        );
        refused(
            &file,
            r#"{"permission": "allow"}"#,
            "permission = \"allow\"",
        );
        refused(&file, r#"{"tools": {"ba?h": true}}"#, "tools.ba?h");
        // A specific allow is replaced by wats's own rule: fine.
        std::fs::write(&file, r#"{"permission": {"bash": "allow", "b*": "deny"}}"#).unwrap();
        assert!(check(Access::Workspace).is_ok());
        std::fs::remove_file(&file).unwrap();
        for sub in ["agent", "agents", "mode", "modes"] {
            refused(
                &workspace.join(".opencode").join(sub).join("wats.md"),
                "x",
                "wats.md",
            );
        }
        // Above the workspace, as OpenCode walks up outside a git repo.
        refused(
            &root.path().join("opencode.jsonc"),
            r#"{"mode": {"wats": {}}}"#,
            "mode.wats",
        );
        refused(
            &root.path().join("data/.opencode/agent/wats.md"),
            "x",
            "wats.md",
        );
        // The agent's own OpenCode directory, at every isolated level.
        let own = agent_dir(&work).join("opencode.jsonc");
        std::fs::write(&own, r#"{"agent": {"wats": {}}}"#).unwrap();
        assert!(check(Access::SwarmOnly).is_err());
        assert!(check(Access::Inherit).is_ok());
        std::fs::remove_file(&own).unwrap();
        // swarm-only reads no project config: the workspace is not looked at.
        std::fs::write(&file, r#"{"mode": {"wats": {}}}"#).unwrap();
        assert!(check(Access::SwarmOnly).is_ok());
        // ~/.opencode is not read (a documented gap): home's .opencode is skipped.
        std::fs::remove_file(&file).unwrap();
        let in_home = home.join("data/agent/opencode/workspace");
        std::fs::create_dir_all(&in_home).unwrap();
        std::fs::create_dir_all(home.join(".opencode/agent")).unwrap();
        std::fs::write(home.join(".opencode/agent/wats.md"), "x").unwrap();
        assert!(check_config_sources(Access::Workspace, &in_home, &work, Some(&home)).is_ok());
    }

    #[test]
    fn the_agent_dir_gets_a_stub_once() {
        let dir = tempfile::tempdir().unwrap();
        let agent = ensure_agent_dir(dir.path()).unwrap();
        assert_eq!(agent, dir.path().join("opencode-config/opencode"));
        let stub = std::fs::read_to_string(agent.join("opencode.jsonc")).unwrap();
        let v: Value = serde_json::from_str(&strip_jsonc(&stub)).unwrap();
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        std::fs::write(agent.join("opencode.jsonc"), "{}").unwrap();
        ensure_agent_dir(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(agent.join("opencode.jsonc")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn start_warnings_are_existence_checks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let work = home.join("data/agent/x/workspace");
        std::fs::create_dir_all(&work).unwrap();
        assert!(start_warnings(Access::Workspace, &work, Some(&home)).is_empty());
        std::fs::create_dir_all(home.join(".opencode")).unwrap();
        std::fs::write(home.join("opencode.json"), "{}").unwrap();
        let w = start_warnings(Access::Workspace, &work, Some(&home));
        assert_eq!(w.len(), 2, "{w:?}");
        let w = start_warnings(Access::SwarmOnly, &work, Some(&home));
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn global_file_and_paste_block() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        assert_eq!(
            global_config_file(None, Some(home.clone())).unwrap(),
            home.join(".config/opencode/opencode.json")
        );
        let xdg = dir.path().join("xdg");
        std::fs::create_dir_all(xdg.join("opencode")).unwrap();
        std::fs::write(xdg.join("opencode/opencode.jsonc"), "{}").unwrap();
        assert_eq!(
            global_config_file(Some(xdg.clone().into()), None).unwrap(),
            xdg.join("opencode/opencode.jsonc")
        );
        let block = paste_block("/usr/bin/wats", &s(&["mcp", "--profile", "alice"]));
        let v: Value = serde_json::from_str(&block).unwrap();
        assert_eq!(
            v["mcp"]["wats"]["command"],
            serde_json::json!(["/usr/bin/wats", "mcp", "--profile", "alice"])
        );
    }
}
