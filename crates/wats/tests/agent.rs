//! The agent runner (SPEC §2.8): wakes a (fake) agent CLI on asks from
//! others, never on its own posts, honours --allow, and runs a heartbeat.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use wats::agent::{AgentConfig, DEFAULT_HEARTBEAT};
use wats::event::{Draft, Kind};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

async fn node(dir: &Path, peers: &[&Arc<Node>]) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 0;
    config.min_pow_bits = 0;
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

/// A fake agent CLI: records its arguments and the prompt read on stdin.
fn fake_agent(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let out = dir.join("runs");
    std::fs::create_dir_all(&out).unwrap();
    let script = dir.join("fake-claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nn=$(ls {out} | grep -c prompt)\ncat > {out}/run-$n.tmp\necho \"$@\" > {out}/run-$n.args\nmv {out}/run-$n.tmp {out}/run-$n.prompt\n",
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn runs(dir: &Path) -> Vec<(String, String)> {
    let out = dir.join("runs");
    let mut runs = Vec::new();
    for n in 0.. {
        let prompt = out.join(format!("run-{n}.prompt"));
        let args = out.join(format!("run-{n}.args"));
        if !prompt.exists() {
            break;
        }
        // Wait for the script to finish writing both files.
        for _ in 0..50 {
            if args.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        runs.push((
            std::fs::read_to_string(prompt).unwrap(),
            std::fs::read_to_string(args).unwrap_or_default(),
        ));
    }
    runs
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn ask(text: &str) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ask),
        title: Some(text.into()),
        text: text.into(),
        ..Draft::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_wakes_on_asks_and_heartbeats() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    let c = node(&dir.path().join("c"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    let config = AgentConfig {
        profile: "agent".into(),
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(4),
        max_runs_per_hour: 100,
        // Only b is allowed: c's asks are ignored.
        allow: vec![b.author_id()],
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec!["mcp".into(), "--profile".into(), "agent".into()],
        extra_args: vec!["--model".into(), "test-model".into()],
        run_timeout: Duration::from_secs(10),
        work_dir: dir.path().join("work"),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The agent's own ask (same profile, same node) does not wake it.
    let own = a
        .publish_as(&a.author_key("agent")?, ask("my own question"), &[])
        .await?;
    // c is not in --allow.
    let other = c.publish(ask("ignored: not allowed"), &[]).await?;
    // b's ask wakes it.
    let wanted = b.publish(ask("Who can review my CSV?"), &[]).await?;

    eventually("an event-triggered run", || !runs(dir.path()).is_empty()).await;
    let (prompt, args) = runs(dir.path()).remove(0);
    assert!(prompt.starts_with(DEFAULT_HEARTBEAT.lines().next().unwrap()));
    assert!(prompt.contains(&wanted.id), "{prompt}");
    assert!(prompt.contains("New events of interest"));
    assert!(!prompt.contains(&own.id), "own posts never wake the agent");
    assert!(
        !prompt.contains(&other.id),
        "authors outside --allow are ignored"
    );
    assert!(
        args.contains("-p") && args.contains("--strict-mcp-config"),
        "{args}"
    );
    assert!(args.contains("--allowedTools mcp__wats__*"), "{args}");
    assert!(args.contains("--model test-model"), "{args}");
    let mcp_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("work/mcp-agent.json"))?)?;
    assert_eq!(mcp_config["mcpServers"]["wats"]["args"][2], "agent");

    // With nothing new, the heartbeat still wakes it.
    eventually("a heartbeat run", || {
        runs(dir.path())
            .iter()
            .any(|(p, _)| p.contains("Heartbeat: nothing specific woke you"))
    })
    .await;

    let _ = stop.send(());
    runner.await??;
    for n in [a, b, c] {
        n.shutdown().await?;
    }
    Ok(())
}

/// `wats agent` serves mcp.sock like `wats node`: an interactive `wats mcp`
/// session (another profile) attaches to the same node, and its ask wakes
/// the agent.
#[tokio::test(flavor = "multi_thread")]
async fn interactive_session_attaches_while_agent_runs() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;

    let dir = tempfile::tempdir()?;
    let data = dir.path().join("node");
    let fake = fake_agent(dir.path());
    let wats = env!("CARGO_BIN_EXE_wats");
    let mut agent = tokio::process::Command::new(wats)
        .args([
            "--loopback",
            "--pow-bits=0",
            "--min-pow-bits=0",
            "--profile",
            "agent",
            "--dir",
        ])
        .arg(&data)
        .args([
            "agent",
            "claude",
            "--every",
            "1s",
            "--heartbeat",
            "1h",
            "--command",
        ])
        .arg(&fake)
        .kill_on_drop(true)
        .spawn()?;
    eventually("the agent node listens on mcp.sock", || {
        wats::attach::socket_path(&data).exists()
    })
    .await;

    let mut cmd = tokio::process::Command::new(wats);
    cmd.args(["--loopback", "--profile", "human", "--dir"])
        .arg(&data)
        .args(["mcp", "--idle-exit", "1"]);
    let client = ().serve(TokioChildProcess::new(cmd)?).await?;
    let posted = client
        .call_tool(CallToolRequestParams::new("post").with_arguments(
            serde_json::json!({"title": "Help wanted", "text": "Can an agent summarize this?", "kind": "ask"})
                .as_object()
                .unwrap()
                .clone(),
        ))
        .await?
        .structured_content
        .unwrap();
    let id = posted["id"].as_str().unwrap().to_string();
    // Still a single node: no second one was started on the data dir.
    assert!(!data.join("node.log").exists());

    eventually("the agent woke for the human's ask", || {
        runs(dir.path()).iter().any(|(p, _)| p.contains(&id))
    })
    .await;
    client.cancel().await?;
    agent.kill().await?;
    Ok(())
}

/// `wats mcp add claude` replaces any `wats` entry, registers the absolute
/// binary with `mcp --profile <OS user>`, and keeps an explicit --dir.
#[test]
fn mcp_add_claude_registers_wats() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let fake = dir.path().join("claude");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = dir.path().join("data");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("USER", "Vincent.Carluer")
        .env_remove("WATS_PROFILE")
        .arg("--dir")
        .arg(&data)
        .args(["mcp", "add", "claude", "--claude"])
        .arg(&fake)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines[0], "mcp remove --scope user wats");
    assert!(
        lines[1].starts_with("mcp add --scope user wats -- /"),
        "{}",
        lines[1]
    );
    assert!(
        lines[1].ends_with(&format!(
            "--dir {} mcp --profile vincent-carluer",
            data.display()
        )),
        "{}",
        lines[1]
    );
    // An explicit profile and scope win.
    std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .args([
            "--profile",
            "ops",
            "mcp",
            "add",
            "claude",
            "--scope",
            "project",
            "--claude",
        ])
        .arg(&fake)
        .output()
        .unwrap();
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls
            .lines()
            .last()
            .unwrap()
            .contains("--scope project wats -- ")
    );
    assert!(calls.lines().last().unwrap().ends_with("mcp --profile ops"));
}
