//! Lending tools and MCP servers (`wats share`, ADR 0037).
//!
//! - **One checklist** drives what a `workspace` run may use and what the
//!   agent announces: native tools (`agent.tools`), MCP servers of the
//!   user's own runtime configuration (`agent.servers`, names only) and the
//!   caps they give (`agent.caps`).
//! - **Definitions are read at each run** from the runtime's
//!   configuration, never copied into config.toml; a secret never becomes
//!   a command-line argument.
//! - **Off by default** for servers, and lending one needs `agent.review`
//!   or `agent.allow` ([`check_lend`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::access::Access;

/// The runtimes that lend servers in 0.4.8.
pub const RUNTIMES: [&str; 5] = ["claude", "codex", "opencode", "copilot", "antigravity"];

/// Whether `runtime` lends servers.
pub fn supports(runtime: &str) -> bool {
    RUNTIMES.contains(&runtime)
}

/// Said of every server, in the checklist and when one is lent.
pub const SERVER_WARNING: &str = "acts with your account: a post can ask it to do anything \
     its tools allow";

/// Said when servers are lent: what each guardrail guards.
pub const GUARDRAILS: &str = "agent.review stops a leak before it is posted, not an action \
     taken during the run; only agent.allow (who may wake the agent) or giving it read tools \
     only (agent.tools mcp__<server>__<tool>) keeps a stranger from steering a server that writes";

/// A native capability of a runtime: a group of its tools, and the caps
/// it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Native {
    pub label: &'static str,
    pub tools: &'static [&'static str],
    pub caps: &'static [&'static str],
    /// Checked when nothing was chosen yet (ADR 0037 D1).
    pub default: bool,
}

const RUN_LABEL: &str =
    "Run commands (not sandboxed: a post can make it read your secrets or change your files)";

const FETCH_LABEL: &str = "Fetch a URL (read a page or an API)";

const CLAUDE_NATIVES: &[Native] = &[
    Native {
        label: "Read and search files in its workspace",
        tools: &["Read", "Glob", "Grep"],
        caps: &[],
        default: true,
    },
    Native {
        label: "Edit files in its workspace",
        tools: &["Edit", "Write"],
        caps: &[],
        default: true,
    },
    Native {
        label: "Search the web",
        tools: &["WebSearch"],
        caps: &["web-search"],
        default: true,
    },
    Native {
        label: FETCH_LABEL,
        tools: &["WebFetch"],
        caps: &["web-fetch"],
        default: true,
    },
    Native {
        label: RUN_LABEL,
        tools: &["Bash"],
        caps: &["code-exec"],
        default: false,
    },
];

const CODEX_NATIVES: &[Native] = &[
    Native {
        label: "Generate images",
        tools: &[crate::access::CODEX_IMAGE_GENERATION],
        caps: &["image-gen"],
        default: true,
    },
    Native {
        label: "Search the web",
        tools: &["standalone_web_search"],
        caps: &["web-search"],
        default: true,
    },
    Native {
        label: "Run commands, read-only sandbox (it can still read your files and secrets)",
        tools: &["shell_tool"],
        caps: &["code-exec"],
        default: false,
    },
];

const OPENCODE_NATIVES: &[Native] = &[
    Native {
        label: "Read and search files in its workspace",
        tools: &["read", "glob", "grep", "list"],
        caps: &[],
        default: true,
    },
    Native {
        label: "Edit files in its workspace",
        tools: &["edit"],
        caps: &[],
        default: true,
    },
    Native {
        label: "Search the web",
        tools: &["websearch"],
        caps: &["web-search"],
        default: true,
    },
    Native {
        label: FETCH_LABEL,
        tools: &["webfetch"],
        caps: &["web-fetch"],
        default: true,
    },
    Native {
        label: RUN_LABEL,
        tools: &["bash"],
        caps: &["code-exec"],
        default: false,
    },
];

const COPILOT_NATIVES: &[Native] = &[
    Native {
        label: FETCH_LABEL,
        tools: &["url"],
        caps: &["web-fetch"],
        default: true,
    },
    Native {
        label: RUN_LABEL,
        tools: &["shell"],
        caps: &["code-exec"],
        default: false,
    },
];

const ANTIGRAVITY_NATIVES: &[Native] = &[
    Native {
        label: "Generate images",
        tools: &["image"],
        caps: &["image-gen"],
        default: true,
    },
    Native {
        label: "Search the web",
        tools: &["web-search"],
        caps: &["web-search"],
        default: true,
    },
    Native {
        label: FETCH_LABEL,
        tools: &["web-fetch"],
        caps: &["web-fetch"],
        default: true,
    },
    Native {
        label: "Run commands, in its terminal sandbox (macOS: nothing outside its workspace)",
        tools: &["shell"],
        caps: &["code-exec"],
        default: false,
    },
];

/// The native capabilities `wats share` offers for `runtime`.
pub fn natives(runtime: &str) -> &'static [Native] {
    match runtime {
        "claude" => CLAUDE_NATIVES,
        "codex" => CODEX_NATIVES,
        "opencode" => OPENCODE_NATIVES,
        "copilot" => COPILOT_NATIVES,
        "antigravity" => ANTIGRAVITY_NATIVES,
        _ => &[],
    }
}

/// A runtime's name for people.
pub fn runtime_label(runtime: &str) -> &str {
    match runtime {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "copilot" => "GitHub Copilot CLI",
        "antigravity" => "Antigravity CLI",
        other => other,
    }
}

/// Whether a server name can be lent: what every runtime takes as is in
/// a tool name and a config key.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The user's configuration files of `runtime` that hold its MCP servers:
/// `~/.claude.json` (user scope; `$CLAUDE_CONFIG_DIR/.claude.json` when
/// set), `$CODEX_HOME/config.toml`, OpenCode's global config.
pub fn source_paths(runtime: &str) -> Vec<PathBuf> {
    let home = std::env::home_dir();
    match runtime {
        "claude" => std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or(home)
            .map(|d| d.join(".claude.json"))
            .into_iter()
            .collect(),
        "codex" => crate::access::codex_home()
            .map(|d| d.join("config.toml"))
            .into_iter()
            .collect(),
        "copilot" => crate::hosts::Host::Copilot
            .user_path()
            .into_iter()
            .collect(),
        "antigravity" => crate::hosts::Host::Antigravity
            .user_path()
            .into_iter()
            .collect(),
        "opencode" => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.map(|h| h.join(".config")));
            config
                .map(|c| {
                    ["config.json", "opencode.json", "opencode.jsonc"]
                        .map(|n| c.join("opencode").join(n))
                        .to_vec()
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// The MCP servers of `runtime` in `paths` ([`source_paths`]), name →
/// definition as JSON; `wats` left out, a later file winning. A missing
/// file holds none.
pub fn servers_in(runtime: &str, paths: &[PathBuf]) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let servers = match runtime {
            "codex" => {
                let table: toml::Table = toml::from_str(&text)
                    .with_context(|| format!("{}: not valid TOML", path.display()))?;
                table
                    .get("mcp_servers")
                    .map(serde_json::to_value)
                    .transpose()?
                    .unwrap_or(Value::Null)
            }
            "opencode" => {
                let config: Value = serde_json::from_str(&crate::opencode::strip_jsonc(&text))
                    .with_context(|| format!("{}: not valid JSON(C)", path.display()))?;
                config["mcp"].clone()
            }
            _ => {
                let config: Value = serde_json::from_str(&text)
                    .with_context(|| format!("{}: not valid JSON", path.display()))?;
                config["mcpServers"].clone()
            }
        };
        for (name, definition) in servers.as_object().into_iter().flatten() {
            if name != "wats" && definition.is_object() {
                out.insert(name.clone(), definition.clone());
            }
        }
    }
    Ok(out)
}

/// The user's MCP servers of `runtime`.
pub fn user_servers(runtime: &str) -> Result<BTreeMap<String, Value>> {
    servers_in(runtime, &source_paths(runtime))
}

/// How a server is reached, as the checklist shows it.
pub fn transport(definition: &Value) -> &'static str {
    let kind = definition["type"].as_str().unwrap_or_default();
    if matches!(kind, "http" | "sse" | "remote" | "streamable-http")
        || definition.get("url").is_some()
    {
        "remote"
    } else {
        "local"
    }
}

/// The servers of `lend` found in `available`, and a warning for each
/// one that is not there or cannot be lent.
pub fn pick(
    lend: &[String],
    available: &BTreeMap<String, Value>,
) -> (Vec<(String, Value)>, Vec<String>) {
    let mut servers = Vec::new();
    let mut warnings = Vec::new();
    for name in lend {
        let name = name.trim();
        if !valid_name(name) {
            warnings.push(format!(
                "agent.servers {name:?}: not used (a name of letters, digits, '-' and '_')"
            ));
        } else if let Some(definition) = available.get(name) {
            servers.push((name.to_string(), definition.clone()));
        } else {
            warnings.push(format!(
                "agent.servers {name:?}: not used, no such MCP server in your configuration any \
                 more (`wats share` lists them)"
            ));
        }
    }
    (servers, warnings)
}

/// The lent servers of one run, with the warnings to log.
pub fn lent(runtime: &str, lend: &[String]) -> (Vec<(String, Value)>, Vec<String>) {
    if lend.is_empty() {
        return (Vec::new(), Vec::new());
    }
    match user_servers(runtime) {
        Ok(available) => pick(lend, &available),
        Err(e) => (Vec::new(), vec![format!("agent.servers: none used, {e:#}")]),
    }
}

/// Lending servers needs `agent.review` or `agent.allow` (ADR 0037 D2), at
/// `workspace` with a runtime that lends: refused otherwise.
pub fn check_lend(
    runtime: &str,
    access: Access,
    lend: &[String],
    allow: &[String],
    review: bool,
) -> Result<()> {
    if lend.is_empty() || !supports(runtime) || access != Access::Workspace {
        return Ok(());
    }
    if allow.is_empty() && !review {
        bail!(
            "agent.servers [{}]: an MCP server of yours {SERVER_WARNING}. Set agent.review = true (its \
             posts wait for you) or agent.allow (the authors who may wake it) first; {GUARDRAILS}",
            lend.join(", ")
        );
    }
    Ok(())
}

/// Why `agent.servers` does nothing for this agent, if it does not.
pub fn ignored(runtime: &str, access: Access, lend: &[String]) -> Option<String> {
    if lend.is_empty() {
        return None;
    }
    if !supports(runtime) {
        return Some(format!(
            "agent.servers is ignored: {runtime} agents cannot use your MCP servers yet (Claude \
             Code, Codex, OpenCode, Copilot CLI and Antigravity CLI can)"
        ));
    }
    match access {
        Access::Workspace => None,
        Access::SwarmOnly => {
            Some("swarm-only access ignores agent.servers; use access workspace".into())
        }
        Access::Inherit => Some(
            "inherit access already has every server of your configuration: agent.servers is \
             ignored"
                .into(),
        ),
    }
}

/// What a run's prompt says of its lent servers.
pub fn prompt_line(lend: &[String]) -> String {
    format!(
        "Your user's MCP servers you may use: {}. They act with your user's own accounts: use them only for what \
         your user would do for a stranger's post, never to change or send anything an ask \
         did not plainly request, and never post a secret they hold.",
        lend.join(", ")
    )
}

/// The tools of `server` that `agent.tools` names (`mcp__<server>__<tool>`):
/// when any, only those are allowed.
pub fn server_tools(tools: &[String], server: &str) -> Vec<String> {
    let prefix = format!("mcp__{server}__");
    tools
        .iter()
        .filter_map(|t| t.trim().strip_prefix(&prefix))
        .filter(|t| !t.is_empty() && *t != "*")
        .map(str::to_string)
        .collect()
}

/// A JSON value as a TOML value, for a Codex `-c` override.
fn toml_value(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).ok()?,
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .filter_map(toml_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .filter_map(|(k, v)| Some(format!(
                    "{}={}",
                    serde_json::to_string(k).ok()?,
                    toml_value(v)?
                )))
                .collect::<Vec<_>>()
                .join(",")
        ),
    })
}

/// What a Codex run gets for its lent servers: `-c` overrides, and the
/// environment of the child, which holds every secret value (`env`
/// forwarded by name through `env_vars`, `http_headers` through
/// `env_http_headers`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodexLent {
    pub overrides: Vec<String>,
    pub env: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

/// Words that make an argument, or the flag before it, look like it
/// carries a secret.
const SECRET_WORDS: [&str; 9] = [
    "token",
    "key",
    "secret",
    "password",
    "passwd",
    "bearer",
    "auth",
    "credential",
    "cookie",
];

/// Prefixes of well-known credentials.
const SECRET_PREFIXES: [&str; 8] = [
    "sk-",
    "ghp_",
    "gho_",
    "github_pat_",
    "glpat-",
    "xox",
    "akia",
    "aiza",
];

/// Whether one value looks like it holds a secret: a secret word, a
/// credential prefix, or a long random-looking string.
fn looks_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| lower.contains(w))
        || SECRET_PREFIXES.iter().any(|p| lower.starts_with(p))
        || (value.len() >= 32
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '=')))
}

/// Why a Codex server cannot be lent without a secret on Codex's command
/// line, which other local users can list (`ps`): its `command`, `args`
/// or `url` look like they hold one (a URL with a query or a user part,
/// an argument or flag naming a token or key, a credential prefix). Codex
/// takes lent servers only as `-c` overrides; `env`, `env_vars` and
/// headers go through the environment ([`codex_lent`]).
pub fn codex_secret_on_argv(definition: &Value) -> Option<String> {
    if let Some(url) = definition["url"].as_str()
        && (url.contains('?')
            || url.contains('#')
            || url
                .split("://")
                .nth(1)
                .is_some_and(|r| r.split('/').next().is_some_and(|host| host.contains('@'))))
    {
        return Some("its url has a query or credentials".into());
    }
    if definition["command"].as_str().is_some_and(looks_secret) {
        return Some("its command looks like it holds a secret".into());
    }
    let args: Vec<&str> = definition["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if args.iter().any(|a| looks_secret(a)) {
        return Some("an argument looks like it holds a secret".into());
    }
    None
}

/// [`CodexLent`] of `servers`, narrowed to the tools `agent.tools` names.
/// A server whose command line would carry a secret is not lent
/// ([`codex_secret_on_argv`]).
pub fn codex_lent(servers: &[(String, Value)], tools: &[String]) -> CodexLent {
    let mut out = CodexLent::default();
    for (name, definition) in servers {
        let Some(fields) = definition.as_object() else {
            continue;
        };
        if let Some(why) = codex_secret_on_argv(definition) {
            out.warnings.push(format!(
                "agent.servers {name:?}: not used with Codex: {why}, and Codex takes the \
                 servers it is given on its command line, which other users of this machine \
                 can list. Move the secret to the server's env (or env_vars, \
                 bearer_token_env_var) in your Codex config.toml"
            ));
            continue;
        }
        let mut env: Vec<(String, String)> = Vec::new();
        let mut forwarded: Vec<Value> = fields
            .get("env_vars")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (key, value) in fields
            .get("env")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            env.push((key.clone(), value));
            forwarded.push(Value::String(key.clone()));
        }
        let mut headers = fields
            .get("env_http_headers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let marker = name.to_ascii_uppercase().replace('-', "_");
        for (i, (header, value)) in fields
            .get("http_headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let var = format!("WATS_LEND_{marker}_{i}");
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            env.push((var.clone(), value));
            headers.insert(header.clone(), Value::String(var));
        }
        if let Some((key, _)) = env
            .iter()
            .find(|(key, value)| out.env.iter().any(|(k, v)| k == key && v != value))
        {
            out.warnings.push(format!(
                "agent.servers {name:?}: not used, its {key} differs from another server's"
            ));
            continue;
        }
        let set = |key: &str, value: String| format!("mcp_servers.{name}.{key}={value}");
        for (key, value) in fields {
            if matches!(
                key.as_str(),
                "env" | "env_vars" | "http_headers" | "env_http_headers" | "enabled_tools"
            ) {
                continue;
            }
            if let Some(value) = toml_value(value) {
                out.overrides.push(set(key, value));
            }
        }
        if !forwarded.is_empty() {
            forwarded.dedup();
            out.overrides.push(set(
                "env_vars",
                toml_value(&Value::Array(forwarded)).unwrap_or_default(),
            ));
        }
        if !headers.is_empty() {
            out.overrides.push(set(
                "env_http_headers",
                toml_value(&Value::Object(headers)).unwrap_or_default(),
            ));
        }
        let named = server_tools(tools, name);
        let enabled = if named.is_empty() {
            fields.get("enabled_tools").cloned()
        } else {
            Some(Value::Array(named.into_iter().map(Value::String).collect()))
        };
        if let Some(value) = enabled.as_ref().and_then(toml_value) {
            out.overrides.push(set("enabled_tools", value));
        }
        if !fields.contains_key("default_tools_approval_mode") {
            out.overrides
                .push(set("default_tools_approval_mode", "\"approve\"".into()));
        }
        for pair in env {
            if !out.env.contains(&pair) {
                out.env.push(pair);
            }
        }
    }
    out
}

/// What `wats share` remembers: the server names it offered per runtime,
/// so that a later run marks the new ones. Names only.
pub const STATE_FILE: &str = "share.json";

/// The server names already offered for `runtime`, or `None` before the
/// first `wats share` for it.
pub fn offered(data_dir: &Path, runtime: &str) -> Option<Vec<String>> {
    let bytes = std::fs::read(data_dir.join(STATE_FILE)).ok()?;
    let state: Value = serde_json::from_slice(&bytes).ok()?;
    let names = state["offered"][runtime].as_array()?;
    Some(
        names
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
    )
}

/// Remember the server names offered for `runtime`.
pub fn remember(data_dir: &Path, runtime: &str, names: &[String]) -> Result<()> {
    let path = data_dir.join(STATE_FILE);
    let mut state: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    if !state["offered"].is_object() {
        state["offered"] = serde_json::json!({});
    }
    state["offered"][runtime] = serde_json::json!(names);
    std::fs::write(&path, serde_json::to_vec_pretty(&state)?)
        .with_context(|| format!("write {}", path.display()))
}

/// An agent's lending settings, before and after the checklist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lending {
    pub tools: Vec<String>,
    pub lend: Vec<String>,
    pub caps: Vec<String>,
}

/// What an agent of `runtime` gets before anyone runs `wats share`: the
/// natives checked by default (running commands is not), their caps.
pub fn default_lending(runtime: &str) -> Lending {
    let natives = natives(runtime);
    let chosen: Vec<usize> = (0..natives.len()).filter(|i| natives[*i].default).collect();
    choose(&Lending::default(), runtime, &chosen, &[], &[])
}

/// Whether a native is on in `tools`: all its tools listed.
pub fn native_on(native: &Native, tools: &[String]) -> bool {
    // Antigravity's `web` (0.4.9 and 0.4.10) lends both search and fetch.
    native.tools.iter().all(|t| {
        tools.iter().any(|x| {
            let x = x.trim();
            x == *t || (x == "web" && matches!(*t, "web-search" | "web-fetch"))
        })
    })
}

/// The cap a lent server proposes (ADR 0037 D3): its name, lowercase.
pub fn server_cap(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// `current` with the checklist's choices: the natives' tools and caps,
/// the lent servers and their caps. What the checklist does not offer
/// (a tool with a pattern, an `mcp__` entry, another cap) is kept.
pub fn choose(
    current: &Lending,
    runtime: &str,
    chosen_natives: &[usize],
    offered_servers: &[String],
    chosen_servers: &[String],
) -> Lending {
    let natives = natives(runtime);
    let mut managed_tools: Vec<&str> = natives
        .iter()
        .flat_map(|n| n.tools.iter().copied())
        .collect();
    if runtime == "antigravity" {
        managed_tools.push("web");
    }
    let mut tools: Vec<String> = current
        .tools
        .iter()
        .filter(|t| !managed_tools.contains(&t.trim()))
        .cloned()
        .collect();
    let mut caps_off: Vec<String> = Vec::new();
    let mut caps_on: Vec<String> = Vec::new();
    for (i, native) in natives.iter().enumerate() {
        let caps = native.caps.iter().map(|c| c.to_string());
        if chosen_natives.contains(&i) {
            tools.extend(native.tools.iter().map(|t| t.to_string()));
            caps_on.extend(caps);
        } else {
            caps_off.extend(caps);
        }
    }
    // `cap:web` stays an alias of either web ability (asks tagged before
    // the split, ADR 0037).
    if caps_on
        .iter()
        .any(|c| c == "web-search" || c == "web-fetch")
    {
        caps_on.push("web".to_string());
    } else if caps_off
        .iter()
        .any(|c| c == "web-search" || c == "web-fetch")
    {
        caps_off.push("web".to_string());
    }
    for name in offered_servers {
        if chosen_servers.contains(name) {
            caps_on.push(server_cap(name));
        } else {
            caps_off.push(server_cap(name));
        }
    }
    let bare = |c: &str| {
        let c = c.trim().to_ascii_lowercase();
        c.strip_prefix("cap:").unwrap_or(&c).to_string()
    };
    let mut caps: Vec<String> = current
        .caps
        .iter()
        .filter(|c| !caps_off.contains(&bare(c)) || caps_on.contains(&bare(c)))
        .cloned()
        .collect();
    for cap in caps_on {
        if !caps.iter().any(|c| bare(c) == cap) {
            caps.push(cap);
        }
    }
    let mut lend: Vec<String> = current
        .lend
        .iter()
        .filter(|n| !offered_servers.contains(n))
        .cloned()
        .collect();
    lend.extend(chosen_servers.iter().cloned());
    Lending { tools, lend, caps }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn servers_are_read_from_each_runtime_config() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude.json");
        std::fs::write(
            &claude,
            r#"{"mcpServers": {"github": {"type": "stdio", "command": "gh-mcp"},
                "wats": {"command": "wats"}, "docs": {"type": "http", "url": "https://d"}},
                "projects": {"/p": {"mcpServers": {"local": {"command": "x"}}}}}"#,
        )
        .unwrap();
        let found = servers_in("claude", &[claude]).unwrap();
        assert_eq!(found.keys().collect::<Vec<_>>(), ["docs", "github"]);
        assert_eq!(transport(&found["docs"]), "remote");
        assert_eq!(transport(&found["github"]), "local");

        let codex = dir.path().join("config.toml");
        std::fs::write(
            &codex,
            "model = \"x\"\n[mcp_servers.crm]\ncommand = \"crm\"\nargs = [\"-v\"]\n\
             [mcp_servers.crm.env]\nCRM_KEY = \"secret\"\n",
        )
        .unwrap();
        let found = servers_in("codex", &[codex]).unwrap();
        assert_eq!(found["crm"]["env"]["CRM_KEY"], "secret");

        let opencode = dir.path().join("opencode.jsonc");
        std::fs::write(
            &opencode,
            "{\n // mine\n \"mcp\": {\"fs\": {\"type\": \"local\", \"command\": [\"fs\"]}}}",
        )
        .unwrap();
        let missing = dir.path().join("none.json");
        let found = servers_in("opencode", &[missing, opencode]).unwrap();
        assert_eq!(found.keys().collect::<Vec<_>>(), ["fs"]);
    }

    #[test]
    fn missing_and_invalid_names_are_not_lent() {
        let available: BTreeMap<String, Value> =
            [("github".to_string(), json!({"command": "gh"}))].into();
        let (servers, warnings) = pick(&s(&["github", "gone", "a.b"]), &available);
        assert_eq!(servers.len(), 1);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("gone"), "{warnings:?}");
        assert!(warnings[1].contains("a.b"), "{warnings:?}");
    }

    #[test]
    fn lending_needs_review_or_allow() {
        let lend = s(&["crm"]);
        let e = check_lend("claude", Access::Workspace, &lend, &[], false).unwrap_err();
        assert!(e.to_string().contains("agent.review"), "{e}");
        assert!(check_lend("claude", Access::Workspace, &lend, &[], true).is_ok());
        assert!(check_lend("claude", Access::Workspace, &lend, &s(&["ab"]), false).is_ok());
        assert!(check_lend("claude", Access::SwarmOnly, &lend, &[], false).is_ok());
        assert!(check_lend("vibe", Access::Workspace, &lend, &[], false).is_ok());
        assert!(check_lend("claude", Access::Workspace, &[], &[], false).is_ok());
        assert!(ignored("vibe", Access::Inherit, &lend).is_some());
        assert!(ignored("codex", Access::SwarmOnly, &lend).is_some());
        assert!(ignored("codex", Access::Inherit, &lend).is_some());
        assert!(ignored("codex", Access::Workspace, &lend).is_none());
    }

    #[test]
    fn codex_secrets_go_to_the_environment() {
        let servers = vec![
            (
                "crm".to_string(),
                json!({"command": "crm", "args": ["-v"], "env": {"CRM_KEY": "s3cret"},
                    "env_vars": ["HOME"]}),
            ),
            (
                "docs".to_string(),
                json!({"url": "https://d", "http_headers": {"Authorization": "Bearer t0ken"},
                    "default_tools_approval_mode": "prompt"}),
            ),
        ];
        let lent = codex_lent(&servers, &s(&["mcp__crm__search", "Read"]));
        let all = lent.overrides.join("\n");
        assert!(!all.contains("s3cret") && !all.contains("t0ken"), "{all}");
        assert!(all.contains("mcp_servers.crm.command=\"crm\""), "{all}");
        assert!(all.contains("mcp_servers.crm.args=[\"-v\"]"), "{all}");
        assert!(
            all.contains("mcp_servers.crm.env_vars=[\"HOME\",\"CRM_KEY\"]"),
            "{all}"
        );
        assert!(
            all.contains("mcp_servers.crm.enabled_tools=[\"search\"]"),
            "{all}"
        );
        assert!(
            all.contains("mcp_servers.crm.default_tools_approval_mode=\"approve\""),
            "{all}"
        );
        assert!(
            all.contains(
                "mcp_servers.docs.env_http_headers={\"Authorization\"=\"WATS_LEND_DOCS_0\"}"
            ),
            "{all}"
        );
        assert!(all.contains("mcp_servers.docs.default_tools_approval_mode=\"prompt\""));
        assert!(!all.contains("docs.enabled_tools"), "{all}");
        assert_eq!(
            lent.env,
            vec![
                ("CRM_KEY".to_string(), "s3cret".to_string()),
                ("WATS_LEND_DOCS_0".to_string(), "Bearer t0ken".to_string())
            ]
        );

        let clash = vec![
            ("a".to_string(), json!({"command": "a", "env": {"K": "1"}})),
            ("b".to_string(), json!({"command": "b", "env": {"K": "2"}})),
        ];
        let lent = codex_lent(&clash, &[]);
        assert_eq!(lent.env, vec![("K".to_string(), "1".to_string())]);
        assert_eq!(lent.warnings.len(), 1);
        assert!(!lent.overrides.join(" ").contains("mcp_servers.b."));
    }

    #[test]
    fn codex_never_lends_a_secret_on_its_command_line() {
        for bad in [
            json!({"command": "crm", "args": ["--api-key", "x"]}),
            json!({"command": "crm", "args": ["--token=abc"]}),
            json!({"command": "crm", "args": ["ghp_abcdef"]}),
            json!({"command": "crm", "args": ["0123456789abcdef0123456789abcdef"]}),
            json!({"url": "https://mcp.example.com/sse?key=abc"}),
            json!({"url": "https://user:pw@mcp.example.com/"}),
        ] {
            assert!(codex_secret_on_argv(&bad).is_some(), "{bad}");
            let lent = codex_lent(&[("x".to_string(), bad.clone())], &[]);
            assert!(lent.overrides.is_empty(), "{bad}");
            assert_eq!(lent.warnings.len(), 1);
        }
        for good in [
            json!({"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]}),
            json!({"command": "/usr/local/bin/crm-mcp", "args": ["--stdio"], "env": {"CRM_KEY": "s"}}),
            json!({"url": "https://mcp.example.com/mcp", "bearer_token_env_var": "CRM_TOKEN"}),
        ] {
            assert_eq!(codex_secret_on_argv(&good), None, "{good}");
        }
    }

    #[test]
    fn the_checklist_sets_tools_lend_and_caps() {
        let current = Lending {
            tools: s(&["Read(/srv/**)", "Bash", "mcp__crm__search"]),
            lend: s(&["old"]),
            caps: s(&["pdf", "code-exec"]),
        };
        let next = choose(
            &current,
            "claude",
            &[0, 2, 3],
            &s(&["crm", "old", "mail"]),
            &s(&["crm"]),
        );
        assert_eq!(
            next.tools,
            s(&[
                "Read(/srv/**)",
                "mcp__crm__search",
                "Read",
                "Glob",
                "Grep",
                "WebSearch",
                "WebFetch"
            ])
        );
        assert_eq!(next.lend, s(&["crm"]));
        assert_eq!(
            next.caps,
            s(&["pdf", "web-search", "web-fetch", "web", "crm"])
        );
        assert!(native_on(&CLAUDE_NATIVES[0], &next.tools));
        assert!(!native_on(&CLAUDE_NATIVES[4], &next.tools));
        // Antigravity's old `web` reads as both web items, and goes.
        let old = Lending {
            tools: s(&["web"]),
            ..Lending::default()
        };
        let natives = natives("antigravity");
        let web: Vec<usize> = (0..natives.len())
            .filter(|i| native_on(&natives[*i], &old.tools))
            .collect();
        assert_eq!(web.len(), 2);
        let next = choose(&old, "antigravity", &web, &[], &[]);
        assert_eq!(next.tools, s(&["web-search", "web-fetch"]));
    }

    #[test]
    fn running_commands_is_off_by_default() {
        for runtime in RUNTIMES {
            let natives = natives(runtime);
            assert!(!natives.is_empty());
            for native in natives {
                assert_eq!(
                    native.default,
                    !native.caps.contains(&"code-exec"),
                    "{native:?}"
                );
            }
        }
    }

    #[test]
    fn what_was_offered_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(offered(dir.path(), "claude"), None);
        remember(dir.path(), "claude", &s(&["a"])).unwrap();
        remember(dir.path(), "codex", &[]).unwrap();
        assert_eq!(offered(dir.path(), "claude"), Some(s(&["a"])));
        assert_eq!(offered(dir.path(), "codex"), Some(Vec::new()));
    }
}
