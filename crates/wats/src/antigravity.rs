//! Antigravity CLI (`agy`) as the node's agent (ADR 0038 §2).
//!
//! Each run spawns, from the run directory, with `HOME` set to the agent's
//! own home (`<data dir>/agent/<name>/home`):
//!
//! ```text
//! agy -p <prompt> --output-format stream-json --print-timeout <n>s
//!     [--model <m>] [extra args]
//! ```
//!
//! That home holds the agent's Antigravity state: its login (the OAuth
//! token, in the keychain or `.gemini/antigravity-cli/`), and three files
//! wats rewrites before each run:
//!
//! - `.gemini/config/mcp_config.json`: the wats server only, with the
//!   run's token (`0600`);
//! - `.gemini/antigravity-cli/settings.json`: `permissions` that deny
//!   files, commands and URLs and allow `mcp(wats/*)`;
//! - `.gemini/config/hooks.json`: a `PreToolUse` gate, `wats gate
//!   antigravity`, that hard-denies every tool but the wats ones (the
//!   rules do not cover the browser, subagents, messages or images).
//!
//! Only `swarm-only` in this version: `workspace` waits for check (d),
//! `inherit` for a per-run MCP config.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::access::Access;

/// The runtime's name in config.toml.
pub const RUNTIME: &str = "antigravity";

/// Its command.
pub const COMMAND: &str = "agy";

/// The server name of wats in a run's MCP config.
pub const SERVER: &str = "wats";

/// Said where its access level is shown.
pub const CLOSED_SOURCE: &str = "Antigravity CLI is closed source: wats cannot check what \
     it reads or sends";

/// Antigravity's access level (ADR 0038 §2.2): swarm-only by default,
/// workspace (check (d): Ask fails closed headless), never inherit.
pub fn access(value: Option<&str>) -> Result<Access> {
    let level = value.map_or(Ok(Access::SwarmOnly), str::parse)?;
    if level == Access::Inherit {
        bail!(
            "agent.access = inherit is not available with Antigravity CLI: it has no per-run \
             MCP config, so wats would have to write its runner token into your own \
             ~/.gemini/config/mcp_config.json (ADR 0038 §2.2). Use swarm-only or workspace"
        );
    }
    Ok(level)
}

/// What a run's locks let through: its level, and at `workspace` what
/// `agent.tools` and `agent.servers` lend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    pub workspace: bool,
    /// `agent.tools` lists `shell` (run under `--sandbox`).
    pub shell: bool,
    /// `agent.tools` lists `web`.
    pub web: bool,
    /// The lent MCP servers.
    pub servers: Vec<String>,
}

impl Lock {
    /// The lock of a run at `access`, with `agent.tools` and the lent
    /// servers' names (both ignored at swarm-only).
    pub fn of(access: Access, tools: &[String], servers: &[String]) -> Lock {
        if access != Access::Workspace {
            return Lock::default();
        }
        let lists = |kind: &str| tools.iter().any(|t| t.trim().eq_ignore_ascii_case(kind));
        Lock {
            workspace: true,
            shell: lists("shell"),
            web: lists("web"),
            servers: servers.to_vec(),
        }
    }

    /// The gate's arguments after `wats gate antigravity`.
    pub fn gate_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (on, flag) in [
            (self.workspace, "--workspace"),
            (self.shell, "--shell"),
            (self.web, "--web"),
        ] {
            if on {
                out.push(flag.to_string());
            }
        }
        for server in &self.servers {
            out.extend(["--server".to_string(), server.clone()]);
        }
        out
    }
}

/// Lending the shell needs Antigravity's terminal sandbox, which since
/// 1.4.4 does nothing, silently, where it is unavailable: wats lends it on
/// macOS (Seatbelt) only for now.
pub fn check_shell(lock: &Lock) -> Result<()> {
    if lock.shell && !cfg!(target_os = "macos") {
        bail!(
            "agent.tools shell with Antigravity CLI needs its terminal sandbox, which silently \
             does nothing where Linux namespaces are unavailable: wats lends the shell on macOS \
             only for now (ADR 0038 §2.2)"
        );
    }
    Ok(())
}

/// Arguments wats sets itself, or that would undo the level: refused in
/// `agent.args`.
pub const REFUSED_ARGS: [&str; 9] = [
    "-p",
    "--print",
    "--prompt",
    "--output-format",
    "--input-format",
    "--dangerously-skip-permissions",
    "--add-dir",
    "--continue",
    "--conversation",
];

/// Refuse `agent.args` that wats sets itself or that would undo the level.
pub fn check_extra_args(extra: &[String]) -> Result<()> {
    for arg in extra {
        let flag = arg.split('=').next().unwrap_or(arg);
        if REFUSED_ARGS.contains(&flag) || flag == "-c" || flag == "--mode" {
            bail!(
                "agent.args {arg:?}: wats sets the Antigravity CLI flags of a run itself \
                 (ADR 0038); remove it"
            );
        }
    }
    Ok(())
}

/// The agent's own home (`HOME` of its runs).
pub fn agent_home(work_dir: &Path) -> PathBuf {
    work_dir.join("home")
}

fn mcp_config_path(home: &Path) -> PathBuf {
    home.join(".gemini/config/mcp_config.json")
}

fn settings_path(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli/settings.json")
}

fn hooks_path(home: &Path) -> PathBuf {
    home.join(".gemini/config/hooks.json")
}

/// The permission rules of a run (precedence Deny > Ask > Allow). No
/// `mcp(*)` deny: it would beat the allows, and the run's MCP config
/// holds only wats and the lent servers anyway. At workspace, files are
/// Antigravity's: inside it allowed, outside Ask, which a headless run
/// denies (check (d)).
pub fn permissions(lock: &Lock) -> Value {
    let mut allow = vec![format!("mcp({SERVER}/*)")];
    allow.extend(lock.servers.iter().map(|s| format!("mcp({s}/*)")));
    let mut deny: Vec<&str> = Vec::new();
    if !lock.workspace {
        deny.extend(["read_file(*)", "write_file(*)"]);
    }
    if !lock.shell {
        deny.push("command(*)");
    }
    if !lock.web {
        deny.extend(["read_url(*)", "execute_url(*)"]);
    }
    json!({"allow": allow, "deny": deny})
}

/// `settings.json` with wats's `permissions`, the rest kept.
pub fn settings(current: Option<&str>, lock: &Lock) -> Result<String> {
    let mut map = match current.map(serde_json::from_str::<Value>).transpose() {
        Ok(Some(Value::Object(map))) => map,
        Ok(None) => serde_json::Map::new(),
        _ => bail!("not a JSON object"),
    };
    map.insert("permissions".into(), permissions(lock));
    Ok(format!("{}\n", serde_json::to_string_pretty(&map)?))
}

/// `hooks.json`: the gate on every tool.
pub fn hooks(gate: &str) -> Value {
    json!({
        "wats-gate": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": gate, "timeout": 10}],
            }],
        },
    })
}

/// A word of a POSIX shell command line.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The gate's command line: `<wats> gate antigravity [lock flags]`.
pub fn gate_command(wats: &Path, lock: &Lock) -> String {
    let mut words = vec![
        shell_word(&wats.to_string_lossy()),
        "gate".into(),
        RUNTIME.into(),
    ];
    words.extend(lock.gate_args().iter().map(|w| shell_word(w)));
    words.join(" ")
}

/// Write a run's files into the agent's home: its MCP config (`0600`:
/// wats with the run's token, and the lent servers), settings and hooks.
/// `real_home` is passed to the wats server, which must not see the
/// agent's home as `HOME`.
pub struct Home<'a> {
    pub home: &'a Path,
    pub wats_command: &'a Path,
    pub wats_args: &'a [String],
    pub token: &'a str,
    pub real_home: Option<&'a Path>,
    pub lock: &'a Lock,
    pub lent: &'a [(String, Value)],
}

pub fn prepare_home(h: &Home) -> Result<()> {
    make_home(h.home)?;
    let mut env = serde_json::Map::new();
    env.insert("WATS_RUNNER_TOKEN".into(), json!(h.token));
    if let Some(real) = h.real_home {
        env.insert("HOME".into(), json!(real));
    }
    let mut servers = serde_json::Map::new();
    if h.lock.workspace {
        for (name, definition) in h.lent {
            if name != SERVER {
                servers.insert(name.clone(), definition.clone());
            }
        }
    }
    servers.insert(
        SERVER.into(),
        json!({"command": h.wats_command, "args": h.wats_args, "env": env}),
    );
    crate::access::write_private(
        &mcp_config_path(h.home),
        &serde_json::to_vec_pretty(&json!({"mcpServers": servers}))?,
    )?;
    write_locks(h.home, h.wats_command, h.lock)
}

/// The agent's home, `0700`, with its two configuration directories.
fn make_home(home: &Path) -> Result<()> {
    for dir in [
        home.join(".gemini/config"),
        home.join(".gemini/antigravity-cli"),
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The login run of `wats setup antigravity --agent`: the swarm-only
/// locks, and an MCP config with no server.
pub fn prepare_login(home: &Path, wats_command: &Path) -> Result<()> {
    make_home(home)?;
    crate::access::write_private(&mcp_config_path(home), b"{\"mcpServers\": {}}\n")?;
    write_locks(home, wats_command, &Lock::default())
}

/// The prompt of the login run.
pub const LOGIN_PROMPT: &str = "Reply with the single word OK.";

/// The permission rules and the gate.
fn write_locks(home: &Path, wats_command: &Path, lock: &Lock) -> Result<()> {
    let path = settings_path(home);
    let current = std::fs::read_to_string(&path).ok();
    let text = settings(current.as_deref(), lock)
        .with_context(|| format!("{}: left as it is", path.display()))?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    let path = hooks_path(home);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&hooks(&gate_command(wats_command, lock)))?,
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Refuse a run directory below an `.agents/` directory: Antigravity
/// would load its hooks, rules and servers (ADR 0038 §2.1).
pub fn check_ancestors(run_dir: &Path) -> Result<()> {
    for dir in run_dir.ancestors() {
        let agents = dir.join(".agents");
        if agents.is_dir() {
            bail!(
                "{} exists above the agent's run directory: Antigravity CLI would load its \
                 rules, hooks and MCP servers into every run. Move it, or the data directory",
                agents.display()
            );
        }
    }
    Ok(())
}

/// Inputs of one run's arguments.
pub struct Run<'a> {
    pub prompt: &'a str,
    /// A lent shell runs in Antigravity's terminal sandbox.
    pub sandbox: bool,
    pub timeout: std::time::Duration,
    pub model: Option<&'a str>,
}

/// `agy` arguments of one run, before the extra arguments.
pub fn run_args(r: &Run) -> Vec<String> {
    let mut out: Vec<String> = vec![
        "-p".into(),
        r.prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--print-timeout".into(),
        format!("{}s", r.timeout.as_secs().max(60)),
    ];
    if r.sandbox {
        out.push("--sandbox".into());
    }
    if let Some(model) = r.model {
        out.push(format!("--model={model}"));
    }
    out
}

/// The gate's answer to one `PreToolUse` payload: allow what the lock
/// lets through, deny everything else, the tools the permission rules do
/// not cover included (the browser, subagents, messages, schedules,
/// images). The rules still apply after an allow: the two locks stack.
pub fn decide(payload: &Value, lock: &Lock) -> Value {
    let call = &payload["toolCall"];
    let name = call["name"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let allowed = mcp_server(call).is_some_and(|server| {
        server == SERVER || (lock.workspace && lock.servers.contains(&server))
    }) || (lock.workspace && is_file_tool(&name))
        || (lock.shell && is_shell_tool(&name))
        || (lock.web && is_web_tool(&name));
    if allowed {
        json!({"decision": "allow"})
    } else {
        json!({
            "decision": "deny",
            "reason": format!(
                "{name}: not available to this WeAreTheSwarm agent ({})",
                if lock.workspace {
                    "workspace: its files, and only the tools it was lent"
                } else {
                    "swarm-only: the wats tools only"
                }
            ),
        })
    }
}

/// The MCP server of a tool call: a tool named after it (`mcp_<server>_…`,
/// `mcp__<server>__…`), or a generic MCP call (`call_mcp_tool`) with a
/// server argument.
fn mcp_server(call: &Value) -> Option<String> {
    let name = call["name"].as_str().unwrap_or_default();
    for prefix in ["mcp__", "mcp_"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let sep = if prefix == "mcp__" { "__" } else { "_" };
            if let Some((server, _)) = rest.split_once(sep) {
                return Some(server.to_string());
            }
        }
    }
    if !name.to_ascii_lowercase().contains("mcp") {
        return None;
    }
    call["args"].as_object().and_then(|args| {
        args.iter()
            .find(|(k, _)| k.to_ascii_lowercase().contains("server"))
            .and_then(|(_, v)| v.as_str().map(str::to_string))
    })
}

/// File tools (`read_file`, `write_to_file`, `replace_file_content`,
/// `list_dir`, `grep_search`…): Antigravity's rules keep them inside the
/// workspace.
fn is_file_tool(name: &str) -> bool {
    !name.contains("browser")
        && (name.contains("file")
            || name.contains("dir")
            || name.contains("grep")
            || name == "find_by_name"
            || name == "codebase_search")
}

/// Shell tools (`run_command`, `send_command_input`, `command_status`).
fn is_shell_tool(name: &str) -> bool {
    !name.contains("browser") && (name.contains("command") || name.contains("terminal"))
}

/// Web tools (`search_web`, `read_url_content`), not the browser.
fn is_web_tool(name: &str) -> bool {
    !name.contains("browser") && (name.contains("web") || name.contains("url"))
}

/// The LLM tokens of a `result` event's usage.
pub fn tokens(usage: &Value) -> Option<u64> {
    usage["total_tokens"].as_u64().or_else(|| {
        let counts: Vec<u64> = ["input_tokens", "output_tokens", "thinking_tokens"]
            .iter()
            .filter_map(|k| usage[k].as_u64())
            .collect();
        (!counts.is_empty()).then(|| counts.iter().sum())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn levels_and_arguments() {
        assert_eq!(access(None).unwrap(), Access::SwarmOnly);
        assert_eq!(access(Some("workspace")).unwrap(), Access::Workspace);
        assert!(access(Some("inherit")).is_err());
        assert!(check_extra_args(&s(&["--effort=high"])).is_ok());
        assert!(check_extra_args(&s(&["--dangerously-skip-permissions"])).is_err());
        assert!(check_extra_args(&s(&["--mode=auto"])).is_err());
        assert_eq!(
            Lock::of(Access::SwarmOnly, &s(&["shell"]), &s(&["crm"])),
            Lock::default()
        );
        let lock = Lock::of(Access::Workspace, &s(&["web"]), &s(&["crm"]));
        assert!(lock.workspace && lock.web && !lock.shell);
        assert_eq!(
            lock.gate_args(),
            s(&["--workspace", "--web", "--server", "crm"])
        );
    }

    #[test]
    fn swarm_only_rules_deny_files_commands_and_urls() {
        let rules = permissions(&Lock::default());
        assert_eq!(rules["allow"], json!(["mcp(wats/*)"]));
        for rule in ["read_file(*)", "write_file(*)", "command(*)", "read_url(*)"] {
            assert!(
                rules["deny"].as_array().unwrap().contains(&json!(rule)),
                "{rule}"
            );
        }
        let lock = Lock::of(Access::Workspace, &s(&["shell"]), &s(&["crm"]));
        let rules = permissions(&lock);
        assert_eq!(rules["allow"], json!(["mcp(wats/*)", "mcp(crm/*)"]));
        assert_eq!(rules["deny"], json!(["read_url(*)", "execute_url(*)"]));
    }

    #[test]
    fn the_gate_lets_only_what_the_lock_lends() {
        let call = |name: &str, args: Value| json!({"toolCall": {"name": name, "args": args}});
        let swarm = Lock::default();
        for ok in [
            call("mcp_wats_post", json!({})),
            call(
                "call_mcp_tool",
                json!({"ServerName": "wats", "ToolName": "post"}),
            ),
        ] {
            assert_eq!(decide(&ok, &swarm)["decision"], "allow", "{ok}");
        }
        for no in [
            call("run_command", json!({"CommandLine": "ls"})),
            call("read_file", json!({"Path": "/etc/hosts"})),
            call("call_mcp_tool", json!({"ServerName": "github"})),
            call("browser_navigate", json!({})),
            call("send_message", json!({})),
            json!({}),
        ] {
            assert_eq!(decide(&no, &swarm)["decision"], "deny", "{no}");
        }
        let workspace = Lock::of(Access::Workspace, &[], &s(&["crm"]));
        for ok in [
            call("read_file", json!({"Path": "a.rs"})),
            call("write_to_file", json!({})),
            call("call_mcp_tool", json!({"ServerName": "crm"})),
        ] {
            assert_eq!(decide(&ok, &workspace)["decision"], "allow", "{ok}");
        }
        for no in [
            call("run_command", json!({})),
            call("search_web", json!({})),
            call("execute_browser_javascript", json!({})),
            call("invoke_subagent", json!({})),
            call("generate_image", json!({})),
        ] {
            assert_eq!(decide(&no, &workspace)["decision"], "deny", "{no}");
        }
        let lent = Lock::of(Access::Workspace, &s(&["shell", "web"]), &[]);
        for ok in [
            "run_command",
            "send_command_input",
            "search_web",
            "read_url_content",
        ] {
            assert_eq!(
                decide(&call(ok, json!({})), &lent)["decision"],
                "allow",
                "{ok}"
            );
        }
        assert_eq!(
            decide(&call("browser_navigate", json!({})), &lent)["decision"],
            "deny"
        );
    }

    #[test]
    fn the_home_holds_wats_and_the_lent_servers_with_the_locks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(settings_path(&home), r#"{"model": "flash"}"#).unwrap();
        let lent = vec![("crm".to_string(), json!({"command": "crm"}))];
        let lock = Lock::of(Access::Workspace, &[], &s(&["crm"]));
        prepare_home(&Home {
            home: &home,
            wats_command: Path::new("/opt/my wats"),
            wats_args: &s(&["mcp"]),
            token: "tok",
            real_home: Some(Path::new("/Users/a")),
            lock: &lock,
            lent: &lent,
        })
        .unwrap();
        let read = |p: PathBuf| -> Value {
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let mcp = read(mcp_config_path(&home));
        assert_eq!(mcp["mcpServers"].as_object().unwrap().len(), 2);
        assert_eq!(mcp["mcpServers"]["wats"]["env"]["WATS_RUNNER_TOKEN"], "tok");
        assert_eq!(mcp["mcpServers"]["wats"]["env"]["HOME"], "/Users/a");
        let settings = read(settings_path(&home));
        assert_eq!(settings["model"], "flash");
        assert_eq!(
            settings["permissions"]["allow"],
            json!(["mcp(wats/*)", "mcp(crm/*)"])
        );
        let hooks = read(hooks_path(&home));
        assert_eq!(
            hooks["wats-gate"]["PreToolUse"][0]["hooks"][0]["command"],
            "'/opt/my wats' gate antigravity --workspace --server crm"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&mcp_config_path(&home)), 0o600);
            assert_eq!(mode(&home), 0o700);
        }
        prepare_login(&home, Path::new("/w")).unwrap();
        assert!(
            read(mcp_config_path(&home))["mcpServers"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn runs_and_results() {
        let args = run_args(&Run {
            prompt: "hi",
            sandbox: true,
            timeout: std::time::Duration::from_secs(1500),
            model: Some("gemini-3.8-flash"),
        });
        assert_eq!(
            args,
            [
                "-p",
                "hi",
                "--output-format",
                "stream-json",
                "--print-timeout",
                "1500s",
                "--sandbox",
                "--model=gemini-3.8-flash"
            ]
        );
        assert_eq!(
            tokens(&json!({"input_tokens": 10, "output_tokens": 2, "total_tokens": 15})),
            Some(15)
        );
        assert_eq!(
            tokens(&json!({"input_tokens": 10, "output_tokens": 2})),
            Some(12)
        );
        assert_eq!(tokens(&json!({})), None);

        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("a/b");
        std::fs::create_dir_all(&run).unwrap();
        assert!(check_ancestors(&run).is_ok());
        std::fs::create_dir_all(dir.path().join("a/.agents")).unwrap();
        assert!(check_ancestors(&run).is_err());
    }
}
