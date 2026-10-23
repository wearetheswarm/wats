//! GitHub Copilot CLI as the node's agent (ADR 0038 §1).
//!
//! Each run spawns, from the run directory:
//!
//!     copilot -p <prompt> --output-format json --no-ask-user
//!             --no-auto-update --allow-all-tools [level flags] [extra args]
//!
//! - **swarm-only** (the default): `COPILOT_HOME` is the agent's own, its
//!   `mcp-config.json` holds the wats server only; `--disable-builtin-mcps`,
//!   `--available-tools wats`, and `shell`, `write` and `url` denied
//!   (a deny wins over `--allow-all-tools`).
//! - **workspace**: the same home, with the lent servers (ADR 0037); the
//!   run starts in the workspace, Copilot's path checks keep file tools
//!   inside it; `shell` and `url` stay denied unless `agent.tools` lists
//!   them.
//! - **inherit**: the user's own `~/.copilot`, with the wats server added
//!   by `--additional-mcp-config`; needs `agent.review` or `agent.allow`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::access::Access;

/// The server name of wats in a run's MCP config.
pub const SERVER: &str = "wats";

/// Said when inherit is chosen.
pub const INHERIT_WARNING: &str = "Copilot CLI with inherit uses your own ~/.copilot: your MCP \
     servers, the builtin GitHub server and every tool, with your GitHub account";

/// Copilot's default access level: swarm-only (ADR 0038 §1).
pub fn access(value: Option<&str>) -> Result<Access> {
    value.map_or(Ok(Access::SwarmOnly), str::parse)
}

/// `inherit` is refused without `agent.review` or `agent.allow`.
pub fn check_access(access: Access, allow: &[String], review: bool) -> Result<()> {
    if access == Access::Inherit && allow.is_empty() && !review {
        bail!(
            "agent.access = inherit with Copilot CLI hands swarm posts to your own Copilot \
             ({INHERIT_WARNING}): set agent.review = true (its posts wait for you) or \
             agent.allow (the authors who may wake it) first, or use swarm-only or workspace"
        );
    }
    Ok(())
}

/// Arguments wats sets itself, or that would undo a level: refused in
/// `agent.args`.
pub const REFUSED_ARGS: [&str; 16] = [
    "-p",
    "--prompt",
    "-i",
    "--interactive",
    "--output-format",
    "--allow-all",
    "--yolo",
    "--allow-all-paths",
    "--allow-all-urls",
    "--add-dir",
    "--available-tools",
    "--additional-mcp-config",
    "--trust-folder",
    "--resume",
    "--continue",
    "--config-dir",
];

/// Refuse `agent.args` that wats sets itself or that would undo a level.
pub fn check_extra_args(extra: &[String]) -> Result<()> {
    for arg in extra {
        let flag = arg.split('=').next().unwrap_or(arg);
        if REFUSED_ARGS.contains(&flag) || flag.starts_with("-r") && flag.len() == 2 {
            bail!(
                "agent.args {arg:?}: wats sets the Copilot CLI flags of a run itself \
                 (ADR 0038); remove it"
            );
        }
    }
    Ok(())
}

/// The agent's own Copilot home (`COPILOT_HOME` of its runs).
pub fn agent_home(work_dir: &Path) -> PathBuf {
    work_dir.join("copilot-home")
}

/// The wats entry of a run's MCP config: the run's token in its env.
pub fn wats_entry(command: &Path, args: &[String], token: &str) -> Value {
    json!({
        "type": "local",
        "command": command,
        "args": args,
        "env": {"WATS_RUNNER_TOKEN": token},
        "tools": ["*"],
    })
}

/// The `mcpServers` of a run: wats, and at `workspace` the lent servers
/// (a lent one named `wats` is left out).
pub fn run_servers(
    access: Access,
    wats: Value,
    lent: &[(String, Value)],
) -> serde_json::Map<String, Value> {
    let mut servers = serde_json::Map::new();
    if access == Access::Workspace {
        for (name, definition) in lent {
            if name != SERVER {
                servers.insert(name.clone(), definition.clone());
            }
        }
    }
    servers.insert(SERVER.into(), wats);
    servers
}

/// Write `{"mcpServers": …}` to `path`, `0600` (it holds the runner
/// token and the lent servers' secrets).
pub fn write_mcp_config(path: &Path, servers: &serde_json::Map<String, Value>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    crate::access::write_private(
        path,
        &serde_json::to_vec_pretty(&json!({"mcpServers": servers}))?,
    )
}

/// Inputs of one run's arguments.
pub struct Run<'a> {
    pub access: Access,
    pub prompt: &'a str,
    /// `agent.tools` (workspace).
    pub tools: &'a [String],
    /// `--additional-mcp-config @<file>` at inherit.
    pub inherit_config: Option<&'a Path>,
    pub model: Option<&'a str>,
    pub usage_file: &'a Path,
}

/// The base of an `agent.tools` entry (`shell(git:*)` → `shell`).
fn base(tool: &str) -> &str {
    tool.split('(').next().unwrap_or(tool).trim()
}

/// Whether `agent.tools` lends a kind (`shell`, `url`).
fn lends(tools: &[String], kind: &str) -> bool {
    tools.iter().any(|t| base(t).eq_ignore_ascii_case(kind))
}

/// `copilot` arguments of one run, before the extra arguments.
pub fn run_args(r: &Run) -> Vec<String> {
    let mut out: Vec<String> = vec![
        "-p".into(),
        r.prompt.into(),
        "--output-format".into(),
        "json".into(),
        "--no-ask-user".into(),
        "--no-auto-update".into(),
        "--allow-all-tools".into(),
        "--usage-output-file".into(),
        r.usage_file.display().to_string(),
    ];
    let mut deny: Vec<&str> = Vec::new();
    match r.access {
        Access::SwarmOnly => {
            out.extend([
                "--disable-builtin-mcps".into(),
                "--disallow-temp-dir".into(),
                "--no-custom-instructions".into(),
                format!("--available-tools={SERVER}"),
            ]);
            deny.extend(["shell", "write", "url"]);
        }
        Access::Workspace => {
            out.extend([
                "--disable-builtin-mcps".into(),
                "--disallow-temp-dir".into(),
            ]);
            if !lends(r.tools, "shell") {
                deny.push("shell");
            }
            if !lends(r.tools, "url") {
                deny.push("url");
            }
        }
        Access::Inherit => {
            if let Some(path) = r.inherit_config {
                out.push(format!("--additional-mcp-config=@{}", path.display()));
            }
        }
    }
    for kind in deny {
        out.push(format!("--deny-tool={kind}"));
    }
    if let Some(model) = r.model {
        out.push(format!("--model={model}"));
    }
    out
}

/// Where a run's usage file goes (`--usage-output-file`).
pub fn usage_path(work_dir: &Path) -> PathBuf {
    work_dir.join("copilot-usage.json")
}

/// The longest prompt passed in `-p`: well under the argument limits.
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// The prompt, cut to [`MAX_PROMPT_BYTES`] at a character boundary.
pub fn fit_prompt(prompt: &str) -> String {
    if prompt.len() <= MAX_PROMPT_BYTES {
        return prompt.to_string();
    }
    let mut end = MAX_PROMPT_BYTES;
    while !prompt.is_char_boundary(end) {
        end -= 1;
    }
    prompt[..end].to_string()
}

/// The transcript line of a run's usage file: `{"type": "copilot_usage",
/// "ai_credits", "premium_requests", "usage"}`. None without the file.
pub fn usage_line(path: &Path) -> Option<Value> {
    let usage: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let premium = usage["totalPremiumRequests"]
        .as_f64()
        .or_else(|| usage["premiumRequests"].as_f64());
    Some(json!({
        "type": "copilot_usage",
        "ai_credits": ai_credits(&usage),
        "premium_requests": premium,
        "usage": usage,
    }))
}

/// The variables a swarm-only or workspace run removes: every
/// `COPILOT_*` but the token and the model (`COPILOT_ALLOW_ALL=true`
/// would trust the run directory, `COPILOT_HOME` move the home).
pub fn removed_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    vars.map(|(k, _)| k)
        .filter(|k| {
            let k = k.to_string_lossy();
            k.starts_with("COPILOT_")
                && !matches!(k.as_ref(), "COPILOT_GITHUB_TOKEN" | "COPILOT_MODEL")
        })
        .collect()
}

/// The AI credits of a usage file (`--usage-output-file`), or of a
/// `session.usage_checkpoint` event: `totalNanoAiu` / 10⁹.
pub fn ai_credits(value: &Value) -> Option<f64> {
    let nano = value["totalNanoAiu"]
        .as_u64()
        .or_else(|| value["data"]["totalNanoAiu"].as_u64())
        .or_else(|| value["usage"]["totalNanoAiu"].as_u64())?;
    Some(nano as f64 / 1e9)
}

/// The model and effort of a run, from its `user.message` event
/// (`responsesReasoning`), for `data.via` and the run record.
pub fn model_of(event: &Value) -> Option<(String, Option<String>)> {
    if event["type"] != "user.message" {
        return None;
    }
    let reasoning = &event["data"]["responsesReasoning"];
    let model = reasoning["model"].as_str()?.to_string();
    Some((model, reasoning["effort"].as_str().map(str::to_string)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn args(access: Access, tools: &[String]) -> Vec<String> {
        run_args(&Run {
            access,
            prompt: "hello",
            tools,
            inherit_config: Some(Path::new("/w/inherit.json")),
            model: Some("gpt-6"),
            usage_file: Path::new("/w/usage.json"),
        })
    }

    #[test]
    fn swarm_only_is_the_wats_tools_only() {
        assert_eq!(access(None).unwrap(), Access::SwarmOnly);
        let a = args(Access::SwarmOnly, &s(&["shell"]));
        assert_eq!(&a[..2], ["-p", "hello"]);
        for flag in [
            "--disable-builtin-mcps",
            "--available-tools=wats",
            "--deny-tool=shell",
            "--deny-tool=write",
            "--deny-tool=url",
            "--no-ask-user",
            "--model=gpt-6",
        ] {
            assert!(a.contains(&flag.to_string()), "{flag}: {a:?}");
        }
        assert!(!a.iter().any(|x| x.starts_with("--additional-mcp-config")));
    }

    #[test]
    fn workspace_lends_what_agent_tools_lists() {
        let a = args(Access::Workspace, &[]);
        assert!(a.contains(&"--deny-tool=shell".to_string()), "{a:?}");
        assert!(a.contains(&"--deny-tool=url".to_string()), "{a:?}");
        assert!(!a.iter().any(|x| x.starts_with("--available-tools")));
        let a = args(Access::Workspace, &s(&["shell", "url"]));
        assert!(!a.contains(&"--deny-tool=shell".to_string()), "{a:?}");
        assert!(!a.contains(&"--deny-tool=url".to_string()), "{a:?}");
    }

    #[test]
    fn inherit_adds_wats_and_needs_guardrails() {
        let a = args(Access::Inherit, &[]);
        assert!(a.contains(&"--additional-mcp-config=@/w/inherit.json".to_string()));
        assert!(!a.contains(&"--disable-builtin-mcps".to_string()));
        assert!(check_access(Access::Inherit, &[], false).is_err());
        assert!(check_access(Access::Inherit, &[], true).is_ok());
        assert!(check_access(Access::Inherit, &s(&["ab"]), false).is_ok());
        assert!(check_access(Access::Workspace, &[], false).is_ok());
    }

    #[test]
    fn refused_extra_args() {
        assert!(check_extra_args(&s(&["--reasoning-effort", "high"])).is_ok());
        for bad in [
            "--yolo",
            "--allow-all-paths",
            "--add-dir=/",
            "-r",
            "--available-tools=x",
        ] {
            assert!(check_extra_args(&s(&[bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn servers_env_and_usage() {
        let wats = wats_entry(Path::new("/bin/wats"), &s(&["mcp"]), "tok");
        assert_eq!(wats["env"]["WATS_RUNNER_TOKEN"], "tok");
        let lent = vec![(
            "crm".to_string(),
            json!({"type": "local", "command": "crm"}),
        )];
        assert_eq!(run_servers(Access::Workspace, wats.clone(), &lent).len(), 2);
        assert_eq!(run_servers(Access::SwarmOnly, wats, &lent).len(), 1);

        let vars = [
            ("COPILOT_ALLOW_ALL", "true"),
            ("COPILOT_HOME", "/x"),
            ("COPILOT_GITHUB_TOKEN", "t"),
            ("PATH", "/bin"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        assert_eq!(
            removed_env(vars.into_iter()),
            ["COPILOT_ALLOW_ALL", "COPILOT_HOME"]
        );

        let checkpoint = json!({"type": "session.usage_checkpoint",
            "data": {"totalNanoAiu": 177_617_500u64, "totalPremiumRequests": 1}});
        assert!((ai_credits(&checkpoint).unwrap() - 0.1776175).abs() < 1e-9);
        let message = json!({"type": "user.message", "data": {"responsesReasoning":
            {"model": "gpt-6-luna", "effort": "medium"}}});
        assert_eq!(
            model_of(&message),
            Some(("gpt-6-luna".to_string(), Some("medium".to_string())))
        );
    }
}
