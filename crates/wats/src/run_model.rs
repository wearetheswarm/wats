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
//!    - Mistral Vibe, at `inherit`: `active_model` of its `config.toml`
//!      (at `swarm-only` its own home sets none: Vibe's default).
//!    - Copilot CLI: `COPILOT_MODEL`, else at `inherit` `model` of its
//!      `config.json`.

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
    /// `agent.effort`.
    pub effort: Option<&'a str>,
    pub extra_args: &'a [String],
    /// The agent's directory (its own OpenCode config lives there).
    pub work_dir: &'a Path,
    /// The workspace.
    pub workdir: &'a Path,
    /// `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: Option<PathBuf>,
    /// The user's OpenCode global config directory (`~/.config/opencode`).
    pub opencode_global: Option<PathBuf>,
    /// The user's Vibe home (`$VIBE_HOME`, or `~/.vibe`).
    pub vibe_home: Option<PathBuf>,
    /// The user's Copilot home (`$COPILOT_HOME`, or `~/.copilot`).
    pub copilot_home: Option<PathBuf>,
    /// `COPILOT_MODEL`, which Copilot runs keep.
    pub copilot_model_env: Option<String>,
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
        // Only where `agent.effort` reaches the run.
        effort: non_empty(r.effort.map(str::to_string))
            .filter(|_| crate::agent::takes_effort(r.runtime)),
    };
    match r.runtime {
        "codex" => {
            // An extra `-c` wins over `agent.effort` (which then passes
            // nothing), and both over the user's config.toml.
            out.effort =
                non_empty(codex_override(r.extra_args, "model_reasoning_effort")).or(out.effort);
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
        // At swarm-only, Vibe's own home holds no model: its default.
        "vibe" if out.model.is_none() && r.access == Access::Inherit => {
            out.model = r
                .vibe_home
                .as_deref()
                .and_then(|h| std::fs::read_to_string(h.join("config.toml")).ok())
                .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                .and_then(|t| non_empty(t.get("active_model")?.as_str().map(str::to_string)));
        }
        "copilot" if out.model.is_none() => {
            out.model = non_empty(r.copilot_model_env.clone()).or_else(|| {
                (r.access == Access::Inherit)
                    .then(|| {
                        let text =
                            std::fs::read_to_string(r.copilot_home.as_ref()?.join("config.json"))
                                .ok()?;
                        let config: Value = serde_json::from_str(&text).ok()?;
                        non_empty(config["model"].as_str().map(str::to_string))
                    })
                    .flatten()
            });
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
            effort: None,
            extra_args: extra,
            work_dir: dir,
            workdir: dir,
            codex_home: Some(dir.join("codex")),
            opencode_global: Some(dir.join("global")),
            vibe_home: Some(dir.join("vibe")),
            copilot_home: Some(dir.join("copilot")),
            copilot_model_env: None,
        }
    }

    #[test]
    fn vibe_and_copilot_read_the_user_config_at_inherit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vibe")).unwrap();
        std::fs::write(
            dir.path().join("vibe/config.toml"),
            "active_model = \"devstral-2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("copilot")).unwrap();
        std::fs::write(
            dir.path().join("copilot/config.json"),
            r#"{"model": "gpt-6"}"#,
        )
        .unwrap();
        let none: Vec<String> = Vec::new();
        let model = |runtime, access| resolve(&run(runtime, access, dir.path(), &none)).model;
        assert_eq!(
            model("vibe", Access::Inherit).as_deref(),
            Some("devstral-2")
        );
        assert_eq!(model("vibe", Access::SwarmOnly), None);
        assert_eq!(model("copilot", Access::Inherit).as_deref(), Some("gpt-6"));
        assert_eq!(model("copilot", Access::SwarmOnly), None);
        let mut env = run("copilot", Access::SwarmOnly, dir.path(), &none);
        env.copilot_model_env = Some("claude-sonnet-5".into());
        assert_eq!(resolve(&env).model.as_deref(), Some("claude-sonnet-5"));
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
        // agent.effort: over config.toml, under an extra -c.
        let mut inherit = run("codex", Access::Inherit, dir.path(), &none);
        inherit.effort = Some("low");
        assert_eq!(resolve(&inherit).effort.as_deref(), Some("low"));
        let mut both = run("codex", Access::Workspace, dir.path(), &extra);
        both.effort = Some("low");
        assert_eq!(resolve(&both).effort.as_deref(), Some("high"));
        // A runtime that takes no effort does not claim one.
        let mut vibe = run("vibe", Access::SwarmOnly, dir.path(), &none);
        vibe.effort = Some("low");
        assert_eq!(resolve(&vibe).effort, None);
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
        for runtime in [
            "codex",
            "opencode",
            "claude",
            "vibe",
            "copilot",
            "antigravity",
        ] {
            assert_eq!(
                resolve(&run(runtime, Access::Workspace, dir.path(), &none)),
                Resolved::default(),
                "{runtime}"
            );
        }
    }
}
