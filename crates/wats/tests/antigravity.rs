//! Antigravity CLI as an agent runtime (ADR 0038 §2), against fake `agy`
//! scripts only: never the real CLI. The runner's command line, the
//! agent's own home (its MCP config, permission rules and gate) for each
//! level, its JSONL `result`, and the gate itself (`wats gate`).

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

/// A fake `agy`: records its arguments (one per line), working directory,
/// `HOME`, its home's files and stdin, then prints a good run.
fn fake(dir: &Path) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = dir.join("agy");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > {out}/args
pwd > {out}/cwd
echo "$HOME" > {out}/home
cp "$HOME/.gemini/config/mcp_config.json" {out}/mcp.json
cp "$HOME/.gemini/antigravity-cli/settings.json" {out}/settings.json
cp "$HOME/.gemini/config/hooks.json" {out}/hooks.json
cat > {out}/stdin
echo '{{"event":"init","conversation_id":"c1","init":{{"cwd":"/w","tools":["run_command"],"permission_mode":"request-review"}}}}'
echo '{{"event":"result","result":{{"status":"SUCCESS","response":"Nothing to do.","num_turns":2,"usage":{{"input_tokens":100,"output_tokens":20,"thinking_tokens":5,"total_tokens":125}}}}}}'
"#,
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(dir: &Path, command: PathBuf, access: Access) -> AgentConfig {
    let work = dir.join("data/agent/antigravity");
    AgentConfig {
        runtime: "antigravity".into(),
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
        extra_args: vec!["--effort=high".into()],
        run_timeout: Duration::from_secs(120),
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
        model: Some("gemini-3.8-flash".into()),
        effort: None,
        work_dir: work,
        data_dir: dir.join("data"),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
        idle_heartbeat: true,
    }
}

async fn one_run(dir: &Path, config: AgentConfig) -> wats::runs::RunRecord {
    let a = node(&dir.join("node")).await.unwrap();
    let work = config.work_dir.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("an Antigravity run is indexed as ok", || {
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

fn json(dir: &Path, name: &str) -> Value {
    serde_json::from_str(&read(dir, name)).unwrap()
}

#[tokio::test]
async fn a_swarm_only_run_is_locked_twice() {
    let dir = tempfile::tempdir().unwrap();
    let command = fake(dir.path());
    let config = config(dir.path(), command, Access::SwarmOnly);
    let home = wats::antigravity::agent_home(&config.work_dir);
    let record = one_run(dir.path(), config).await;
    assert_eq!(
        (record.turns, record.tokens),
        (Some(2), Some(125)),
        "{record:?}"
    );

    let args: Vec<String> = read(dir.path(), "args").lines().map(String::from).collect();
    assert_eq!(args[0], "-p");
    assert!(args[1].contains("WeAreTheSwarm"), "the prompt: {}", args[1]);
    assert!(
        read(dir.path(), "args").contains("read {id; max_tokens}"),
        "the prompt lists the wats tools' arguments"
    );
    for flag in [
        "stream-json",
        "--print-timeout",
        "120s",
        "--model=gemini-3.8-flash",
        "--effort=high",
    ] {
        assert!(args.iter().any(|a| a == flag), "{flag}: {args:?}");
    }
    assert!(!args.iter().any(|a| a == "--sandbox"), "{args:?}");
    assert!(read(dir.path(), "stdin").is_empty());
    assert_eq!(
        read(dir.path(), "home").trim_end(),
        home.display().to_string()
    );
    let mcp = json(dir.path(), "mcp.json");
    assert_eq!(mcp["mcpServers"].as_object().unwrap().len(), 1);
    assert!(mcp["mcpServers"]["wats"]["env"]["WATS_RUNNER_TOKEN"].is_string());
    let rules = &json(dir.path(), "settings.json")["permissions"];
    assert!(
        rules["deny"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("read_file(*)"))
    );
    let gate = json(dir.path(), "hooks.json")["wats-gate"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(gate, "/opt/wats gate antigravity");
    let cwd = PathBuf::from(read(dir.path(), "cwd").trim_end());
    assert!(cwd.ends_with("locked"), "{}", cwd.display());
}

#[tokio::test]
async fn a_workspace_run_has_its_files_and_what_is_lent() {
    let dir = tempfile::tempdir().unwrap();
    let command = fake(dir.path());
    let mut workspace = config(dir.path(), command, Access::Workspace);
    workspace.tools = vec!["web".into()];
    one_run(dir.path(), workspace).await;
    let rules = &json(dir.path(), "settings.json")["permissions"];
    assert_eq!(rules["deny"], serde_json::json!(["command(*)"]));
    let gate = json(dir.path(), "hooks.json")["wats-gate"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        gate,
        "/opt/wats gate antigravity --workspace --web-search --web-fetch"
    );
    let cwd = PathBuf::from(read(dir.path(), "cwd").trim_end());
    assert!(cwd.ends_with("workspace"), "{}", cwd.display());
}

/// `wats gate antigravity`: the hook payload on stdin, the decision on
/// stdout; anything unreadable is denied.
#[test]
fn the_gate_command_decides() {
    use std::io::Write;
    let gate = |flags: &[&str], payload: &str| -> Value {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .args(["gate", "antigravity"])
            .args(flags)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let command = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"echo hi"}},"workspacePaths":["/w"]}"#;
    assert_eq!(gate(&[], command)["decision"], "deny");
    assert_eq!(
        gate(&["--workspace", "--shell"], command)["decision"],
        "allow"
    );
    let wats =
        r#"{"toolCall":{"name":"call_mcp_tool","args":{"ServerName":"wats","ToolName":"post"}}}"#;
    assert_eq!(gate(&[], wats)["decision"], "allow");
    assert_eq!(gate(&["--workspace"], "not json")["decision"], "deny");
}
