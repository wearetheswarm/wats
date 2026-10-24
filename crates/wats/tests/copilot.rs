//! GitHub Copilot CLI as an agent runtime (ADR 0038), against fake
//! `copilot` scripts only: never the real CLI. The runner's command line,
//! its own Copilot home and MCP config for each access level, the prompt
//! in `-p`, its JSONL events and the AI credits of its usage file.

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

/// A fake `copilot`: records its arguments (one per line), working
/// directory, `COPILOT_*` variables, its home's MCP config and stdin, then
/// prints the events of a good run and writes its usage file.
fn fake(dir: &Path) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = dir.join("copilot");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > {out}/args
pwd > {out}/cwd
env | grep -E '^COPILOT_' | sort > {out}/env
cat "$COPILOT_HOME/mcp-config.json" > {out}/mcp.json 2>/dev/null
cat > {out}/stdin
usage=""
prev=""
for a in "$@"; do
  case "$a" in --usage-output-file) prev=u; continue;; esac
  [ "$prev" = u ] && usage="$a"; prev=""
done
echo '{{"type":"user.message","data":{{"content":"x","responsesReasoning":{{"model":"gpt-6-luna","effort":"medium"}}}}}}'
echo '{{"type":"assistant.message","data":{{"content":"Nothing to do.","toolRequests":[]}}}}'
echo '{{"type":"result","exitCode":0,"usage":{{"premiumRequests":1,"totalApiDurationMs":10}}}}'
echo '{{"totalNanoAiu":250000000,"totalPremiumRequests":1}}' > "$usage"
"#,
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(dir: &Path, command: PathBuf, access: Access) -> AgentConfig {
    let work = dir.join("data/agent/copilot");
    AgentConfig {
        runtime: "copilot".into(),
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
        extra_args: vec!["--reasoning-effort=low".into()],
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
        model: Some("gpt-6".into()),
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
    eventually("a Copilot run is indexed as ok", || {
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

#[tokio::test]
async fn a_swarm_only_run_has_the_wats_tools_only() {
    let dir = tempfile::tempdir().unwrap();
    let command = fake(dir.path());
    let config = config(dir.path(), command, Access::SwarmOnly);
    let home = wats::copilot::agent_home(&config.work_dir);
    let record = one_run(dir.path(), config).await;
    assert_eq!(record.status, "ok", "{record:?}");
    assert_eq!(
        record.model, "gpt-6",
        "agent.model wins over the reported one"
    );
    assert_eq!(record.ai_credits, Some(0.25));

    let args: Vec<String> = read(dir.path(), "args").lines().map(String::from).collect();
    assert_eq!(args[0], "-p");
    assert!(args[1].contains("WeAreTheSwarm"), "the prompt: {}", args[1]);
    for flag in [
        "--output-format",
        "--no-ask-user",
        "--allow-all-tools",
        "--disable-builtin-mcps",
        "--available-tools=wats",
        "--deny-tool=shell",
        "--deny-tool=write",
        "--deny-tool=url",
        "--model=gpt-6",
        "--reasoning-effort=low",
    ] {
        assert!(args.iter().any(|a| a == flag), "{flag}: {args:?}");
    }
    assert!(
        read(dir.path(), "stdin").is_empty(),
        "the prompt is not on stdin too"
    );
    let env = read(dir.path(), "env");
    assert!(
        env.contains(&format!("COPILOT_HOME={}", home.display())),
        "{env}"
    );
    let mcp: Value = serde_json::from_str(&read(dir.path(), "mcp.json")).unwrap();
    let servers = mcp["mcpServers"].as_object().unwrap();
    assert_eq!(servers.keys().collect::<Vec<_>>(), ["wats"]);
    assert_eq!(servers["wats"]["command"], "/opt/wats");
    assert!(servers["wats"]["env"]["WATS_RUNNER_TOKEN"].is_string());
    let mode = std::fs::metadata(home.join("mcp-config.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let cwd = PathBuf::from(read(dir.path(), "cwd").trim_end());
    assert!(cwd.ends_with("locked"), "{}", cwd.display());
}

#[tokio::test]
async fn workspace_and_inherit_runs() {
    let dir = tempfile::tempdir().unwrap();
    let command = fake(dir.path());
    let mut workspace = config(dir.path(), command.clone(), Access::Workspace);
    workspace.tools = vec!["url".into()];
    one_run(dir.path(), workspace).await;
    let args = read(dir.path(), "args");
    assert!(args.contains("--deny-tool=shell"), "{args}");
    assert!(!args.contains("--deny-tool=url"), "{args}");
    assert!(!args.contains("--available-tools"), "{args}");
    assert!(args.contains("--disable-builtin-mcps"), "{args}");
    let cwd = PathBuf::from(read(dir.path(), "cwd").trim_end());
    assert!(cwd.ends_with("workspace"), "{}", cwd.display());

    let other = tempfile::tempdir().unwrap();
    let command = fake(other.path());
    let mut inherit = config(other.path(), command, Access::Inherit);
    inherit.allow = vec!["ab".repeat(32)];
    one_run(other.path(), inherit).await;
    let args = read(other.path(), "args");
    assert!(args.contains("--additional-mcp-config=@"), "{args}");
    assert!(!args.contains("--disable-builtin-mcps"), "{args}");
    assert!(!read(other.path(), "env").contains("COPILOT_HOME="));
}
