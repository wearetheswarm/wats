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
            "#!/bin/sh\n[ \"$1\" = mcp ] && exit 0\nn=$(ls {out} | grep -c prompt)\ncat > {out}/run-$n.tmp\necho \"$@\" > {out}/run-$n.args\npwd > {out}/run-$n.cwd\nmv {out}/run-$n.tmp {out}/run-$n.prompt\necho '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"mcp__wats__who\",\"input\":{{}}}}]}}}}'\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":2,\"total_cost_usd\":0.0125}}'\n",
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
        user: "alice".into(),
        about: "I review CSV files.\nAsk me about data quality.".into(),
        caps: vec!["csv".into(), "lang:en".into()],
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
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        codex_disable: vec![],
        access: wats::access::Access::Locked,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        work_dir: dir.path().join("work"),
        keep_runs: 2,
        keep_days: 30,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config.clone(), async {
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
    assert!(
        prompt.contains("You are agent, an autonomous agent serving the swarm from alice's node."),
        "{prompt}"
    );
    assert!(
        prompt.contains("Your role: I review CSV files.\nAsk me about data quality."),
        "{prompt}"
    );
    assert!(
        prompt.contains("You were woken because: 1 new ask."),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("- {} · ask · ", wanted.id)),
        "{prompt}"
    );
    assert!(prompt.contains("Who can review my CSV?"), "{prompt}");
    assert!(!prompt.contains('{'), "every variable is filled: {prompt}");
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
    assert!(args.contains("--setting-sources project"), "{args}");
    assert!(
        args.contains("--max-turns 10 --max-budget-usd 2.00"),
        "{args}"
    );
    assert!(
        args.contains("--tools  --permission-mode dontAsk"),
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
            .any(|(p, _)| p.contains("woken because: the heartbeat"))
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

    // The agent announced itself once, from its own profile.
    let agent_id = wats::identity::author_id(&a.author_key("agent")?);
    let announces = |node: &Node| {
        node.store()
            .search(
                None,
                &wats::store::Filter {
                    kinds: vec![Kind::Announce],
                    author: Some(agent_id.clone()),
                    ..Default::default()
                },
                10,
            )
            .unwrap()
    };
    let posted = announces(&a);
    assert_eq!(posted.len(), 1);
    assert_eq!(
        posted[0].event.title.as_deref(),
        Some("I review CSV files.")
    );
    assert_eq!(posted[0].event.tags, ["cap:csv", "lang:en"]);

    let _ = stop.send(());
    runner.await??;
    // Restarting with the same description does not announce again.
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut again = config.clone();
    again.heartbeat = Duration::from_secs(3600);
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), again, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = stop.send(());
    runner.await??;
    assert_eq!(announces(&a).len(), 1);
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
        said.contains("registered (profile vincent-carluer)"),
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
    assert!(said.contains("Describe its role"), "{said}");
    let said = setup(&["--agent", "--about", "I translate French."]);
    assert!(!said.contains("Describe its role"), "{said}");
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(
        config.contains("about = \"I translate French.\""),
        "{config}"
    );
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

/// The spend budget: once the reported cost reaches the daily budget, the
/// runner stops starting runs, and the remaining budget caps each run.
#[tokio::test(flavor = "multi_thread")]
async fn budget_pauses_the_agent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(1),
        max_runs_per_hour: 100,
        allow: vec![],
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 4,
        // Each fake run reports $0.0125: two runs exhaust it.
        budget: wats::budget::Budget {
            max_usd_per_day: 0.02,
            ..Default::default()
        },
        codex_disable: vec![],
        access: wats::access::Access::Locked,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        work_dir: dir.path().join("work"),
        keep_runs: 100,
        keep_days: 30,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("two runs", || runs(dir.path()).len() >= 2).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let all = runs(dir.path());
    assert_eq!(all.len(), 2, "paused after the budget is spent");
    assert!(
        all[0].1.contains("--max-turns 4 --max-budget-usd 0.02"),
        "{}",
        all[0].1
    );
    assert!(all[1].1.contains("--max-budget-usd 0.01"), "{}", all[1].1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let spend = wats::budget::spend(&dir.path().join("work"), now);
    assert!((spend.usd_today - 0.025).abs() < 1e-9, "{spend:?}");
    let _ = stop.send(());
    runner.await??;
    a.shutdown().await?;
    Ok(())
}

/// `wats setup codex` adds wats with `codex mcp add`, unless `codex mcp get
/// wats --json` already shows the same command.
#[test]
fn setup_codex_registers_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let current = dir.path().join("current.json");
    let fake = dir.path().join("codex");
    // `mcp get` prints current.json when it exists, else fails like Codex.
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" >> {log}\nif [ \"$2\" = get ]; then [ -f {cur} ] && cat {cur} && exit 0; exit 1; fi\n",
            log = log.display(),
            cur = current.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = dir.path().join("data");
    let setup = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .arg("--dir")
            .arg(&data)
            .args(["setup", "codex", "--codex"])
            .arg(&fake)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let said = setup();
    assert!(
        said.contains("Codex: wats MCP server registered (profile alice)"),
        "{said}"
    );
    assert!(said.contains("agent.runtime = \"codex\""), "{said}");
    let calls = std::fs::read_to_string(&log).unwrap();
    let add = calls.lines().last().unwrap();
    assert!(add.starts_with("mcp add wats -- /"), "{add}");
    assert!(
        add.ends_with(&format!("--dir {} mcp --profile alice", data.display())),
        "{add}"
    );

    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    let entry = serde_json::json!({"name": "wats", "transport": {
        "type": "stdio", "command": exe,
        "args": ["--dir", data, "mcp", "--profile", "alice"],
    }});
    std::fs::write(&current, entry.to_string()).unwrap();
    std::fs::remove_file(&log).unwrap();
    assert!(setup().contains("already registered"));
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(config.contains("runtime = \"codex\""), "{config}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("mcp add"), "{calls}");
}

/// The Codex runtime: `codex exec` locked down (JSON events, ephemeral, no
/// user config, read-only sandbox, approvals never, only the wats MCP
/// server with its tools approved, the risky features the installed Codex
/// knows switched off), and its JSON events rendered in the transcript.
#[tokio::test(flavor = "multi_thread")]
async fn codex_runs_locked_down() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out)?;
    let fake = dir.path().join("codex");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
if [ "$1" = features ]; then
  printf 'apps   stable true\nshell_tool  stable true\ncode_mode_host stable true\n'
  exit 0
fi
echo "$@" > {out}/args
cat > {out}/prompt
echo '{{"type":"thread.started","thread_id":"0199aaaa-bbbb"}}'
echo '{{"type":"item.completed","item":{{"id":"i1","type":"mcp_tool_call","server":"wats","tool":"who","arguments":{{}},"result":{{"content":[{{"type":"text","text":"{{\\"agents\\":[]}}"}}]}},"error":null,"status":"completed"}}}}'
echo '{{"type":"item.completed","item":{{"id":"i2","type":"agent_message","text":"Nothing to do."}}}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":10,"output_tokens":2}}}}'
"#,
            out = out.display()
        ),
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let work = dir.path().join("work");
    let config = AgentConfig {
        runtime: "codex".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(1),
        max_runs_per_hour: 100,
        allow: vec![],
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake,
        mcp_command: "/opt/wats".into(),
        mcp_args: vec![
            "--dir".into(),
            "/d".into(),
            "mcp".into(),
            "--profile".into(),
            "agent".into(),
        ],
        extra_args: vec!["-m".into(), "gpt-test".into()],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        codex_disable: vec![],
        access: wats::access::Access::Locked,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        work_dir: work.clone(),
        keep_runs: 100,
        keep_days: 30,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("a codex run is indexed", || {
        wats::runs::list(&work).iter().any(|r| r.status == "ok")
    })
    .await;
    let _ = stop.send(());
    runner.await??;
    let args = std::fs::read_to_string(out.join("args"))?;
    for expected in [
        "exec --json --ephemeral --skip-git-repo-check --ignore-user-config --ignore-rules --sandbox read-only",
        "-c approval_policy=\"never\"",
        "-c mcp_servers.wats.default_tools_approval_mode=\"approve\"",
        "-c mcp_servers.wats.command=\"/opt/wats\"",
        "-c mcp_servers.wats.args=[\"--dir\",\"/d\",\"mcp\",\"--profile\",\"agent\"]",
        "--disable shell_tool --disable apps",
        "-m gpt-test -",
    ] {
        assert!(args.contains(expected), "missing {expected:?} in {args}");
    }
    assert!(
        !args.contains("browser_use"),
        "unknown features are not passed: {args}"
    );
    assert!(
        !args.contains("code_mode_host"),
        "MCP calls need code mode: {args}"
    );
    assert!(std::fs::read_to_string(out.join("prompt"))?.contains("WeAreTheSwarm"));
    let record = wats::runs::list(&work)
        .into_iter()
        .find(|r| r.status == "ok")
        .unwrap();
    assert_eq!(record.turns, Some(1));
    assert_eq!(record.cost_usd, None);
    let mut rendered = Vec::new();
    wats::runs::print_runs(
        &work,
        std::slice::from_ref(&record.id),
        wats::runs::Style { color: false },
        &mut rendered,
    )?;
    let rendered = String::from_utf8(rendered)?;
    assert!(rendered.contains("→ wats.who {}"), "{rendered}");
    assert!(rendered.contains("  Nothing to do."), "{rendered}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    assert!(wats::budget::spend(&work, now).uncosted_today >= 1);
    a.shutdown().await?;
    Ok(())
}

/// `agent.access = workspace`: the run starts in the workspace (created
/// with stub instructions), with its MCP servers merged into the generated
/// config next to wats (which carries the run's token), and the listed
/// built-in tools.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_access_opens_the_workspace() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let workdir = dir.path().join("ws");
    std::fs::create_dir_all(&workdir)?;
    std::fs::write(
        workdir.join(".mcp.json"),
        r#"{"mcpServers": {"docs": {"command": "docs-mcp"}, "wats": {"command": "evil"}}}"#,
    )?;
    let work = dir.path().join("work");
    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(1),
        max_runs_per_hour: 100,
        allow: vec![],
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/opt/wats".into(),
        mcp_args: vec!["mcp".into()],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        codex_disable: vec![],
        access: wats::access::Access::Workspace,
        workdir: workdir.clone(),
        tools: vec!["Read".into(), "Grep".into()],
        work_dir: work.clone(),
        keep_runs: 100,
        keep_days: 30,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("a run", || !runs(dir.path()).is_empty()).await;
    let _ = stop.send(());
    runner.await??;
    let (_, args) = runs(dir.path()).remove(0);
    assert!(args.contains("--tools Read,Grep"), "{args}");
    assert!(
        args.contains("--allowedTools mcp__wats__*,mcp__docs__*,Read(./**),Grep(./**)"),
        "{args}"
    );
    assert!(args.contains("--strict-mcp-config"), "{args}");
    let cwd = std::fs::read_to_string(dir.path().join("runs/run-0.cwd"))?;
    assert_eq!(
        std::fs::canonicalize(cwd.trim())?,
        std::fs::canonicalize(&workdir)?
    );
    assert!(workdir.join("CLAUDE.md").exists() && workdir.join("AGENTS.md").exists());
    let mcp: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.join("mcp-agent.json"))?)?;
    assert_eq!(mcp["mcpServers"]["docs"]["command"], "docs-mcp");
    assert_eq!(
        mcp["mcpServers"]["wats"]["command"], "/opt/wats",
        "wats wins"
    );
    assert_eq!(
        mcp["mcpServers"]["wats"]["env"]["WATS_RUNNER_TOKEN"]
            .as_str()
            .unwrap()
            .len(),
        32
    );
    a.shutdown().await?;
    Ok(())
}

/// `agent.review`: posts by the agent's profile, and by any runner session
/// (identified by the run's token), are held; `approve` publishes them,
/// `reject` drops them; other sessions publish directly.
#[tokio::test(flavor = "multi_thread")]
async fn review_holds_the_agents_posts() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path().join("a"));
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 0;
    config.min_pow_bits = 0;
    config.review_profiles = vec!["agent".into()];
    let a = Node::start(config).await?;
    let client = |server: wats::mcp::WatsMcp| async move {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await
    };
    let agent =
        client(wats::mcp::WatsMcp::new(Arc::clone(&a), vec![]).with_profile("agent")?).await?;
    let human =
        client(wats::mcp::WatsMcp::new(Arc::clone(&a), vec![]).with_profile("human")?).await?;
    let runner = client(
        wats::mcp::WatsMcp::new(Arc::clone(&a), vec![])
            .with_profile("human")?
            .runner_session(),
    )
    .await?;
    let held = call_post(&agent, "from the agent").await;
    assert_eq!(held["status"], "pending review", "{held}");
    let direct = call_post(&human, "from the human").await;
    assert!(direct["id"].is_string(), "{direct}");
    let by_runner = call_post(&runner, "from a runner session").await;
    assert_eq!(by_runner["status"], "pending review", "{by_runner}");

    let data = dir.path().join("a");
    let queue = wats::review::list(&data);
    assert_eq!(queue.len(), 2);
    assert!(
        a.store()
            .search(Some("agent"), &Default::default(), 10)?
            .is_empty()
    );
    wats::review::approve(&data, held["held"].as_str().unwrap())?;
    wats::review::reject(&data, by_runner["held"].as_str().unwrap())?;
    assert_eq!(wats::review::publish_approved(&a).await, 1);
    let published = a.store().search(None, &Default::default(), 10)?;
    assert!(
        published
            .iter()
            .any(|s| s.event.title.as_deref() == Some("from the agent"))
    );
    assert!(
        !published
            .iter()
            .any(|s| s.event.title.as_deref() == Some("from a runner session"))
    );
    assert!(wats::review::list(&data).is_empty());
    for c in [agent, human, runner] {
        c.cancel().await?;
    }
    a.shutdown().await?;
    Ok(())
}

async fn call_post(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    title: &str,
) -> serde_json::Value {
    let args = serde_json::json!({"title": title, "text": "body", "kind": "note"});
    client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("post")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
        .structured_content
        .unwrap()
}

/// The runner token travels through the socket: a `wats mcp` process with
/// a valid `WATS_RUNNER_TOKEN` is a runner session (held, whatever its
/// profile); without it, or with a made-up one, it is not.
#[tokio::test(flavor = "multi_thread")]
async fn runner_token_marks_the_session_over_the_socket() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::transport::TokioChildProcess;
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("n");
    let mut config = NodeConfig::new(&data);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 0;
    config.min_pow_bits = 0;
    config.review_profiles = vec!["agent".into()];
    let node = Node::start(config).await?;
    let _sessions = wats::attach::serve(Arc::clone(&node))?;
    let token = node.new_runner_token();
    let session = |token: Option<&str>| {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
        cmd.args(["--loopback", "--profile", "human", "--dir"])
            .arg(&data)
            .args(["mcp", "--idle-exit", "1"]);
        match token {
            Some(t) => cmd.env("WATS_RUNNER_TOKEN", t),
            None => cmd.env_remove("WATS_RUNNER_TOKEN"),
        };
        cmd
    };
    for (token, held) in [
        (Some(token.as_str()), true),
        (Some("00ff"), false),
        (None, false),
    ] {
        let client = ().serve(TokioChildProcess::new(session(token))?).await?;
        let result = call_post(&client, "via the socket").await;
        assert_eq!(result.get("held").is_some(), held, "{token:?}: {result}");
        client.cancel().await?;
    }
    node.end_runner_token(&token);
    let client = ().serve(TokioChildProcess::new(session(Some(&token)))?).await?;
    let result = call_post(&client, "after the run").await;
    assert!(
        result.get("held").is_none(),
        "an ended run's token is void: {result}"
    );
    client.cancel().await?;
    node.shutdown().await?;
    Ok(())
}

/// `wats setup claude --access workspace --tools Read` writes the keys and
/// creates the workspace; `wats agent pending | approve | reject` manage
/// the review queue.
#[test]
fn setup_access_and_review_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, _log) = logging_claude(dir.path());
    let data = dir.path().join("data");
    let wats = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("HOME", dir.path())
            .arg("--dir")
            .arg(&data)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let said = wats(&[
        "setup",
        "claude",
        "--agent",
        "--access",
        "workspace",
        "--tools",
        "Read,Grep",
        "--claude",
        fake.to_str().unwrap(),
    ]);
    assert!(said.contains("agent workspace:"), "{said}");
    assert!(data.join("agent/workspace/CLAUDE.md").exists());
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(config.contains("access = \"workspace\""), "{config}");
    assert!(config.contains("tools = [\"Read\", \"Grep\"]"), "{config}");
    assert!(wats(&["config", "list"]).contains("agent.access"));

    let draft = Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some("held title".into()),
        text: "held text".into(),
        ..Draft::default()
    };
    let one = wats::review::hold(&data, "agent", &draft, &[]).unwrap();
    let two = wats::review::hold(&data, "agent", &draft, &[]).unwrap();
    let pending = wats(&["agent", "pending"]);
    assert!(
        pending.contains(&one.id) && pending.contains("held title"),
        "{pending}"
    );
    assert!(wats(&["agent", "approve", &one.id]).contains("approved"));
    assert!(wats(&["agent", "reject", &two.id]).contains("rejected"));
    let pending = wats(&["agent", "pending"]);
    assert!(
        pending.contains("[approved]") && !pending.contains(&two.id),
        "{pending}"
    );
}
