//! The agent runner (SPEC §2.8): wakes a (fake) agent CLI on asks from
//! others, never on its own posts, honours --allow, and runs a heartbeat.
//! Every run leaves a transcript and an index record.

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

/// A fake agent CLI: records its arguments and the prompt read on stdin, and
/// prints a Claude Code stream-json result line.
fn fake_agent(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let out = dir.join("runs");
    std::fs::create_dir_all(&out).unwrap();
    let script = dir.join("fake-claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = mcp ] && exit 0\nn=$(ls {out} | grep -c prompt)\ncat > {out}/run-$n.tmp\necho \"$@\" > {out}/run-$n.args\nmv {out}/run-$n.tmp {out}/run-$n.prompt\necho '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"mcp__wats__who\",\"input\":{{}}}}]}}}}'\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":2,\"total_cost_usd\":0.0125}}'\n",
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
        runtime: "claude".into(),
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
        keep_runs: 2,
        keep_days: 30,
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
    assert!(
        args.contains("--output-format stream-json --verbose"),
        "{args}"
    );
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

    // Transcripts and the index: the first run woke for b's ask, reported
    // its turns and cost; retention keeps the last 2.
    let work = dir.path().join("work");
    eventually("runs indexed", || {
        wats::runs::list(&work)
            .iter()
            .filter(|r| r.status == "ok")
            .count()
            >= 2
    })
    .await;
    let records = wats::runs::list(&work);
    assert!(records.len() <= 2, "{records:?}");
    let last = records.iter().rfind(|r| r.status == "ok").unwrap();
    assert_eq!(last.turns, Some(2));
    assert_eq!(last.cost_usd, Some(0.0125));
    assert_eq!(last.exit, Some(0));
    let transcript = std::fs::read_to_string(wats::runs::transcript_path(&work, &last.id))?;
    let first: serde_json::Value = serde_json::from_str(transcript.lines().next().unwrap())?;
    assert_eq!(first["type"], "wats_run", "{transcript}");
    assert!(transcript.contains("\"type\":\"wats_end\""), "{transcript}");

    let _ = stop.send(());
    runner.await??;
    for n in [a, b, c] {
        n.shutdown().await?;
    }
    Ok(())
}

/// `wats --agent claude` serves mcp.sock: an interactive `wats mcp` session
/// (another profile) attaches to the same node, and its ask wakes the
/// agent. `wats agent runs` and `wats agent logs` then show the run.
#[tokio::test(flavor = "multi_thread")]
async fn interactive_session_attaches_while_agent_runs() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;

    let dir = tempfile::tempdir()?;
    let data = dir.path().join("node");
    let fake = fake_agent(dir.path());
    let wats = env!("CARGO_BIN_EXE_wats");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;
    let mut agent = tokio::process::Command::new(wats)
        .env("HOME", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(["--loopback", "--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args([
            "--agent",
            "claude",
            "--agent-profile",
            "agent",
            "--every",
            "1s",
            "--heartbeat",
            "1h",
            "--agent-command",
        ])
        .arg(&fake)
        .args(["--", "--model", "trailing"])
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
    assert!(runs(dir.path())[0].1.contains("--model trailing"));
    let watch = |args: &[&str]| {
        let out = std::process::Command::new(wats)
            .arg("--dir")
            .arg(&data)
            .arg("agent")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    eventually("the run is indexed", || {
        watch(&["runs", "--json"]).contains("\"status\":\"ok\"")
    })
    .await;
    let table = watch(&["runs"]);
    assert!(table.contains("$0.0125"), "{table}");
    let logs = watch(&["logs"]);
    assert!(logs.contains(&format!("woke for {id}")), "{logs}");
    assert!(logs.contains("→ wats.who"), "{logs}");
    assert!(logs.contains("■ ok · exit 0"), "{logs}");
    client.cancel().await?;
    agent.kill().await?;
    Ok(())
}

/// `wats setup claude` registers wats in Claude Code (absolute binary,
/// explicit --dir, profile = OS user) and enables the runner in
/// config.toml; --mcp and --agent do one half; registering again is a no-op
/// once Claude Code has the entry.
#[test]
fn setup_claude_registers_and_enables() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, log) = logging_claude(dir.path());
    let data = dir.path().join("data");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let setup = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("HOME", &home)
            .env("USER", "Vincent.Carluer")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("WATS_PROFILE")
            .arg("--dir")
            .arg(&data)
            .args(["setup", "claude", "--claude"])
            .arg(&fake)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let said = setup(&[]);
    assert!(
        said.contains("registered (scope user, profile vincent-carluer)"),
        "{said}"
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
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(config.contains("runtime = \"claude\""), "{config}");

    // Once Claude Code has the entry, --mcp changes nothing.
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    let entry = serde_json::json!({"mcpServers": {"wats": {
        "type": "stdio", "command": exe, "env": {},
        "args": ["--dir", data, "mcp", "--profile", "vincent-carluer"],
    }}});
    std::fs::write(home.join(".claude.json"), entry.to_string()).unwrap();
    std::fs::remove_file(&log).unwrap();
    let said = setup(&["--mcp"]);
    assert!(said.contains("already registered"), "{said}");
    assert!(!said.contains("agent.runtime"), "{said}");
    assert!(!log.exists());

    // --agent only touches config.toml; project scope always re-adds.
    std::fs::remove_file(data.join("config.toml")).unwrap();
    let said = setup(&["--agent"]);
    assert!(said.contains("agent.runtime"), "{said}");
    assert!(!log.exists());
    setup(&["--mcp", "--scope", "project"]);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains("mcp add --scope project wats -- "),
        "{calls}"
    );
}

/// `wats config`: set, get, list and unset keys, with checked values.
#[test]
fn config_command_edits_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let wats = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .arg("--dir")
            .arg(&data)
            .arg("config")
            .args(args)
            .output()
            .unwrap()
    };
    let text = |out: std::process::Output| {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(text(wats(&["get", "agent.heartbeat"])), "30m (default)\n");
    text(wats(&["set", "agent.heartbeat", "2h"]));
    text(wats(&["set", "agent.allow", "aa,bb"]));
    assert_eq!(text(wats(&["get", "agent.heartbeat"])), "\"2h\"\n");
    let list = text(wats(&["list"]));
    assert!(list.contains("agent.allow"), "{list}");
    assert!(list.contains("[\"aa\", \"bb\"]"), "{list}");
    assert!(list.contains("infra"), "{list}");
    assert!(!wats(&["set", "agent.heartbeat", "soon"]).status.success());
    assert!(!wats(&["set", "agent.colour", "red"]).status.success());
    text(wats(&["unset", "agent.heartbeat"]));
    assert_eq!(text(wats(&["get", "agent.heartbeat"])), "30m (default)\n");
}

/// A fake `claude` that logs its arguments, one line per call.
fn logging_claude(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("calls.log");
    let fake = dir.join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\ncat > /dev/null\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

/// Run `wats` until its banner is printed, then stop it; returns stderr.
async fn banner_of(cmd: &mut tokio::process::Command) -> String {
    use tokio::io::AsyncBufReadExt;
    let mut child = cmd
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = String::new();
    let read = async {
        while let Ok(Some(line)) = lines.next_line().await {
            seen.push_str(&line);
            seen.push('\n');
            if line.starts_with("Ctrl-C to stop") {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(20), read).await;
    child.kill().await.unwrap();
    seen
}

/// `wats --agent claude` registers wats in Claude Code only when the user
/// config does not already have the same entry, then runs the node with
/// the agent.
#[tokio::test(flavor = "multi_thread")]
async fn agent_claude_registers_once_then_runs_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, log) = logging_claude(dir.path());
    let data = dir.path().join("data");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let wats = env!("CARGO_BIN_EXE_wats");
    let run = || {
        let mut cmd = tokio::process::Command::new(wats);
        cmd.env("HOME", &home)
            .env("USER", "tester")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("WATS_PROFILE")
            .args(["--loopback", "--dir"])
            .arg(&data)
            .args(["--agent", "claude", "--heartbeat", "1h", "--agent-command"])
            .arg(&fake);
        cmd
    };
    let banner = banner_of(&mut run()).await;
    assert!(
        banner.contains("wats registered in Claude Code"),
        "{banner}"
    );
    assert!(
        banner.contains("agent claude: on, posts as tester-agent"),
        "{banner}"
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("mcp add --scope user wats -- "), "{calls}");

    // Claude Code now has the entry (the fake does not write it: do it).
    let exe = std::fs::canonicalize(wats).unwrap();
    let config = serde_json::json!({"mcpServers": {"wats": {
        "type": "stdio", "command": exe, "env": {},
        "args": ["--dir", data, "mcp", "--profile", "tester"],
    }}});
    std::fs::write(home.join(".claude.json"), config.to_string()).unwrap();
    std::fs::remove_file(&log).unwrap();
    let banner = banner_of(&mut run()).await;
    assert!(!banner.contains("registered"), "{banner}");
    assert!(!log.exists() || !std::fs::read_to_string(&log).unwrap().contains("mcp "));
}

/// A bare `wats` runs the agents enabled in `<data dir>/config.toml`;
/// `--no-agent` and flags override it.
#[tokio::test(flavor = "multi_thread")]
async fn bare_wats_runs_agents_from_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "[agent]\nruntime = \"claude\"\nprofile = \"cfg-agent\"\nheartbeat = \"2h\"\ncommand = \"/usr/bin/true\"\n",
    )
    .unwrap();
    let wats = env!("CARGO_BIN_EXE_wats");
    let base = |extra: &[&str]| {
        let mut cmd = tokio::process::Command::new(wats);
        cmd.args(["--loopback", "--dir"]).arg(&data).args(extra);
        cmd
    };
    let banner = banner_of(&mut base(&[])).await;
    assert!(
        banner.contains("agent claude: on, posts as cfg-agent"),
        "{banner}"
    );
    assert!(banner.contains("heartbeat 2h"), "{banner}");
    let banner = banner_of(&mut base(&["--agent-profile", "flag-agent"])).await;
    assert!(banner.contains("posts as flag-agent"), "{banner}");
    let banner = banner_of(&mut base(&["--no-agent"])).await;
    assert!(banner.contains("agent: off"), "{banner}");

    std::fs::write(data.join("config.toml"), "[agent]\nenabled = true\n").unwrap();
    // ... but `wats config` still works, to fix it.
    let out = std::process::Command::new(wats)
        .arg("--dir")
        .arg(&data)
        .args(["config", "unset", "agent.runtime"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = std::process::Command::new(wats)
        .args(["--loopback", "--dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a typo in config.toml is an error");
}
