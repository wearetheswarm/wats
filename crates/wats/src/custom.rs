//! Any other agent CLI: the `custom` runtime (ADR 0039).
//!
//! An `[agents.<name>]` table gives the command, an argument template and
//! the format of the per-run MCP config. wats renders the template, writes
//! the MCP file (the wats server only, `0600`, removed after the run),
//! runs the command and reads nothing of its output: the agent answers
//! through the wats MCP tools. wats cannot confine such a CLI, so a custom
//! agent needs `agent.review` or `agent.allow` (ADR 0039 §3).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;

/// The runtime's name in config.toml.
pub const RUNTIME: &str = "custom";

/// Said wherever a custom agent's access is shown.
pub const UNCONFINED: &str =
    "unconfined: wats can't confine this CLI, so a post can ask it for anything it can do";

/// The placeholders of `agent.args`.
pub const PLACEHOLDERS: [&str; 6] = [
    "prompt",
    "prompt_file",
    "mcp_config",
    "cwd",
    "model",
    "wats_mcp",
];

/// How the per-run MCP config is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `{"mcpServers": {"wats": {...}}}` (Claude Code, Cursor and others).
    #[default]
    ClaudeJson,
    /// `[mcp_servers.wats]` (Codex style).
    CodexToml,
    /// None: the CLI's own configuration runs `wats mcp`; the run's token
    /// is in its environment.
    None,
}

pub const FORMATS: [&str; 3] = ["claude-json", "codex-toml", "none"];

impl std::str::FromStr for Format {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "claude-json" => Ok(Format::ClaudeJson),
            "codex-toml" => Ok(Format::CodexToml),
            "none" => Ok(Format::None),
            other => bail!(
                "agent.mcp_format {other:?} is one of: {}",
                FORMATS.join(", ")
            ),
        }
    }
}

/// The placeholders an argument holds, in order (`{name}`; `{{` is a
/// literal brace).
fn placeholders(arg: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = arg;
    while let Some(start) = rest.find('{') {
        if rest[start..].starts_with("{{") {
            rest = &rest[start + 2..];
            continue;
        }
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        out.push(&rest[start + 1..start + len]);
        rest = &rest[start + len + 1..];
    }
    out
}

/// Refuse an unknown placeholder, naming it.
pub fn check_args(args: &[String]) -> Result<()> {
    for arg in args {
        for name in placeholders(arg) {
            if !PLACEHOLDERS.contains(&name) {
                bail!(
                    "agent.args {arg:?}: unknown placeholder {{{name}}} (known: {})",
                    PLACEHOLDERS.map(|p| format!("{{{p}}}")).join(", ")
                );
            }
        }
    }
    Ok(())
}

/// Whether the template uses a placeholder.
pub fn uses(args: &[String], name: &str) -> bool {
    args.iter().any(|a| placeholders(a).contains(&name))
}

/// Whether the prompt goes on stdin: neither `{prompt}` nor
/// `{prompt_file}` in the template.
pub fn prompt_on_stdin(args: &[String]) -> bool {
    !uses(args, "prompt") && !uses(args, "prompt_file")
}

/// What the placeholders stand for in one run.
pub struct Values<'a> {
    pub prompt: &'a str,
    pub prompt_file: &'a Path,
    pub mcp_config: Option<&'a Path>,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    /// The `wats mcp` command line, as one shell-quoted string.
    pub wats_mcp: &'a str,
}

/// The template with its placeholders filled. An argument that holds
/// `{model}` is dropped when no model is set, and so is one with
/// `{mcp_config}` when the format writes none.
pub fn render(args: &[String], v: &Values) -> Vec<String> {
    let mut out = Vec::new();
    'arg: for arg in args {
        let mut rendered = String::new();
        let mut rest = arg.as_str();
        while let Some(start) = rest.find('{') {
            rendered.push_str(&rest[..start]);
            if rest[start..].starts_with("{{") {
                rendered.push('{');
                rest = &rest[start + 2..];
                continue;
            }
            let Some(len) = rest[start..].find('}') else {
                break;
            };
            let name = &rest[start + 1..start + len];
            let value = match name {
                "prompt" => v.prompt.to_string(),
                "prompt_file" => v.prompt_file.display().to_string(),
                "mcp_config" => match v.mcp_config {
                    Some(path) => path.display().to_string(),
                    None => continue 'arg,
                },
                "cwd" => v.cwd.display().to_string(),
                "model" => match v.model {
                    Some(model) => model.to_string(),
                    None => continue 'arg,
                },
                "wats_mcp" => v.wats_mcp.to_string(),
                other => format!("{{{other}}}"),
            };
            rendered.push_str(&value);
            rest = &rest[start + len + 1..];
        }
        rendered.push_str(&rest.replace("}}", "}"));
        out.push(rendered);
    }
    out
}

/// Where a run's MCP config goes, for a format that writes one: in the
/// run directory at `agent.mcp_file` (`.cursor/mcp.json`: CLIs that read
/// a project config and take no file flag), else in the agent's
/// directory, for `{mcp_config}`.
pub fn mcp_path(
    format: Format,
    work_dir: &Path,
    run_dir: &Path,
    mcp_file: Option<&str>,
) -> Option<PathBuf> {
    if format == Format::None {
        return None;
    }
    if let Some(file) = mcp_file {
        return Some(run_dir.join(file));
    }
    Some(work_dir.join(match format {
        Format::CodexToml => "custom-mcp.toml",
        _ => "custom-mcp.json",
    }))
}

/// Refuse an `agent.mcp_file` that is not a plain relative path.
pub fn check_mcp_file(file: &str) -> Result<()> {
    let path = Path::new(file);
    if file.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!(
            "agent.mcp_file {file:?}: a path inside the run directory, such as \
             .cursor/mcp.json"
        );
    }
    Ok(())
}

/// The run directory of a custom agent: its own empty directory (never
/// the node's), `<agent dir>/run`.
pub fn run_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("run")
}

/// Where a run's prompt file goes (`{prompt_file}`).
pub fn prompt_path(work_dir: &Path) -> PathBuf {
    work_dir.join("custom-prompt.txt")
}

/// Write the MCP config of one run: the wats server only, with the run's
/// token, `0600`.
pub fn write_mcp(
    format: Format,
    path: &Path,
    command: &Path,
    args: &[String],
    token: &str,
) -> Result<()> {
    let text = match format {
        Format::ClaudeJson => serde_json::to_string_pretty(&json!({"mcpServers": {"wats": {
            "type": "stdio",
            "command": command,
            "args": args,
            "env": {"WATS_RUNNER_TOKEN": token},
        }}}))?,
        Format::CodexToml => {
            let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
            format!(
                "[mcp_servers.wats]\ncommand = {}\nargs = [{}]\n\n[mcp_servers.wats.env]\n\
                 WATS_RUNNER_TOKEN = {}\n",
                quote(&command.to_string_lossy()),
                args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(", "),
                quote(token)
            )
        }
        Format::None => return Ok(()),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    crate::access::write_private(path, text.as_bytes())
}

/// `agent.env` entries (`KEY=VALUE`) as pairs.
pub fn env_pairs(entries: &[String]) -> Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|e| match e.split_once('=') {
            Some((k, v))
                if !k.trim().is_empty()
                    && k.trim()
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                Ok((k.trim().to_string(), v.to_string()))
            }
            _ => bail!("agent.env {e:?}: not KEY=VALUE"),
        })
        .collect()
}

/// A custom agent needs `agent.review` or `agent.allow` (D1), a command,
/// and a template whose placeholders it knows.
pub fn check(
    command: Option<&Path>,
    args: &[String],
    allow: &[String],
    review: bool,
) -> Result<()> {
    if command.is_none_or(|c| c.as_os_str().is_empty()) {
        bail!("a custom agent needs agent.command, the CLI to run (`wats setup custom --agent`)");
    }
    check_args(args)?;
    if allow.is_empty() && !review {
        bail!(
            "a custom agent is {UNCONFINED}: set agent.review = true (its posts wait for you) \
             or agent.allow (the authors who may wake it) first"
        );
    }
    Ok(())
}

/// The `data.via` runtime of a custom agent: `custom:<name>`.
pub fn via_runtime(name: &str) -> String {
    format!("{RUNTIME}:{name}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn templates_and_placeholders() {
        let args = s(&[
            "-p",
            "--mcp-config",
            "{mcp_config}",
            "--model={model}",
            "{{x}}",
        ]);
        assert!(check_args(&args).is_ok());
        assert!(prompt_on_stdin(&args));
        let e = check_args(&s(&["{promt}"])).unwrap_err();
        assert!(e.to_string().contains("{promt}"), "{e}");
        let v = Values {
            prompt: "hi",
            prompt_file: Path::new("/w/p.txt"),
            mcp_config: Some(Path::new("/w/m.json")),
            cwd: Path::new("/w"),
            model: None,
            wats_mcp: "/bin/wats mcp",
        };
        assert_eq!(
            render(&args, &v),
            ["-p", "--mcp-config", "/w/m.json", "{x}"]
        );
        let v = Values {
            model: Some("m1"),
            mcp_config: None,
            ..v
        };
        assert_eq!(
            render(&args, &v),
            ["-p", "--mcp-config", "--model=m1", "{x}"]
        );
        let args = s(&[
            "--message-file",
            "{prompt_file}",
            "--ext",
            "{wats_mcp}",
            "{prompt}",
        ]);
        assert!(!prompt_on_stdin(&args));
        assert_eq!(
            render(&args, &v),
            ["--message-file", "/w/p.txt", "--ext", "/bin/wats mcp", "hi"]
        );
    }

    #[test]
    fn mcp_files_hold_wats_only() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = mcp_path(Format::ClaudeJson, dir.path(), dir.path(), None).unwrap();
        write_mcp(
            Format::ClaudeJson,
            &json_path,
            Path::new("/bin/wats"),
            &s(&["mcp"]),
            "t",
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["wats"]["env"]["WATS_RUNNER_TOKEN"], "t");
        let toml_path = mcp_path(Format::CodexToml, dir.path(), dir.path(), None).unwrap();
        write_mcp(
            Format::CodexToml,
            &toml_path,
            Path::new("/bin/wats"),
            &s(&["mcp"]),
            "t",
        )
        .unwrap();
        let t: toml::Table = toml::from_str(&std::fs::read_to_string(&toml_path).unwrap()).unwrap();
        assert_eq!(
            t["mcp_servers"]["wats"]["env"]["WATS_RUNNER_TOKEN"].as_str(),
            Some("t")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&json_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(mcp_path(Format::None, dir.path(), dir.path(), None), None);
        assert_eq!(
            mcp_path(
                Format::ClaudeJson,
                Path::new("/a"),
                Path::new("/a/run"),
                Some(".cursor/mcp.json")
            ),
            Some(PathBuf::from("/a/run/.cursor/mcp.json"))
        );
        assert!(check_mcp_file(".kiro/settings/mcp.json").is_ok());
        for bad in ["/etc/x", "../x", "", "a/../../b"] {
            assert!(check_mcp_file(bad).is_err(), "{bad}");
        }
        assert_eq!("none".parse::<Format>().unwrap(), Format::None);
        assert!("yaml".parse::<Format>().is_err());
    }

    #[test]
    fn guardrails_env_and_via() {
        let cmd = Some(Path::new("cursor-agent"));
        assert!(check(cmd, &[], &[], false).is_err());
        assert!(check(cmd, &[], &[], true).is_ok());
        assert!(check(cmd, &[], &s(&["ab"]), false).is_ok());
        assert!(check(None, &[], &[], true).is_err());
        assert!(check(cmd, &s(&["{nope}"]), &[], true).is_err());
        assert_eq!(
            env_pairs(&s(&["A=1", "B_2=x=y"])).unwrap(),
            [
                ("A".to_string(), "1".to_string()),
                ("B_2".to_string(), "x=y".to_string())
            ]
        );
        assert!(env_pairs(&s(&["no value"])).is_err());
        assert_eq!(via_runtime("cursor"), "custom:cursor");
    }
}
