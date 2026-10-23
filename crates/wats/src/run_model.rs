//! The model (and effort) a run will use, for `data.via` (ADR 0036): Codex
//! and OpenCode never report it in their JSON streams, so it is resolved
//! before the run from what the run will read. Nothing is guessed: when
//! the runtime falls back to its own built-in default, the model stays
//! unknown.
//!
//! In order:
//! 1. a model the extra arguments choose (`--model X`, `-m X`,
//!    `--model=X`), the one the CLI then uses;
//! 2. `agent.model`;
//! 3. the configuration the run reads:
//!    - Codex, at `inherit` only: `model` and `model_reasoning_effort` of
//!      `$CODEX_HOME/config.toml`. The other levels pass
//!      `--ignore-user-config`, so the user's choice does not apply.
//!    - OpenCode: `model` of the workspace's `opencode.json(c)` (at
//!      `workspace`), else of the agent's own OpenCode directory (at
//!      `swarm-only` and `workspace`), else of the user's global config
//!      (at `inherit`).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::access::Access;

/// What a run will use: a model and an effort, each when known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Inputs of [`resolve`].
pub struct Run<'a> {
    pub runtime: &'a str,
    pub access: Access,
    pub model: Option<&'a str>,
    pub extra_args: &'a [String],
    /// The agent's directory (its own OpenCode config lives there).
    pub work_dir: &'a Path,
    /// The workspace.
    pub workdir: &'a Path,
    /// `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: Option<PathBuf>,
    /// The user's OpenCode global config directory (`~/.config/opencode`).
    pub opencode_global: Option<PathBuf>,
}

/// A value the extra arguments give to `long` or `short` (`--model X`,
/// `-m X`, `--model=X`).
fn arg_value(args: &[String], long: &str, short: Option<&str>) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == long || short.is_some_and(|s| arg == s) {
            return it.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&format!("{long}=")) {
            return Some(value.to_string());
        }
    }
    None
}

/// A `-c key=value` (Codex) override among the extra arguments.
fn codex_override(args: &[String], key: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value = if arg == "-c" || arg == "--config" {
            it.next().map(String::as_str)
        } else {
            arg.strip_prefix("--config=")
        };
        if let Some((k, v)) = value.and_then(|v| v.split_once('='))
            && k.trim() == key
        {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The `model` key of an OpenCode config file (JSON with comments).
fn opencode_model(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&crate::opencode::strip_jsonc(&text)).ok()?;
    non_empty(config["model"].as_str().map(str::to_string))
}

fn first_opencode_model(dir: &Path, names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| opencode_model(&dir.join(n)))
}

/// The user's global OpenCode config directory: `$XDG_CONFIG_HOME/opencode`,
/// else `~/.config/opencode`.
pub fn opencode_global_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
        .map(|d| d.join("opencode"))
}

/// What the run will use, as far as can be known before it starts.
pub fn resolve(r: &Run) -> Resolved {
    let mut out = Resolved {
        model: non_empty(arg_value(r.extra_args, "--model", Some("-m")))
            .or_else(|| non_empty(r.model.map(str::to_string))),
        effort: None,
    };
    match r.runtime {
        "codex" => {
            out.effort = non_empty(codex_override(r.extra_args, "model_reasoning_effort"));
            if out.model.is_none() {
                out.model = non_empty(codex_override(r.extra_args, "model"));
            }
            if r.access == Access::Inherit
                && let Some(home) = &r.codex_home
                && let Ok(text) = std::fs::read_to_string(home.join("config.toml"))
                && let Ok(table) = toml::from_str::<toml::Table>(&text)
            {
                let get =
                    |k: &str| non_empty(table.get(k).and_then(|v| v.as_str()).map(str::to_string));
                out.model = out.model.or_else(|| get("model"));
                out.effort = out.effort.or_else(|| get("model_reasoning_effort"));
            }
        }
        "opencode" if out.model.is_none() => {
            let names = ["opencode.jsonc", "opencode.json", "config.json"];
            out.model = match r.access {
                Access::Inherit => r
                    .opencode_global
                    .as_deref()
                    .and_then(|d| first_opencode_model(d, &names)),
                level => (level == Access::Workspace)
                    .then(|| {
                        crate::opencode::workspace_files(r.workdir)
                            .iter()
                            .find_map(|p| opencode_model(p))
                    })
                    .flatten()
                    .or_else(|| {
                        first_opencode_model(&crate::opencode::agent_dir(r.work_dir), &names)
                    }),
            };
        }
        _ => {}
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn run<'a>(runtime: &'a str, access: Access, dir: &'a Path, extra: &'a [String]) -> Run<'a> {
        Run {
            runtime,
            access,
            model: None,
            extra_args: extra,
            work_dir: dir,
            workdir: dir,
            codex_home: Some(dir.join("codex")),
            opencode_global: Some(dir.join("global")),
        }
    }

    #[test]
    fn codex_reads_the_user_config_at_inherit_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("codex")).unwrap();
        std::fs::write(
            dir.path().join("codex/config.toml"),
            "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"medium\"\n",
        )
        .unwrap();
        let none: Vec<String> = Vec::new();
        assert_eq!(
            resolve(&run("codex", Access::Inherit, dir.path(), &none)),
            Resolved {
                model: Some("gpt-5.5".into()),
                effort: Some("medium".into())
            }
        );
        // --ignore-user-config: the user's choice does not apply.
        assert_eq!(
            resolve(&run("codex", Access::Workspace, dir.path(), &none)),
            Resolved::default()
        );
        let extra = s(&["-c", "model_reasoning_effort=\"high\"", "--model", "gpt-6"]);
        assert_eq!(
            resolve(&run("codex", Access::Workspace, dir.path(), &extra)),
            Resolved {
                model: Some("gpt-6".into()),
                effort: Some("high".into())
            }
        );
        let mut configured = run("codex", Access::SwarmOnly, dir.path(), &none);
        configured.model = Some("o5");
        assert_eq!(resolve(&configured).model.as_deref(), Some("o5"));
    }

    #[test]
    fn opencode_reads_the_config_its_level_loads() {
        let dir = tempfile::tempdir().unwrap();
        let agent = crate::opencode::agent_dir(dir.path());
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("opencode.jsonc"),
            "{\n  // the agent's\n  \"model\": \"azure/gpt-5\"\n}",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("global")).unwrap();
        std::fs::write(
            dir.path().join("global/opencode.json"),
            r#"{"model": "anthropic/claude-sonnet-5"}"#,
        )
        .unwrap();
        let none: Vec<String> = Vec::new();
        let model = |access| resolve(&run("opencode", access, dir.path(), &none)).model;
        assert_eq!(model(Access::SwarmOnly).as_deref(), Some("azure/gpt-5"));
        assert_eq!(
            model(Access::Inherit).as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        std::fs::write(
            dir.path().join("opencode.json"),
            r#"{"model": "local/qwen"}"#,
        )
        .unwrap();
        assert_eq!(model(Access::Workspace).as_deref(), Some("local/qwen"));
        assert_eq!(model(Access::SwarmOnly).as_deref(), Some("azure/gpt-5"));
        let extra = s(&["--model=x/y"]);
        assert_eq!(
            resolve(&run("opencode", Access::SwarmOnly, dir.path(), &extra))
                .model
                .as_deref(),
            Some("x/y")
        );
    }

    #[test]
    fn nothing_is_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let none: Vec<String> = Vec::new();
        for runtime in ["codex", "opencode", "claude"] {
            assert_eq!(
                resolve(&run(runtime, Access::Workspace, dir.path(), &none)),
                Resolved::default(),
                "{runtime}"
            );
        }
    }
}
