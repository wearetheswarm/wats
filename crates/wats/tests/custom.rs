//! The `custom` runtime (ADR 0039), against a fake CLI: the rendered
//! template, the prompt on stdin or in a file, the per-run MCP config (in
//! the agent's directory or inside the run directory) and its removal
//! after the run, the environment, `data.via`; then `wats setup custom`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use wats::access::Access;
use wats::agent::{AgentConfig, DEFAULT_HEARTBEAT};
use wats::node::{NetworkMode, Node, NodeConfig};

async fn node(dir: &Path) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    Node::start(config).await
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A fake CLI: records its arguments, working directory, the run's MCP
/// file (from `--mcp` or `.cursor/mcp.json`), WATS_RUNNER_TOKEN and FOO,
/// and its stdin, then exits 0.
fn fake(dir: &Path) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = dir.join("fake-cli");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > {out}/args
pwd > {out}/cwd
prev=""
for a in "$@"; do
  [ "$prev" = "--mcp" ] && cp "$a" {out}/mcp.json
  [ "$prev" = "--file" ] && cp "$a" {out}/prompt_file
  prev="$a"
done
[ -f .cursor/mcp.json ] && cp .cursor/mcp.json {out}/mcp.json
echo "token=${{WATS_RUNNER_TOKEN:+set}} foo=$FOO" > {out}/env
cat > {out}/stdin
"#,
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(dir: &Path, command: PathBuf) -> AgentConfig {
    let access = Access::Inherit;
    let work = dir.join("data/agent/cursor");
    AgentConfig {
        runtime: "custom".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(1),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command,
        mcp_command: "/opt/wats".into(),
        mcp_args: ["--dir", "/d", "mcp", "--profile", "agent"]
            .map(String::from)
            .to_vec(),
        extra_args: Vec::new(),
        run_timeout: Duration::from_secs(10),
        max_turns: 7,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access,
        workdir: work.join("workspace"),
        tools: vec![],
        servers: Vec::new(),
        mcp_format: Default::default(),
        mcp_file: None,
        env: Vec::new(),
        model: None,
        work_dir: work,
        data_dir: dir.join("data"),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    }
}

async fn one_run(dir: &Path, config: AgentConfig) -> wats::runs::RunRecord {
    let a = node(&dir.join("node")).await.unwrap();
    let work = config.work_dir.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("a custom run is indexed as ok", || {
        wats::runs::list(&work).iter().any(|r| r.status == "ok")
    })
    .await;
    let _ = stop.send(());
    runner.await.unwrap().unwrap();
    a.shutdown().await.unwrap();
    wats::runs::list(&work)
        .into_iter()
        .find(|r| r.status == "ok")
        .unwrap()
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join("out").join(name)).unwrap_or_default()
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|i| i.to_string()).collect()
}

#[tokio::test]
async fn the_template_the_mcp_file_and_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path(), fake(dir.path()));
    config.extra_args = s(&[
        "-p",
        "--mcp",
        "{mcp_config}",
        "--model={model}",
        "--cwd={cwd}",
    ]);
    config.env = vec![("FOO".into(), "bar".into())];
    let work = config.work_dir.clone();
    one_run(dir.path(), config).await;
    let args: Vec<String> = read(dir.path(), "args").lines().map(String::from).collect();
    assert_eq!(args[0], "-p");
    assert_eq!(args[1], "--mcp");
    assert!(args[2].ends_with("custom-mcp.json"), "{args:?}");
    assert!(
        !args.iter().any(|a| a.starts_with("--model")),
        "no model: dropped"
    );
    assert!(
        args[3].starts_with("--cwd=") && args[3].ends_with("run"),
        "{args:?}"
    );
    assert!(
        read(dir.path(), "stdin").contains("WeAreTheSwarm"),
        "the prompt on stdin"
    );
    let mcp: Value = serde_json::from_str(&read(dir.path(), "mcp.json")).unwrap();
    assert_eq!(mcp["mcpServers"].as_object().unwrap().len(), 1);
    assert!(mcp["mcpServers"]["wats"]["env"]["WATS_RUNNER_TOKEN"].is_string());
    assert_eq!(read(dir.path(), "env").trim(), "token= foo=bar");
    assert!(
        !work.join("custom-mcp.json").exists(),
        "removed after the run"
    );
    let cwd = PathBuf::from(read(dir.path(), "cwd").trim_end());
    assert!(cwd.ends_with("cursor/run"), "{}", cwd.display());
}

#[tokio::test]
async fn a_project_mcp_file_and_a_prompt_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path(), fake(dir.path()));
    config.extra_args = s(&["--file", "{prompt_file}", "--ext", "{wats_mcp}"]);
    config.mcp_file = Some(".cursor/mcp.json".into());
    config.model = Some("m1".into());
    let work = config.work_dir.clone();
    one_run(dir.path(), config).await;
    assert!(
        read(dir.path(), "stdin").is_empty(),
        "the prompt is in its file"
    );
    assert!(read(dir.path(), "prompt_file").contains("WeAreTheSwarm"));
    let mcp: Value = serde_json::from_str(&read(dir.path(), "mcp.json")).unwrap();
    assert!(mcp["mcpServers"]["wats"].is_object());
    let args: Vec<String> = read(dir.path(), "args").lines().map(String::from).collect();
    assert!(args[3].starts_with("/opt/wats --dir /d mcp"), "{args:?}");
    assert_eq!(
        read(dir.path(), "env").trim(),
        "token=set foo=",
        "{{wats_mcp}}: the token in its env"
    );
    assert!(!work.join("run/.cursor/mcp.json").exists());
    assert!(!wats::custom::prompt_path(&work).exists());
    let record = wats::runs::list(&work).into_iter().next().unwrap();
    assert_eq!(record.runtime, "custom");
}

/// `wats setup custom --agent`: checked, written, review on.
#[test]
fn setup_custom_writes_the_agent_with_review() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    let setup = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path().join("home"))
            .env("WATS_RUN_DIR", dir.path().join("run"))
            .arg("--dir")
            .arg(&data)
            .args(["setup", "custom", "--agent"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let bad = setup(&[
        "--name",
        "cursor-cli",
        "--command",
        "agent",
        "--args",
        "-p {promt}",
    ]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("{promt}"));
    let out = setup(&[
        "--name",
        "cursor-cli",
        "--command",
        "agent",
        "--args",
        "-p --force --approve-mcps {prompt}",
        "--mcp-file",
        ".cursor/mcp.json",
    ]);
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains("unconfined"), "{said}");
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    for line in [
        "runtime = \"custom\"",
        "command = \"agent\"",
        "mcp_file = \".cursor/mcp.json\"",
        "review = true",
    ] {
        assert!(config.contains(line), "{line}: {config}");
    }
}
