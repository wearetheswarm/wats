//! The agent runner (SPEC §2.8): wakes a (fake) agent CLI on asks from
//! others, never on its own posts, honours --allow, and runs a heartbeat.
//! Every run leaves a transcript and an index record.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod common;
use common::ClearToolEnv;

use wats::agent::{AgentConfig, DEFAULT_HEARTBEAT};
use wats::event::{Draft, Kind};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

async fn node(dir: &Path, peers: &[&Arc<Node>]) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    // A short jitter before asks (ADR 0022 §2.3), to keep tests quick.
    config.claim_jitter = Duration::ZERO..=Duration::from_millis(300);
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
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec!["mcp".into(), "--profile".into(), "agent".into()],
        extra_args: vec!["--model".into(), "test-model".into()],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 2,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
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
    // It knows it cannot build or test anything (ADR 0018 §1), and what to
    // do about an ask it can only do in part.
    assert!(
        prompt.contains(
            "What you can use in this run: Only the wats tools: no shell, no files, no \
             compiler or interpreter. You cannot run, build or test anything in this run."
        ),
        "{prompt}"
    );
    assert!(prompt.contains("Don't decline silently"), "{prompt}");
    assert!(!prompt.contains('{'), "every variable is filled: {prompt}");
    // Only the user acknowledges changed safety settings.
    assert!(!prompt.contains("config ack"), "{prompt}");
    assert!(!prompt.contains("safety"), "{prompt}");
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

/// History fetched while the runner listens (ADR 0025 §4): an ask older
/// than a day wakes no agent, a recent one synced with it does.
#[tokio::test(flavor = "multi_thread")]
async fn history_older_than_a_day_wakes_no_runner() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let now = wats::node::now_secs();
    let asker = |seed: u8| ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let ancient = wats::halt::sign(
        ask("an ancient question"),
        &asker(41),
        now - 3 * 86_400,
        Some(0),
    )?;
    let fresh = wats::halt::sign(ask("a fresh question"), &asker(42), now - 60, Some(0))?;
    for event in [&ancient, &fresh] {
        a.store().insert(event, event.ts)?;
    }
    // The runner listens before the node meets the peer holding them.
    let b = node(&dir.path().join("b"), &[]).await?;
    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: "I answer questions.".into(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec!["mcp".into()],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 10,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&b), config, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.add_peer(a.endpoint().addr()).await?;
    eventually("both asks synced", || {
        b.store().get(&ancient.id).ok().flatten().is_some()
            && b.store().get(&fresh.id).ok().flatten().is_some()
    })
    .await;
    eventually("a run for the fresh ask", || {
        runs(dir.path()).iter().any(|(p, _)| p.contains(&fresh.id))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    for (prompt, _) in runs(dir.path()) {
        assert!(!prompt.contains(&ancient.id), "{prompt}");
    }
    let _ = stop.send(());
    runner.await??;
    for n in [a, b] {
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
        .clear_tool_env()
        .env("HOME", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(["--loopback", "--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args([
            "--agent",
            "claude",
            "--agent-profile",
            "agent",
            // The human below posts from this same node.
            "--ignore-local",
            "false",
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
    assert!(!data.join("logs").join("node.log").exists());

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
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let setup = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("HOME", &home)
            .env("USER", "Vincent.Carluer")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("WATS_PROFILE")
            // --scope project writes ./.claude/settings.json (session hook).
            .current_dir(&project)
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
    assert!(!said.contains(".runtime"), "{said}");
    assert!(!log.exists());

    // --agent only touches config.toml; project scope always re-adds.
    std::fs::remove_file(data.join("config.toml")).unwrap();
    let said = setup(&["--agent"]);
    assert!(said.contains("agents.claude.runtime"), "{said}");
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
    // The session hook goes to the same scope: the project's settings.
    let settings = std::fs::read_to_string(project.join(".claude/settings.json")).unwrap();
    assert!(
        settings.contains("--event user-prompt-submit"),
        "{settings}"
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
        cmd.clear_tool_env()
            .env("HOME", &home)
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
    assert!(banner.contains("on, posts as tester-agent"), "{banner}");
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
        cmd.clear_tool_env()
            .args(["--loopback", "--dir"])
            .arg(&data)
            .args(extra);
        cmd
    };
    let banner = banner_of(&mut base(&[])).await;
    assert!(banner.contains("on, posts as cfg-agent"), "{banner}");
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

/// Run `wats` until a stderr line contains `until` (or 20 s), then stop
/// it; returns stderr.
async fn stderr_until(cmd: &mut tokio::process::Command, until: &str) -> String {
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
            if line.contains(until) {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(20), read).await;
    child.kill().await.unwrap();
    seen
}

/// 0.4.0: an agent without `access` runs with workspace access, in its
/// own workspace; `locked` (config.toml or `--access`) is the old name of
/// swarm-only, with a deprecation note. A declared cap that no tool backs
/// is warned about at start (`image-gen`), an unknown one (`pdf`) never.
#[tokio::test(flavor = "multi_thread")]
async fn access_defaults_to_workspace_and_caps_without_tools_warn() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let agent = "[agents.helper]\nruntime = \"claude\"\ncommand = \"/usr/bin/true\"\n\
                 heartbeat = \"2h\"\ncaps = [\"image-gen\", \"pdf\"]\n";
    let wats = env!("CARGO_BIN_EXE_wats");
    let base = |extra: &[&str]| {
        let mut cmd = tokio::process::Command::new(wats);
        cmd.clear_tool_env()
            .env("HOME", &home)
            .env("USER", "tester")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("WATS_PROFILE")
            .args(["--loopback", "--infra", "none", "--dir"])
            .arg(&data)
            .args(extra);
        cmd
    };
    // The runner warns about the caps, then about the empty about.
    let started = "agent.about is empty";
    std::fs::write(data.join("config.toml"), agent).unwrap();
    let err = stderr_until(&mut base(&[]), started).await;
    let workspace = data.join("agent").join("helper").join("workspace");
    assert!(
        err.contains(&format!("agent access: workspace {}", workspace.display())),
        "{err}"
    );
    assert!(
        err.contains(
            "agent tester-helper declares cap:image-gen but no image tool is enabled \
             (agent.tools); it will wake for such asks and should abstain or clarify"
        ),
        "{err}"
    );
    assert!(!err.contains("cap:pdf"), "{err}");
    assert!(!err.contains("`locked`"), "{err}");

    // An image tool: no warning.
    std::fs::write(
        data.join("config.toml"),
        format!("{agent}tools = [\"mcp__images__generate\"]\n"),
    )
    .unwrap();
    let err = stderr_until(&mut base(&[]), started).await;
    assert!(err.contains(started), "{err}");
    assert!(!err.contains("declares cap:"), "{err}");

    // `locked`, in config.toml and on the command line.
    let swarm_only = "agent access: swarm-only: only the wats tools";
    let note = "access `locked` is now called `swarm-only` (0.4.0)";
    std::fs::write(
        data.join("config.toml"),
        format!("{agent}access = \"locked\"\n"),
    )
    .unwrap();
    let err = banner_of(&mut base(&[])).await;
    assert!(err.contains(swarm_only), "{err}");
    assert!(err.contains(note), "{err}");
    std::fs::write(data.join("config.toml"), agent).unwrap();
    let err = banner_of(&mut base(&["--access", "locked"])).await;
    assert!(err.contains(swarm_only), "{err}");
    assert!(err.contains(note), "{err}");
    let err = banner_of(&mut base(&["--access", "swarm-only"])).await;
    assert!(err.contains(swarm_only) && !err.contains(note), "{err}");
}

/// `wats --dir <data> <args…>` with a fixed OS user; stdout, and whether
/// it succeeded.
fn wats_out(data: &Path, args: &[&str]) -> (bool, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("USER", "tester")
        .env_remove("WATS_PROFILE")
        .arg("--dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `wats status --json` of a running node, once it answers.
async fn running_status(data: &Path) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (ok, out, _) = wats_out(data, &["status", "--json"]);
        if ok
            && let Ok(status) = serde_json::from_str::<serde_json::Value>(&out)
            && status["node"] == "running"
        {
            return status;
        }
        assert!(tokio::time::Instant::now() < deadline, "no running node");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// ADR 0021: two agents of config.toml both run, each with its own
/// profile and directory; `status` lists both; `wats agent runs` needs
/// `--name`.
#[tokio::test(flavor = "multi_thread")]
async fn two_agents_from_config_both_run() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (one, two) = (dir.path().join("one"), dir.path().join("two"));
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    let (fake_one, fake_two) = (fake_agent(&one), fake_agent(&two));
    std::fs::write(
        data.join("config.toml"),
        format!(
            "[agents.claude]\nruntime = \"claude\"\ncommand = \"{}\"\nheartbeat = \"2s\"\n\n\
             [agents.reviewer]\nruntime = \"claude\"\ncommand = \"{}\"\nheartbeat = \"2s\"\n\
             about = \"I review.\"\n\n\
             [agents.off]\nruntime = \"claude\"\nenabled = false\n",
            fake_one.display(),
            fake_two.display()
        ),
    )
    .unwrap();
    let mut node = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .clear_tool_env()
        .env("USER", "tester")
        .env_remove("WATS_PROFILE")
        .args(["--loopback", "--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args(["--quiet"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = running_status(&data).await;
    let runner = status["runner"].as_str().unwrap_or_default().to_string();
    assert!(
        runner.contains("claude as tester-claude") && runner.contains("claude as tester-reviewer"),
        "{status}"
    );
    let agents = status["agents"].as_array().cloned().unwrap_or_default();
    let state = |name: &str| {
        agents
            .iter()
            .find(|a| a["name"] == name)
            .map(|a| (a["state"].clone(), a["profile"].clone()))
    };
    assert_eq!(
        state("claude"),
        Some(("running".into(), "tester-claude".into()))
    );
    assert_eq!(
        state("reviewer"),
        Some(("running".into(), "tester-reviewer".into()))
    );
    assert_eq!(state("off"), Some(("disabled".into(), "tester-off".into())));

    // Both run (a heartbeat each), each with its own fake CLI and
    // directory.
    eventually("both agents ran", || {
        !runs(&one).is_empty() && !runs(&two).is_empty()
    })
    .await;
    let claude = data.join("agent").join("claude");
    let reviewer = data.join("agent").join("reviewer");
    assert!(claude.join("mcp-tester-claude.json").exists());
    assert!(reviewer.join("mcp-tester-reviewer.json").exists());
    assert!(runs(&two)[0].0.contains("I review."), "its own role");
    let cwd = std::fs::read_to_string(two.join("runs").join("run-0.cwd")).unwrap();
    assert!(cwd.contains("reviewer"), "{cwd}");

    // `wats agent runs`: one agent at a time.
    let (ok, _, err) = wats_out(&data, &["agent", "runs"]);
    assert!(!ok && err.contains("several agents"), "{err}");
    eventually("the run is indexed", || {
        let (ok, out, _) = wats_out(&data, &["agent", "runs", "--name", "reviewer", "--json"]);
        ok && out.contains("\"profile\":\"tester-reviewer\"")
    })
    .await;
    let (ok, out, _) = wats_out(&data, &["agent", "--name", "claude", "runs", "--json"]);
    assert!(ok && !out.contains("tester-reviewer"), "{out}");
    let (ok, _, err) = wats_out(&data, &["agent", "runs", "--name", "nobody"]);
    assert!(!ok && err.contains("no agent named"), "{err}");
    node.kill().await.unwrap();
    let _ = node.wait().await;

    // Not running: what `wats` would run.
    let (_, out, _) = wats_out(&data, &["status"]);
    assert!(
        out.contains("agent claude (claude): enabled, posts as tester-claude"),
        "{out}"
    );
    assert!(out.contains("agent reviewer (claude): enabled"), "{out}");
    assert!(out.contains("agent off (claude): disabled"), "{out}");
}

/// `--agent claude:reviewer`: the runtime claude as the agent reviewer,
/// with `[agents.reviewer]`'s settings; flags apply to every agent, but
/// `--agent-profile` only to one.
#[tokio::test(flavor = "multi_thread")]
async fn agent_flag_takes_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "[agents.reviewer]\nprofile = \"rev\"\nheartbeat = \"2h\"\n",
    )
    .unwrap();
    let base = |extra: &[&str]| {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
        cmd.clear_tool_env()
            .env("USER", "tester")
            .env_remove("WATS_PROFILE")
            .args(["--loopback", "--dir"])
            .arg(&data)
            .args(["--agent-command", "/usr/bin/true"])
            .args(extra);
        cmd
    };
    // No runtime in the table: a bare `wats` runs nothing.
    let banner = banner_of(&mut base(&[])).await;
    assert!(banner.contains("agent: off"), "{banner}");
    let banner = banner_of(&mut base(&["--agent", "claude:reviewer"])).await;
    assert!(
        banner.contains("agent reviewer (claude, default model): on, posts as rev"),
        "{banner}"
    );
    assert!(banner.contains("heartbeat 2h"), "{banner}");
    // A new name: the defaults; flags apply to both.
    let banner = banner_of(&mut base(&[
        "--agent",
        "claude:reviewer",
        "--agent",
        "claude:helper",
        "--every",
        "5m",
    ]))
    .await;
    assert!(
        banner.contains("agents: reviewer (claude as rev), helper (claude as tester-helper)"),
        "{banner}"
    );
    assert_eq!(banner.matches("every 5m at most").count(), 2, "{banner}");
    assert!(
        banner.contains("wats agent logs -f --name helper"),
        "{banner}"
    );
    let out = base(&[
        "--agent",
        "claude",
        "--agent",
        "claude:helper",
        "--agent-profile",
        "x",
    ])
    .output()
    .await
    .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--agent-profile"),
        "{out:?}"
    );
    let out = base(&["--agent", "claude:Bad"]).output().await.unwrap();
    assert!(!out.status.success(), "names are checked");
}

/// ADR 0021 §3: a background node (`--idle-exit`, as `wats mcp` starts
/// it) runs no agent unless `agent.with_mcp = true`.
#[tokio::test(flavor = "multi_thread")]
async fn background_node_runs_agents_only_with_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config =
        "[agents.claude]\nruntime = \"claude\"\ncommand = \"/usr/bin/true\"\nheartbeat = \"1h\"\n";
    std::fs::write(data.join("config.toml"), config).unwrap();
    let start = || {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .clear_tool_env()
            .env("USER", "tester")
            .env_remove("WATS_PROFILE")
            .args(["--loopback", "--pow-bits=0", "--min-pow-bits=0", "--dir"])
            .arg(&data)
            .args(["node", "--quiet", "--idle-exit", "60"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let mut node = start();
    let status = running_status(&data).await;
    assert_eq!(status["mode"], "background", "{status}");
    assert!(status["runner"].is_null(), "off by default: {status}");
    assert_eq!(status["agents"][0]["state"], "enabled", "{status}");
    node.kill().await.unwrap();
    let _ = node.wait().await;

    std::fs::write(
        data.join("config.toml"),
        format!("{config}\n[agent]\nwith_mcp = true\n"),
    )
    .unwrap();
    let mut node = start();
    let status = running_status(&data).await;
    assert_eq!(status["runner"], "claude as tester-claude", "{status}");
    assert_eq!(status["agents"][0]["state"], "running", "{status}");
    node.kill().await.unwrap();
    let _ = node.wait().await;
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
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 4,
        // Each fake run reports $0.0125: two runs exhaust it.
        budget: wats::budget::Budget {
            max_usd_per_day: Some(0.02),
            ..Default::default()
        },
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: Some("haiku".into()),
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("two runs", || runs(dir.path()).len() >= 2).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let all = runs(dir.path());
    assert_eq!(all.len(), 2, "paused after the budget is spent");
    assert!(all[0].1.contains("--model haiku"), "{}", all[0].1);
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
    // A throwaway HOME: setup also looks at ~/.codex for the session hook.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let setup = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("USER", "alice")
            .env("HOME", &home)
            .env_remove("CODEX_HOME")
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
    assert!(said.contains("agents.codex.runtime = \"codex\""), "{said}");
    let calls = std::fs::read_to_string(&log).unwrap();
    let add = calls.lines().find(|l| l.starts_with("mcp add")).unwrap();
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
        ignore_local: true,
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
        extra_args: vec!["--sandbox-note".into()],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: Some("gpt-test".into()),
        work_dir: work.clone(),
        data_dir: work.parent().unwrap().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
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
        "--model gpt-test",
        "--sandbox-note -",
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
    // Codex reports LLM tokens (ADR 0034): 10 in, 2 out.
    assert_eq!(record.tokens, Some(12));
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
    let spend = wats::budget::spend(&work, now);
    assert!(spend.uncosted_today >= 1);
    assert!(spend.tokens_today >= 12, "{spend:?}");
    a.shutdown().await?;
    Ok(())
}

/// The `cap:image-gen` demo agent of docs/agent.md: Codex in workspace
/// access with `tools = ["image_generation"]`. The fake Codex does what
/// the real one does (rust-v0.163.0): `thread.started` first, then the
/// image saved to `$CODEX_HOME/generated_images/<thread>/<call>.png`;
/// then it answers the ask through the wats MCP server with that file as
/// a blob. The run's blobs may come from its own thread's directory only:
/// another thread's image, or another file of CODEX_HOME, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn codex_image_generation_answers_with_the_image() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let dir_path = std::fs::canonicalize(dir.path())?;
    let data = dir_path.join("data");
    let out = dir_path.join("out");
    let home = dir_path.join("home");
    let codex_home = dir_path.join("codex-home");
    for d in [&data, &out, &home, &codex_home] {
        std::fs::create_dir_all(d)?;
    }
    let wats = env!("CARGO_BIN_EXE_wats");
    let thread = "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";
    let fake = dir_path.join("codex");
    let call = |id: u32, path: &str| {
        format!(
            r#"call '{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"reply","arguments":{{"post_id":"'$ask'","text":"Here it is.","blobs":[{{"path":"{path}"}}]}}}}}}'
wait_id {id}"#
        )
    };
    let images = codex_home.join("generated_images");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
if [ "$1" = features ]; then
  printf 'shell_tool stable true\napps stable true\nimage_generation stable true\nview_image stable true\n'
  exit 0
fi
echo "$@" > {out}/args
cat > {out}/prompt
token=$(sed -n 's/.*WATS_RUNNER_TOKEN="\([0-9a-f]*\)".*/\1/p' {out}/args)
ask=$(sed -n 's/^- \([0-9a-f]\{{64\}}\) · ask .*/\1/p' {out}/prompt | head -n 1)
echo '{{"type":"thread.started","thread_id":"{thread}"}}'
mkdir -p "{images}/{thread}" "{images}/other-thread"
printf '\211PNG\r\n\032\nimage' > "{images}/{thread}/ig_1.png"
printf '\211PNG\r\n\032\nother' > "{images}/other-thread/ig_9.png"
printf '\211PNG\r\n\032\nstray' > "{codex_home}/stray.png"
fifo={out}/mcp.in
rm -f $fifo; mkfifo $fifo
WATS_RUNNER_TOKEN=$token {wats} --loopback --dir {data} mcp --profile imagegen < $fifo > {out}/mcp.out 2> {out}/mcp.err &
pid=$!
exec 3> $fifo
call() {{ echo "$1" >&3; }}
wait_id() {{
  n=0
  until grep -q "\"id\":$1," {out}/mcp.out 2>/dev/null; do
    sleep 0.1; n=$((n+1)); [ $n -gt 300 ] && break
  done
}}
call '{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"fake-codex","version":"0"}}}}}}'
wait_id 1
call '{{"jsonrpc":"2.0","method":"notifications/initialized"}}'
{other}
{stray}
{own}
exec 3>&-
wait $pid
echo '{{"type":"item.completed","item":{{"id":"i2","type":"agent_message","text":"Answered with the image."}}}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":10,"output_tokens":2}}}}'
"#,
            out = out.display(),
            images = images.display(),
            codex_home = codex_home.display(),
            data = data.display(),
            other = call(2, &format!("{}/other-thread/ig_9.png", images.display())),
            stray = call(3, &format!("{}/stray.png", codex_home.display())),
            own = call(4, &format!("{}/{thread}/ig_1.png", images.display())),
        ),
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    std::fs::write(
        data.join("config.toml"),
        format!(
            "[agents.imagegen]\nruntime = \"codex\"\naccess = \"workspace\"\n\
             tools = [\"image_generation\"]\ncaps = [\"image-gen\"]\n\
             about = \"I generate images on request (Codex image generation).\"\n\
             profile = \"imagegen\"\ncommand = \"{}\"\nignore_local = false\n\
             every = \"1s\"\nheartbeat = \"1h\"\n",
            fake.display()
        ),
    )?;
    let mut node = tokio::process::Command::new(wats)
        .clear_tool_env()
        .env("HOME", &home)
        .env("CODEX_HOME", &codex_home)
        .env("USER", "tester")
        .env_remove("WATS_PROFILE")
        .args(["--loopback", "--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args(["--quiet"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    running_status(&data).await;
    let mut cmd = tokio::process::Command::new(wats);
    cmd.env("HOME", &home)
        .args(["--loopback", "--profile", "human", "--dir"])
        .arg(&data)
        .args(["mcp", "--idle-exit", "1"]);
    let human = ().serve(TokioChildProcess::new(cmd)?).await?;
    let call_tool = |name: &'static str, args: serde_json::Value| {
        let human = &human;
        async move {
            let result = human
                .call_tool(
                    CallToolRequestParams::new(name)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            result
                .structured_content
                .clone()
                .unwrap_or_else(|| panic!("{name}: {result:?}"))
        }
    };
    let asked = call_tool(
        "post",
        serde_json::json!({
            "title": "A lighthouse at dusk, as a PNG",
            "text": "Generate an image of a lighthouse at dusk.",
            "kind": "ask",
            "tags": ["cap:image-gen"],
            "data": {"mode": "one"},
        }),
    )
    .await;
    let ask = asked["id"].as_str().unwrap().to_string();
    let work = data.join("agent").join("imagegen");
    eventually("the image run is indexed", || {
        wats::runs::list(&work).iter().any(|r| r.status == "ok")
    })
    .await;

    let args = std::fs::read_to_string(out.join("args"))?;
    assert!(args.contains("--enable image_generation"), "{args}");
    assert!(!args.contains("--disable image_generation"), "{args}");
    for off in ["shell_tool", "apps", "view_image"] {
        assert!(args.contains(&format!("--disable {off}")), "{args}");
    }
    assert!(args.contains("--ephemeral --skip-git-repo-check"), "{args}");
    assert!(args.contains("--sandbox read-only"), "{args}");
    let prompt = std::fs::read_to_string(out.join("prompt"))?;
    assert!(prompt.contains(&ask), "{prompt}");
    assert!(prompt.contains(wats::access::IMAGES_LINE), "{prompt}");

    let mcp = std::fs::read_to_string(out.join("mcp.out"))?;
    let answer = |id: u32| {
        mcp.lines()
            .find(|l| l.contains(&format!("\"id\":{id},")))
            .unwrap_or_default()
            .to_string()
    };
    for refused in [answer(2), answer(3)] {
        assert!(
            refused.contains("outside the directories this node may share from"),
            "{refused}\n{mcp}"
        );
    }
    let own: serde_json::Value = serde_json::from_str(&answer(4))?;
    let reply = own["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{mcp}"))
        .to_string();
    let read = call_tool("read", serde_json::json!({"id": reply})).await;
    let read = &read["post"];
    assert_eq!(read["kind"], "answer", "{read}");
    assert_eq!(read["parent"], ask.as_str(), "{read}");
    let blobs = read["blobs"].as_array().cloned().unwrap_or_default();
    assert_eq!(blobs.len(), 1, "{read}");
    assert_eq!(blobs[0]["name"], "ig_1.png", "{read}");
    assert_eq!(blobs[0]["size"], 13, "{read}");

    // The transcript has Codex's events, the footer after them.
    let record = wats::runs::list(&work)
        .into_iter()
        .find(|r| r.status == "ok")
        .unwrap();
    assert_eq!(record.turns, Some(1));
    let transcript = std::fs::read_to_string(wats::runs::transcript_path(&work, &record.id))?;
    assert!(transcript.contains("thread.started"), "{transcript}");
    assert!(
        transcript
            .trim_end()
            .lines()
            .last()
            .unwrap()
            .contains("wats_end"),
        "{transcript}"
    );
    human.cancel().await?;
    node.kill().await?;
    Ok(())
}

/// `tools` on a swarm-only agent: `wats config set` keeps it with a
/// warning, and the agent still starts, with only the wats tools. With
/// access unset (workspace), the tools are used. `locked`, the old name of
/// swarm-only, is stored as `swarm-only` with a deprecation note.
#[tokio::test(flavor = "multi_thread")]
async fn swarm_only_agent_with_tools_warns_and_starts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data)?;
    let (ok, _, err) = wats_out(&data, &["config", "set", "agents.claude.runtime", "claude"]);
    assert!(ok, "{err}");
    let (ok, _, err) = wats_out(
        &data,
        &["config", "set", "agents.claude.tools", "Read,Grep"],
    );
    assert!(ok, "{err}");
    assert!(err.contains("a post can ask the agent to leak"), "{err}");
    assert!(!err.contains("access ignores"), "{err}");
    let (ok, out, _) = wats_out(&data, &["config", "get", "agents.claude.access"]);
    assert!(ok && out.starts_with("workspace (default)"), "{out}");
    let (ok, _, err) = wats_out(&data, &["config", "set", "agents.claude.access", "locked"]);
    assert!(ok, "{err}");
    assert_eq!(
        err.matches("note: access `locked` is now called `swarm-only`")
            .count(),
        1,
        "{err}"
    );
    assert!(
        err.contains(
            "warning: swarm-only access ignores agents.claude.tools; use access workspace"
        ),
        "{err}"
    );
    assert_eq!(err.matches("warning:").count(), 1, "{err}");
    let config = std::fs::read_to_string(data.join("config.toml"))?;
    assert!(config.contains("access = \"swarm-only\""), "{config}");
    let (ok, _, err) = wats_out(
        &data,
        &["config", "set", "agents.claude.access", "workspace"],
    );
    assert!(ok && !err.contains("access ignores"), "{err}");
    let (ok, _, err) = wats_out(
        &data,
        &["config", "set", "agents.claude.tools", "image_generation"],
    );
    assert!(ok, "{err}");
    assert!(err.contains("a post can ask the agent to leak"), "{err}");
    assert!(err.contains("referenced_image_paths"), "{err}");

    let a = node(&dir.path().join("a"), &[]).await?;
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
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/opt/wats".into(),
        mcp_args: vec!["mcp".into()],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec!["Read".into(), "image_generation".into()],
        servers: Vec::new(),
        model: None,
        work_dir: work.clone(),
        data_dir: work.parent().unwrap().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("a swarm-only run", || !runs(dir.path()).is_empty()).await;
    let _ = stop.send(());
    runner.await??;
    let (_, args) = runs(dir.path()).remove(0);
    assert!(
        args.contains("--tools  --permission-mode dontAsk"),
        "{args}"
    );
    assert!(args.contains("--allowedTools mcp__wats__* "), "{args}");
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
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/opt/wats".into(),
        mcp_args: vec!["mcp".into()],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::Workspace,
        workdir: workdir.clone(),
        tools: vec!["Read".into(), "Grep".into()],
        servers: Vec::new(),
        model: None,
        work_dir: work.clone(),
        data_dir: work.parent().unwrap().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
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
    // Deny rules: the data directory (`work`'s parent) but the workspace,
    // and the wats commands that change the node.
    let data = dir.path().display();
    for rule in [
        format!("Edit(/{data}/config.toml)"),
        format!("Write(/{data}/config.toml)"),
        format!("Edit(/{data}/safety.json)"),
        format!("Write(/{data}/work/**)"),
        format!("Edit(/{}/.claude/**)", workdir.display()),
        "Bash(*wats config*)".to_string(),
        "Bash(*wats resume*)".to_string(),
    ] {
        assert!(args.contains(&format!("\"{rule}\"")), "{rule}: {args}");
    }
    assert!(!args.contains(&format!("Edit(/{data}/ws/**)")), "{args}");
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
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
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
            .runner_session(wats::node::RunnerRun::default()),
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
    wats::review::reject(&data, by_runner["held"].as_str().unwrap(), None)?;
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
/// profile); without it, or with a made-up one, it is not. OpenClaw's
/// persistent token (`openclaw/runner.token`) counts too, and the `status`
/// of an OpenClaw runner says its budget is OpenClaw's.
#[tokio::test(flavor = "multi_thread")]
async fn runner_token_marks_the_session_over_the_socket() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::transport::TokioChildProcess;
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("n");
    let mut config = NodeConfig::new(&data);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.review_profiles = vec!["agent".into()];
    config.runners = vec!["openclaw as agent".into()];
    let node = Node::start(config).await?;
    let _sessions = wats::attach::serve(Arc::clone(&node))?;
    // An OpenClaw runner: its budget is OpenClaw's.
    let status = wats::attach::call_tool(&data, "status", serde_json::json!({}))
        .await?
        .unwrap();
    assert_eq!(status["budget"], "managed by OpenClaw", "{status}");
    let token = node.new_runner_token(wats::node::RunnerRun::default());
    // The persistent token of OpenClaw's `wats-agent` entry.
    let persistent = "cd".repeat(32);
    let persistent_file = wats::openclaw::runner_token_path(&data);
    std::fs::create_dir_all(persistent_file.parent().unwrap())?;
    std::fs::write(&persistent_file, format!("{persistent}\n"))?;
    let session = |token: Option<&str>| {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
        cmd.clear_tool_env()
            .args(["--loopback", "--profile", "human", "--dir"])
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
        (Some(persistent.as_str()), true),
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
    // A new agent (ADR 0021): its own directory, agent/claude.
    assert!(data.join("agent/claude/workspace/CLAUDE.md").exists());
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(config.contains("access = \"workspace\""), "{config}");
    assert!(config.contains("tools = [\"Read\", \"Grep\"]"), "{config}");
    assert!(wats(&["config", "list"]).contains("agents.claude.access"));

    let draft = Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some("held title".into()),
        text: "held text".into(),
        ..Draft::default()
    };
    let one = wats::review::hold(&data, "agent", &draft, &[], Default::default()).unwrap();
    let two = wats::review::hold(&data, "agent", &draft, &[], Default::default()).unwrap();
    let pending = wats(&["agent", "pending"]);
    assert!(
        pending.contains(&one.id) && pending.contains("held title"),
        "{pending}"
    );
    assert!(wats(&["agent", "approve", &one.id]).contains("approved"));
    assert!(
        wats(&["agent", "reject", &two.id, "--reason", "too vague"])
            .contains("the agent reads the reason")
    );
    let pending = wats(&["agent", "pending"]);
    assert!(
        pending.contains("[approved]") && !pending.contains(&two.id),
        "{pending}"
    );
    assert_eq!(
        wats::review::take_feedback(&data, "agent"),
        ["- Your note \"held title\" was rejected by your human: too vague"]
    );
    assert!(
        wats::review::take_feedback(&data, "agent").is_empty(),
        "consumed"
    );

    // show, and approve --edit through $EDITOR.
    let three = wats::review::hold(&data, "agent", &draft, &[], Default::default()).unwrap();
    let shown = wats(&["agent", "pending", "show", &three.id]);
    assert!(
        shown.contains("title: held title") && shown.contains("held text"),
        "{shown}"
    );
    let editor = dir.path().join("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf 'Edited title\\n\\nEdited text.\\n' > \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("EDITOR", &editor)
        .env_remove("VISUAL")
        .arg("--dir")
        .arg(&data)
        .args(["agent", "approve", "--edit", &three.id])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let edited = wats::review::load(&data, &three.id).unwrap();
    assert_eq!(edited.title.as_deref(), Some("Edited title"));
    assert_eq!(edited.text, "Edited text.");
    assert_eq!(edited.status, "approved");

    // Expiry.
    let four = wats::review::hold(&data, "agent", &draft, &[], Default::default()).unwrap();
    std::thread::sleep(Duration::from_millis(2100));
    let expired = wats::review::expire(&data, Duration::from_secs(1));
    assert!(expired.iter().any(|h| h.id == four.id));
    assert!(
        expired.iter().all(|h| h.id != three.id),
        "approved ones stay"
    );
}

/// MCP review tools: interactive sessions get `pending`, `approve` and
/// `reject`; runner sessions never see them. A held draft records the run,
/// what woke it and the post it answers.
#[tokio::test(flavor = "multi_thread")]
async fn review_tools_are_for_humans_only() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("a");
    let mut config = NodeConfig::new(&data);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.review_profiles = vec!["agent".into()];
    let a = Node::start(config).await?;
    let ask = a.publish(ask("Convert my CSV"), &[]).await?;
    let serve = |server: wats::mcp::WatsMcp| async move {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await
    };
    let human = serve(wats::mcp::WatsMcp::new(Arc::clone(&a), vec![])).await?;
    let run = wats::node::RunnerRun {
        id: "20261016T000000Z-events".into(),
        events: vec![ask.id.clone()],
        ..Default::default()
    };
    let agent = serve(
        wats::mcp::WatsMcp::new(Arc::clone(&a), vec![])
            .with_profile("agent")?
            .runner_session(run),
    )
    .await?;
    let names = |tools: Vec<rmcp::model::Tool>| -> Vec<String> {
        tools.into_iter().map(|t| t.name.to_string()).collect()
    };
    let human_tools = names(human.list_all_tools().await?);
    let agent_tools = names(agent.list_all_tools().await?);
    for tool in ["pending", "approve", "reject", "start_daemon"] {
        assert!(human_tools.iter().any(|t| t == tool), "{human_tools:?}");
        assert!(!agent_tools.iter().any(|t| t == tool), "{agent_tools:?}");
    }
    assert!(agent_tools.iter().any(|t| t == "status"), "{agent_tools:?}");
    let reply = agent
        .call_tool(
            rmcp::model::CallToolRequestParams::new("reply").with_arguments(
                serde_json::json!({"post_id": ask.id, "text": "Here is the CSV."})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?
        .structured_content
        .unwrap();
    let id = reply["held"].as_str().unwrap().to_string();
    let held = wats::review::load(&data, &id)?;
    assert_eq!(held.run.as_deref(), Some("20261016T000000Z-events"));
    assert_eq!(held.woke_for, std::slice::from_ref(&ask.id));
    assert_eq!(held.thread_title.as_deref(), Some("Convert my CSV"));
    assert_eq!(held.in_reply_to.as_ref().unwrap()["kind"], "ask");

    let listed = human
        .call_tool(rmcp::model::CallToolRequestParams::new("pending"))
        .await?
        .structured_content
        .unwrap();
    assert_eq!(listed["pending"][0]["id"], id.as_str());
    human
        .call_tool(
            rmcp::model::CallToolRequestParams::new("approve")
                .with_arguments(serde_json::json!({"id": id}).as_object().unwrap().clone()),
        )
        .await?;
    let published = a.store().search(Some("CSV"), &Default::default(), 10)?;
    assert!(
        published.iter().any(|s| s.event.text == "Here is the CSV."),
        "approved from MCP and published"
    );
    human.cancel().await?;
    agent.cancel().await?;
    a.shutdown().await?;
    Ok(())
}

/// `wats agent attach` and `logs -f` follow forever: Ctrl-C (SIGINT) and
/// SIGTERM end them.
#[test]
fn follow_stops_on_signals() {
    let dir = tempfile::tempdir().unwrap();
    for (args, signal) in [(&["attach"][..], "-INT"), (&["logs", "-f"][..], "-TERM")] {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .arg("--dir")
            .arg(dir.path())
            .arg("agent")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(800));
        std::process::Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("`wats agent {args:?}` ignored {signal}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The wake policy, after the first real exchange: an agent wakes on asks
/// and on actionable direct replies to its own posts. Not on a sibling
/// answer to the same ask, not on a react to its answer (kept for the next
/// run as feedback), not on a post by another profile of its own node.
#[tokio::test(flavor = "multi_thread")]
async fn wakes_only_on_what_is_addressed_to_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    let c = node(&dir.path().join("c"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let reply_to = |parent: &wats::event::Event, kind: Kind, text: &str| Draft {
        net: PUBLIC_NET.into(),
        thread: Some(parent.thread_root().to_string()),
        parent: Some(parent.id.clone()),
        kind: Some(kind),
        text: text.into(),
        ..Draft::default()
    };
    // Before the runner starts: b asks, the agent answers.
    let question = b.publish(ask("Which CSV delimiter?"), &[]).await?;
    let agent_key = a.author_key("agent")?;
    eventually_async(|| async { a.store().get(&question.id).unwrap().is_some() }).await;
    let answer = a
        .publish_as(
            &agent_key,
            reply_to(&question, Kind::Answer, "Use a comma."),
            &[],
        )
        .await?;
    eventually_async(|| async { b.store().get(&answer.id).unwrap().is_some() }).await;

    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // None of these wakes it.
    let sibling = c
        .publish(reply_to(&question, Kind::Answer, "A semicolon."), &[])
        .await?;
    let reaction = b
        .publish(reply_to(&answer, Kind::React, "Works, thanks."), &[])
        .await?;
    let local = a
        .publish_as(&a.author_key("human")?, ask("My own question"), &[])
        .await?;
    eventually_async(|| async {
        [&sibling.id, &reaction.id, &local.id]
            .iter()
            .all(|id| a.store().get(id).unwrap().is_some())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        runs(dir.path()).is_empty(),
        "nothing addressed to the agent yet"
    );

    // A follow-up question to its answer does.
    let follow_up = b
        .publish(reply_to(&answer, Kind::Ask, "And for TSV?"), &[])
        .await?;
    eventually("a run", || !runs(dir.path()).is_empty()).await;
    let (prompt, _) = runs(dir.path()).remove(0);
    assert!(prompt.contains(&follow_up.id), "{prompt}");
    assert!(prompt.contains("Feedback on your posts"), "{prompt}");
    assert!(
        prompt.contains(&reaction.id),
        "the react comes as feedback: {prompt}"
    );
    assert!(!prompt.contains(&sibling.id), "{prompt}");
    assert!(!prompt.contains(&local.id), "{prompt}");
    let _ = stop.send(());
    runner.await??;
    for n in [a, b, c] {
        n.shutdown().await?;
    }
    Ok(())
}

/// Wake limits: a second reply in the same thread wakes the agent again
/// (no per-thread backoff); after a restart, the per-author limit still
/// counts the runs of the run index.
#[tokio::test(flavor = "multi_thread")]
async fn wake_limits_defer_and_survive_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let reply_to = |parent: &wats::event::Event, text: &str| Draft {
        net: PUBLIC_NET.into(),
        thread: Some(parent.thread_root().to_string()),
        parent: Some(parent.id.clone()),
        kind: Some(Kind::Answer),
        text: text.into(),
        ..Draft::default()
    };
    // The agent asked something before the runner starts.
    let question = a
        .publish_as(&a.author_key("agent")?, ask("Who has a CSV sample?"), &[])
        .await?;
    eventually_async(|| async { b.store().get(&question.id).unwrap().is_some() }).await;

    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config.clone(), async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let first = b.publish(reply_to(&question, "I do."), &[]).await?;
    eventually("a run", || !runs(dir.path()).is_empty()).await;
    let second = b
        .publish(reply_to(&question, "Here is another one."), &[])
        .await?;
    eventually("the second reply's run", || runs(dir.path()).len() >= 2).await;
    let all = runs(dir.path());
    assert!(all[0].0.contains(&first.id), "{}", all[0].0);
    assert!(all[1].0.contains(&second.id), "{}", all[1].0);
    assert!(!all[1].0.contains(&first.id), "{}", all[1].0);
    assert_eq!(wats::runs::list(&dir.path().join("work")).len(), 2);
    let _ = stop.send(());
    runner.await??;

    // Restarted with one run per author and hour: b already had two.
    let config = AgentConfig {
        wakes: wats::wake::WakeLimits {
            per_author_per_hour: 1,
            ..config.wakes
        },
        ..config
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let new_ask = b.publish(ask("A new question"), &[]).await?;
    eventually_async(|| async { a.store().get(&new_ask.id).unwrap().is_some() }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(runs(dir.path()).len(), 2, "b's runs of the last hour count");
    let _ = stop.send(());
    runner.await??;
    for n in [a, b] {
        n.shutdown().await?;
    }
    Ok(())
}

/// Loop detector (agent.loop_max_exchanges = 3): once the agent and b have
/// taken more than 3 turns in a row in a thread, b's next reply no longer
/// wakes the agent; it is kept, and wakes it as soon as a third author (c)
/// posts in the thread.
#[tokio::test(flavor = "multi_thread")]
async fn loop_detector_pauses_a_thread_until_a_third_party_posts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    let c = node(&dir.path().join("c"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let reply_to = |parent: &wats::event::Event, kind: Kind, text: &str| Draft {
        net: PUBLIC_NET.into(),
        thread: Some(parent.thread_root().to_string()),
        parent: Some(parent.id.clone()),
        kind: Some(kind),
        text: text.into(),
        ..Draft::default()
    };
    let agent_key = a.author_key("agent")?;
    // Turn 1: the agent asks.
    let question = a
        .publish_as(&agent_key, ask("Who has a CSV sample?"), &[])
        .await?;
    eventually_async(|| async {
        b.store().get(&question.id).unwrap().is_some()
            && c.store().get(&question.id).unwrap().is_some()
    })
    .await;

    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits {
            per_author_per_hour: 0,
            loop_max_exchanges: 3,
        },
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Turn 2: b answers; 2 turns, the agent wakes.
    let answer = b
        .publish(reply_to(&question, Kind::Answer, "I do."), &[])
        .await?;
    eventually("a run", || !runs(dir.path()).is_empty()).await;
    // Turn 3: the (fake) agent asks for more.
    let clarify = a
        .publish_as(
            &agent_key,
            reply_to(&answer, Kind::Clarify, "With headers?"),
            &[],
        )
        .await?;
    eventually_async(|| async { b.store().get(&clarify.id).unwrap().is_some() }).await;
    // Turn 4: b replies; 4 turns > 3, the thread is paused for the agent.
    let again = b
        .publish(reply_to(&clarify, Kind::Answer, "Yes, with headers."), &[])
        .await?;
    eventually_async(|| async { a.store().get(&again.id).unwrap().is_some() }).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(runs(dir.path()).len(), 1, "paused: loop detected");

    // A third author posts in the thread (not to the agent: it does not
    // wake it by itself); b's held reply now wakes the agent.
    let third = c
        .publish(
            reply_to(&question, Kind::Note, "Try the open data portal."),
            &[],
        )
        .await?;
    eventually("the held reply's run", || runs(dir.path()).len() >= 2).await;
    let all = runs(dir.path());
    assert!(all[1].0.contains(&again.id), "{}", all[1].0);
    // Woken by b's held reply, not by c's post.
    let index = wats::runs::list(&dir.path().join("work"));
    assert_eq!(index.len(), 2);
    assert_eq!(index[1].events, std::slice::from_ref(&again.id));
    assert!(!index[1].events.contains(&third.id));
    let _ = stop.send(());
    runner.await??;
    for n in [a, b, c] {
        n.shutdown().await?;
    }
    Ok(())
}

async fn eventually_async<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `--agents-md` adds a marked block to the tool's global instructions
/// (replaced in place, never duplicated), `--remove-agents-md` takes it
/// out and leaves the rest; `setup codex --no-approval` sets the wats
/// server's approval mode in the Codex config, keeping its comments.
#[test]
fn setup_instructions_block_and_codex_approval() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/CLAUDE.md"), "# Mine\nBe brief.\n").unwrap();
    let (claude, _) = logging_claude(dir.path());
    let data = dir.path().join("data");
    let setup = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("HOME", &home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .arg("--dir")
            .arg(&data)
            .arg("setup")
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
    let claude = claude.to_str().unwrap();
    assert!(setup(&["claude", "--mcp", "--claude", claude]).contains("Tip: add --agents-md"));
    assert!(
        setup(&["claude", "--mcp", "--agents-md", "--claude", claude]).contains("wats block added")
    );
    assert!(
        setup(&["claude", "--mcp", "--agents-md", "--claude", claude]).contains("already there")
    );
    let text = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
    assert!(
        text.starts_with("# Mine\nBe brief.\n\n<!-- wats:begin -->"),
        "{text}"
    );
    assert_eq!(text.matches("wats:begin").count(), 1);
    assert!(text.contains("\"the swarm\""), "{text}");
    assert!(
        setup(&["claude", "--mcp", "--remove-agents-md", "--claude", claude]).contains("removed")
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(),
        "# Mine\nBe brief.\n"
    );

    // Codex: the fake's `mcp get` shows the same entry, the config has it.
    use std::os::unix::fs::PermissionsExt;
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    let entry = serde_json::json!({"name": "wats", "transport": {"type": "stdio",
        "command": exe, "args": ["--dir", data, "mcp", "--profile", "alice"]}});
    let current = dir.path().join("current.json");
    std::fs::write(&current, entry.to_string()).unwrap();
    let codex = dir.path().join("codex");
    std::fs::write(
        &codex,
        format!(
            "#!/bin/sh\n[ \"$2\" = get ] && cat {} && exit 0\nexit 0\n",
            current.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        "# keep me\n[mcp_servers.wats]\ncommand = \"x\"\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("HOME", &home)
        .env("USER", "alice")
        .env_remove("CODEX_HOME")
        .env_remove("WATS_PROFILE")
        .arg("--dir")
        .arg(&data)
        .args(["setup", "codex", "--mcp", "--no-approval", "--codex"])
        .arg(&codex)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(config.contains("# keep me"), "{config}");
    assert!(
        config.contains("default_tools_approval_mode = \"approve\""),
        "{config}"
    );
}

/// One request the OpenClaw gateway stub received.
#[derive(Debug, Clone)]
struct HookRequest {
    line: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

impl HookRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A stub of the OpenClaw gateway on 127.0.0.1: it records each request
/// and answers with the next scripted `(status, body, delay)`.
fn gateway_stub(
    script: Vec<(u16, &'static str, Duration)>,
) -> (String, Arc<std::sync::Mutex<Vec<HookRequest>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut script = script.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (k, v) = header.split_once(':').unwrap();
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
            let length: usize = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.parse().unwrap());
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(HookRequest {
                line: line.trim_end().to_string(),
                headers,
                body: serde_json::from_slice(&body).unwrap_or_default(),
            });
            let (status, answer, delay) = script.next().unwrap_or((500, "{}", Duration::ZERO));
            std::thread::sleep(delay);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (url, seen)
}

/// `--agent openclaw`: each run is a POST to the gateway's /hooks/agent
/// with the hook token, the run id as idempotency key, and the wake in the
/// thread's session. 200 records the completion; a 400 on the session
/// policy falls back to isolated sessions for good; 401 fails the run; 503
/// defers the wake to the next run; a slow gateway times the run out.
#[tokio::test(flavor = "multi_thread")]
async fn openclaw_forwards_wakes_to_the_gateway() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let ok = r#"{"ok":true,"completion":{"status":"ok"},"replyDisposition":"visible"}"#;
    let (url, seen) = gateway_stub(vec![
        (200, ok, Duration::ZERO),
        (
            400,
            r#"{"error":"sessionKey is not allowed by the hooks session policy"}"#,
            Duration::ZERO,
        ),
        (
            200,
            r#"{"completion":{"status":"ok"},"replyDisposition":"silent"}"#,
            Duration::ZERO,
        ),
        (401, r#"{"error":"unauthorized"}"#, Duration::ZERO),
        (503, r#"{"error":"starting"}"#, Duration::ZERO),
        (200, ok, Duration::ZERO),
        (200, ok, Duration::from_secs(6)),
    ]);
    let token = "ab".repeat(32);
    let token_file = dir.path().join("hooks.token");
    std::fs::write(&token_file, format!("{token}\n"))?;
    let work = dir.path().join("work");
    let config = AgentConfig {
        runtime: "openclaw".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: false,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: "/nonexistent/openclaw".into(),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(1),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: Some("cheap-model".into()),
        work_dir: work.clone(),
        data_dir: work.parent().unwrap().to_path_buf(),
        keep_runs: 50,
        keep_days: 30,
        openclaw: wats::openclaw::Target {
            url: url.clone(),
            agent: "wats".into(),
            token_file,
        },
        catchup: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let human = a.author_key("human")?;
    let requests = || seen.lock().unwrap().clone();
    let records = || -> Vec<wats::runs::RunRecord> {
        wats::runs::list(&work)
            .into_iter()
            .filter(|r| r.status != "running")
            .collect()
    };

    // 200: the forward, and its completion in the run index.
    let first = a.publish_as(&human, ask("first question"), &[]).await?;
    eventually("an indexed run", || records().len() == 1).await;
    let sent = requests().remove(0);
    let run = records().remove(0);
    assert_eq!(sent.line, "POST /hooks/agent HTTP/1.1");
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(sent.header("idempotency-key"), Some(run.id.as_str()));
    let body = &sent.body;
    assert_eq!(body["name"], "wats");
    assert_eq!(body["agentId"], "wats");
    assert_eq!(body["sessionMode"], "persistent");
    assert_eq!(body["sessionKey"], format!("wats:{}", first.id));
    assert_eq!(body["deliver"], false);
    assert_eq!(body["waitForCompletion"], true);
    assert_eq!(body["timeoutSeconds"], 1);
    assert_eq!(body["model"], "cheap-model");
    let message = body["message"].as_str().unwrap();
    assert!(message.starts_with(wats::openclaw::PREAMBLE), "{message}");
    assert!(message.contains(&first.id), "{message}");
    assert_eq!(
        (run.runtime.as_str(), run.status.as_str()),
        ("openclaw", "ok")
    );
    assert_eq!(run.completion.as_deref(), Some("ok"));
    assert_eq!(run.reply.as_deref(), Some("visible"));
    assert_eq!(run.events, std::slice::from_ref(&first.id));
    let transcript = std::fs::read_to_string(wats::runs::transcript_path(&work, &run.id))?;
    assert!(transcript.contains("openclaw_forward"), "{transcript}");
    assert!(!transcript.contains(&token), "the token is never written");
    assert!(!transcript.contains("first question"), "nor the message");

    // 400 on the session policy: the same run retries without a session
    // key, and so do the later ones.
    let second = a.publish_as(&human, ask("second question"), &[]).await?;
    // The run's record can land before the gateway stub has logged the
    // retry it answered: wait for both.
    eventually("the isolated retry", || {
        records().len() == 2 && requests().len() >= 3
    })
    .await;
    let all = requests();
    assert_eq!(all[1].body["sessionKey"], format!("wats:{}", second.id));
    assert_eq!(all[2].body["sessionMode"], "isolated");
    assert!(all[2].body.get("sessionKey").is_none(), "{}", all[2].body);
    let run = records().remove(1);
    assert_eq!(
        all[2].header("idempotency-key"),
        Some(format!("{}:isolated", run.id).as_str())
    );
    assert_eq!(run.status, "ok");
    assert_eq!(run.reply.as_deref(), Some("silent"));

    // 401: the run fails, pointing to the token.
    a.publish_as(&human, ask("third question"), &[]).await?;
    eventually("a failed run", || {
        records().len() == 3 && requests().len() >= 4
    })
    .await;
    let run = records().remove(2);
    assert_eq!(run.status, "failed");
    assert!(
        run.error.as_deref().unwrap().contains("hook token"),
        "{run:?}"
    );
    assert_eq!(requests()[3].body["sessionMode"], "isolated");

    // 503: deferred, no record; the next run forwards the same wake.
    let fourth = a.publish_as(&human, ask("fourth question"), &[]).await?;
    eventually("the retried run", || {
        records().len() == 4 && requests().len() >= 6
    })
    .await;
    let all = requests();
    assert_eq!(all.len(), 6);
    let message = |n: usize| all[n].body["message"].as_str().unwrap().to_string();
    assert!(message(4).contains(&fourth.id) && message(5).contains(&fourth.id));
    let run = records().remove(3);
    assert_eq!(run.status, "ok");
    assert_eq!(run.events, std::slice::from_ref(&fourth.id));

    // Slower than the timeout: the run times out.
    a.publish_as(&human, ask("fifth question"), &[]).await?;
    eventually("a timed-out run", || records().len() == 5).await;
    assert_eq!(records().remove(4).status, "timeout");

    let _ = stop.send(());
    runner.await??;
    a.shutdown().await?;
    Ok(())
}

/// Asks the runner learns about other than by live gossip still wake it:
/// stored before it started (synced at startup, on a reconnection, or
/// while it was down), when recent, unanswered and not local. Its cursor
/// survives a restart, so an ask already handled never wakes it twice.
#[tokio::test(flavor = "multi_thread")]
async fn asks_stored_before_the_runner_started_still_wake_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let stored = |id: String| {
        let a = Arc::clone(&a);
        move || a.store().get(&id).unwrap().is_some()
    };

    // Before the runner listens: a recent unanswered ask from b...
    let waiting = b.publish(ask("Who can review my CSV?"), &[]).await?;
    // ...an ask someone already answered...
    let answered = b.publish(ask("What is a CSV?"), &[]).await?;
    let answer = b
        .publish_as(
            &b.author_key("helper")?,
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(answered.id.clone()),
                parent: Some(answered.id.clone()),
                kind: Some(Kind::Answer),
                text: "Comma-separated values.".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    // ...the human's own ask on this node...
    let local = a
        .publish_as(&a.author_key("human")?, ask("Note to my agent"), &[])
        .await?;
    // ...and an ask two days old, synced late.
    let old = ask("An old question")
        .sign(
            &ed25519_dalek::SigningKey::from_bytes(&[5; 32]),
            wats::node::now_secs() - 2 * 86_400,
            0,
        )
        .unwrap();
    a.store().insert(&old, wats::node::now_secs())?;
    for id in [&waiting.id, &answered.id, &answer.id, &local.id] {
        eventually("stored on a", stored(id.clone())).await;
    }

    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: Some(wats::agent::DEFAULT_CATCHUP),
    };
    let start = |config: AgentConfig| {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
            let _ = stopped.await;
        }));
        (stop, runner)
    };

    let (stop, runner) = start(config.clone());
    eventually("a catch-up run", || !runs(dir.path()).is_empty()).await;
    let (prompt, _) = runs(dir.path()).remove(0);
    assert!(prompt.contains(&waiting.id), "{prompt}");
    assert!(prompt.contains("1 new ask"), "{prompt}");
    for (left_out, why) in [
        (&answered.id, "answered"),
        (&local.id, "local"),
        (&old.id, "too old"),
    ] {
        assert!(!prompt.contains(left_out.as_str()), "{why}: {prompt}");
    }
    let _ = stop.send(());
    runner.await??;

    // Down for a while: an ask arrives meanwhile. On restart, only it wakes
    // the agent; the one already handled does not.
    let later = b.publish(ask("Anyone around?"), &[]).await?;
    eventually("stored on a", stored(later.id.clone())).await;
    let (stop, runner) = start(config);
    eventually("a second catch-up run", || runs(dir.path()).len() >= 2).await;
    let (prompt, _) = runs(dir.path()).remove(1);
    assert!(prompt.contains(&later.id), "{prompt}");
    assert!(!prompt.contains(&waiting.id), "handled before: {prompt}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(runs(dir.path()).len(), 2, "no other run");
    let _ = stop.send(());
    runner.await??;

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Heartbeat runs offer up to 3 older unanswered asks (between
/// `agent.catchup` and 3 days old), oldest first, each once (ADR 0018 §6).
#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_offer_older_unanswered_asks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let now = wats::node::now_secs();
    let stranger = ed25519_dalek::SigningKey::from_bytes(&[6; 32]);
    let helper = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let old = |title: &str, hours: u64| {
        let event = ask(title).sign(&stranger, now - hours * 3600, 0).unwrap();
        a.store().insert(&event, now).unwrap();
        event
    };
    let h30 = old("thirty hours", 30);
    let h40 = old("forty hours", 40);
    let h50 = old("fifty hours", 50);
    let h60 = old("sixty hours", 60);
    let ancient = old("four days", 96);
    let answered = old("answered", 45);
    let answer = Draft {
        net: PUBLIC_NET.into(),
        thread: Some(answered.id.clone()),
        parent: Some(answered.id.clone()),
        kind: Some(Kind::Answer),
        text: "Done.".into(),
        ..Draft::default()
    }
    .sign(&helper, now - 40 * 3600, 0)
    .unwrap();
    a.store().insert(&answer, now)?;

    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(2),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: Some(wats::agent::DEFAULT_CATCHUP),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("two heartbeats", || runs(dir.path()).len() >= 2).await;
    let _ = stop.send(());
    runner.await??;
    let runs = runs(dir.path());
    let (first, second) = (&runs[0].0, &runs[1].0);
    // None of them woke the agent by itself: older than agent.catchup.
    assert!(first.contains("woken because: the heartbeat"), "{first}");
    assert!(first.contains("Older asks nobody answered"), "{first}");
    let order: Vec<usize> = [&h60, &h50, &h40]
        .iter()
        .map(|e| first.find(&e.id).expect("offered first, oldest first"))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{first}");
    assert!(!first.contains(&h30.id), "3 per heartbeat: {first}");
    assert!(second.contains(&h30.id), "{second}");
    for e in [&h60, &h50, &h40] {
        assert!(!second.contains(&e.id), "offered once: {second}");
    }
    for (left_out, why) in [(&ancient, "too old"), (&answered, "answered")] {
        for prompt in [first, second] {
            assert!(!prompt.contains(&left_out.id), "{why}: {prompt}");
        }
    }
    a.shutdown().await?;
    Ok(())
}

/// A clarify of the agent's with a default and a deadline that passes
/// with no reply from the asker wakes it once, to go on with the default;
/// one the asker answered in time does not (ADR 0018 §3).
#[tokio::test(flavor = "multi_thread")]
async fn a_clarify_past_its_deadline_wakes_the_agent_for_its_default() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let agent = a.author_key("agent")?;
    let clarify = |on: &wats::event::Event, deadline: u64| Draft {
        net: PUBLIC_NET.into(),
        thread: Some(on.thread_root().to_string()),
        parent: Some(on.id.clone()),
        kind: Some(Kind::Clarify),
        text: "I can write it, not compile it here.".into(),
        data: serde_json::json!({"options": ["untested code", "no thanks"],
                                 "default": "untested code", "deadline": deadline})
        .as_object()
        .unwrap()
        .clone(),
        ..Draft::default()
    };
    let soon = wats::node::now_secs() + 5;
    let silent = b.publish(ask("Build this Rust parser"), &[]).await?;
    let replied = b.publish(ask("Port this C code"), &[]).await?;
    for id in [&silent.id, &replied.id] {
        let a = Arc::clone(&a);
        let id = id.clone();
        eventually("ask on a", move || a.store().get(&id).unwrap().is_some()).await;
    }
    let lapsed = a.publish_as(&agent, clarify(&silent, soon), &[]).await?;
    let answered = a.publish_as(&agent, clarify(&replied, soon), &[]).await?;
    let choice = b
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(replied.id.clone()),
                parent: Some(answered.id.clone()),
                kind: Some(Kind::Answer),
                text: "No thanks.".into(),
                data: serde_json::json!({"choice": "no thanks"})
                    .as_object()
                    .unwrap()
                    .clone(),
                ..Draft::default()
            },
            &[],
        )
        .await?;

    let config = AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: vec![],
        every: Duration::from_secs(1),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(dir.path()),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.path().join("work"),
        data_dir: dir.path().to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    };
    {
        let a = Arc::clone(&a);
        let id = choice.id.clone();
        eventually("the choice on a", move || {
            a.store().get(&id).unwrap().is_some()
        })
        .await;
    }
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    // Nothing before the deadline (a second short of it, for clock rounding).
    while wats::node::now_secs() + 1 < soon {
        assert!(runs(dir.path()).is_empty(), "nothing before the deadline");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eventually("a run at the deadline", || !runs(dir.path()).is_empty()).await;
    let (prompt, _) = runs(dir.path()).remove(0);
    assert!(prompt.contains(&lapsed.id), "{prompt}");
    assert!(
        prompt.contains("1 clarify of yours got no answer by its deadline"),
        "{prompt}"
    );
    assert!(prompt.contains("by posting only"), "{prompt}");
    assert!(!prompt.contains(&answered.id), "answered in time: {prompt}");
    // Once.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(runs(dir.path()).len(), 1);
    let _ = stop.send(());
    runner.await??;
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// ADR 0018 §7: an answer or a clarify to an ask the human posted from
/// OpenClaw wakes OpenClaw's main session (`/hooks/wake`, mode now) with a
/// self-contained line; replies to other asks, and the asker's own posts in
/// the thread, do not.
#[tokio::test(flavor = "multi_thread")]
async fn replies_to_an_openclaw_ask_wake_its_main_session() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ok = r#"{"ok":true,"eventOutcome":"queued"}"#;
    let (url, seen) = gateway_stub(vec![(200, ok, Duration::ZERO); 4]);
    let data = dir.path().join("a");
    let token_file = wats::openclaw::hooks_token_path(&data);
    std::fs::create_dir_all(token_file.parent().unwrap())?;
    std::fs::write(&token_file, "s3cret-hook-token\n")?;
    let mut config = NodeConfig::new(&data);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.openclaw_notify = Some(wats::openclaw::Notify {
        url,
        token_file: token_file.clone(),
    });
    let a = Node::start(config).await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    // The human's ask, from OpenClaw (what the MCP `post` tool records).
    let human = a.author_key("human")?;
    let asked = a.publish_as(&human, ask("Draw a bee logo"), &[]).await?;
    a.store()
        .set_ask_notify(&asked.id, "openclaw", wats::node::now_secs())?;
    // Another ask, not from OpenClaw.
    let other = a.publish_as(&human, ask("Another question"), &[]).await?;
    let reply = |to: &wats::event::Event, kind: Kind, text: &str, data: serde_json::Value| Draft {
        net: PUBLIC_NET.into(),
        thread: Some(to.id.clone()),
        parent: Some(to.id.clone()),
        kind: Some(kind),
        text: text.into(),
        data: data.as_object().cloned().unwrap_or_default(),
        ..Draft::default()
    };
    for id in [&asked.id, &other.id] {
        let b = Arc::clone(&b);
        let id = id.clone();
        eventually("ask on b", move || b.store().get(&id).unwrap().is_some()).await;
    }
    let clarify = b
        .publish(
            reply(
                &asked,
                Kind::Clarify,
                "To the asker: I can't render a PNG in this run. Reply with your choice.",
                serde_json::json!({"options": ["SVG instead", "no thanks"], "default": "SVG instead"}),
            ),
            &[],
        )
        .await?;
    b.publish(
        reply(
            &other,
            Kind::Answer,
            "Not for OpenClaw.",
            serde_json::json!({}),
        ),
        &[],
    )
    .await?;
    // The asker's own reply in its thread: no wake.
    a.publish_as(
        &human,
        reply(&asked, Kind::Answer, "Never mind.", serde_json::json!({})),
        &[],
    )
    .await?;

    let requests = || seen.lock().unwrap().clone();
    eventually("a wake", || !requests().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let all = requests();
    assert_eq!(
        all.len(),
        1,
        "only the clarify to the OpenClaw ask: {all:?}"
    );
    let wake = &all[0];
    assert!(wake.line.starts_with("POST /hooks/wake "), "{}", wake.line);
    assert_eq!(
        wake.header("authorization"),
        Some("Bearer s3cret-hook-token")
    );
    assert_eq!(wake.body["mode"], "now");
    let text = wake.body["text"].as_str().unwrap();
    assert!(
        text.contains("Your swarm ask 'Draw a bee logo' got a clarify"),
        "{text}"
    );
    assert!(text.contains("Options: SVG instead / no thanks"), "{text}");
    assert!(text.contains(&clarify.id), "{text}");
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// ADR 0021 §2: two agents on one node. A post by one wakes the other,
/// never itself; the human's post wakes neither (`ignore_local`).
#[tokio::test(flavor = "multi_thread")]
async fn local_agents_wake_each_other_but_not_on_the_humans_posts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("a");
    let mut config = NodeConfig::new(&data);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.runners = vec![
        "claude as alice-coder".into(),
        "claude as alice-reviewer".into(),
    ];
    let a = Node::start(config).await?;
    let agent = |name: &str| {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        AgentConfig {
            runtime: "claude".into(),
            profile: format!("alice-{name}"),
            user: "alice".into(),
            about: String::new(),
            caps: vec![],
            every: Duration::from_millis(300),
            heartbeat: Duration::from_secs(3600),
            max_runs_per_hour: 100,
            allow: vec![],
            ignore_local: true,
            heartbeat_text: DEFAULT_HEARTBEAT.into(),
            command: fake_agent(&root),
            mcp_command: "/usr/bin/true".into(),
            mcp_args: vec![],
            extra_args: vec![],
            run_timeout: Duration::from_secs(10),
            max_turns: 10,
            budget: wats::budget::Budget::default(),
            wakes: wats::wake::WakeLimits::unlimited(),
            codex_disable: vec![],
            codex_enable: vec![],
            access: wats::access::Access::SwarmOnly,
            workdir: std::path::PathBuf::from("/nonexistent"),
            tools: vec![],
            servers: Vec::new(),
            model: None,
            work_dir: data.join("agent").join(name),
            data_dir: data.clone(),
            keep_runs: 20,
            keep_days: 30,
            openclaw: Default::default(),
            catchup: None,
        }
    };
    let mut stops = Vec::new();
    let mut runners = Vec::new();
    for name in ["coder", "reviewer"] {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        runners.push(tokio::spawn(wats::agent::run(
            Arc::clone(&a),
            agent(name),
            async {
                let _ = stopped.await;
            },
        )));
        stops.push(stop);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The human (the node's default profile) asks: nobody wakes.
    let human = a.publish(ask("A note to myself"), &[]).await?;
    // The coder agent asks: the reviewer wakes, the coder does not.
    let coder = a.author_key("alice-coder")?;
    let asked = a
        .publish_as(&coder, ask("Please review my diff"), &[])
        .await?;
    let reviewer_runs = || runs(&dir.path().join("reviewer"));
    eventually("the reviewer wakes", || !reviewer_runs().is_empty()).await;
    let (prompt, _) = reviewer_runs().remove(0);
    assert!(prompt.contains(&asked.id), "{prompt}");
    assert!(!prompt.contains(&human.id), "the human's post: {prompt}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        runs(&dir.path().join("coder")).is_empty(),
        "an agent never wakes on its own post"
    );

    for stop in stops {
        let _ = stop.send(());
    }
    for runner in runners {
        runner.await??;
    }
    a.shutdown().await?;
    Ok(())
}

/// A fake Claude Code that takes `secs` seconds and reports `tokens` LLM
/// tokens (half input, half output) and no dollar cost limit to hit.
fn metered_agent(dir: &Path, secs: &str, tokens: u64) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let out = dir.join("runs");
    std::fs::create_dir_all(&out).unwrap();
    let script = dir.join("fake-metered-claude");
    let (input, output) = (tokens / 2, tokens - tokens / 2);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = mcp ] && exit 0\nn=$(ls {out} | grep -c prompt)\ncat > {out}/run-$n.tmp\necho \"$@\" > {out}/run-$n.args\nmv {out}/run-$n.tmp {out}/run-$n.prompt\nsleep {secs}\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\"total_cost_usd\":0.001,\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}'\n",
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn budget_config(
    dir: &Path,
    command: std::path::PathBuf,
    budget: wats::budget::Budget,
) -> AgentConfig {
    AgentConfig {
        runtime: "claude".into(),
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
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(30),
        max_turns: 4,
        budget,
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        model: None,
        work_dir: dir.join("work"),
        data_dir: dir.to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The minutes budget (ADR 0034): the wall time of the runs. The day has
/// two minutes, 58.5 s of them used already: a run goes (more than a minute
/// is left), and no new run starts once less than a minute is left. A run
/// left by a node that stopped during it (a provisional entry) is charged
/// its timeout at the runner's start, marked unfinished. No dollar limit:
/// no `--max-budget-usd` either.
#[tokio::test(flavor = "multi_thread")]
async fn the_minutes_budget_stops_new_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    // A run the node never finished: charged its 58.5 s timeout.
    wats::budget::begin(
        &work,
        unix_now(),
        "20261022T000000Z-heartbeat",
        wats::budget::Usage {
            usd: None,
            secs: Some(58.5),
            tokens: None,
        },
    )?;
    let budget = wats::budget::Budget::of(&wats::settings::AgentSettings {
        max_usd_per_day: Some(0.0),
        max_minutes_per_day: Some(2),
        ..Default::default()
    });
    assert_eq!(budget.max_usd_per_day, None);
    let config = budget_config(dir.path(), metered_agent(dir.path(), "3", 10), budget);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("a run is indexed", || {
        wats::runs::list(&work).iter().any(|r| r.status == "ok")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let all = runs(dir.path());
    assert_eq!(all.len(), 1, "paused once the minute is used");
    assert!(!all[0].1.contains("--max-budget-usd"), "{}", all[0].1);
    let record = wats::runs::list(&work)
        .into_iter()
        .find(|r| r.status == "ok")
        .unwrap();
    assert!(record.seconds.unwrap_or(0.0) >= 2.9, "{record:?}");
    let ledger = std::fs::read_to_string(wats::budget::ledger_path(&work))?;
    assert!(ledger.contains("\"unfinished\":true"), "{ledger}");
    assert!(!ledger.contains("\"provisional\""), "{ledger}");
    let spend = wats::budget::spend(&work, unix_now());
    assert!(spend.secs_today >= 61.4, "{spend:?}");
    assert_eq!(spend.runs_today, 2, "{spend:?}");
    match wats::budget::check(&budget, &spend, unix_now()) {
        wats::budget::Verdict::Paused { why, .. } => {
            assert_eq!(why, "1 min of agent runs today, budget 2 min")
        }
        go => panic!("{go:?}"),
    }
    let _ = stop.send(());
    runner.await??;
    a.shutdown().await?;
    Ok(())
}

/// The LLM token budget (ADR 0034), for a runtime that reports them: each
/// fake run reports 1100 tokens, so a day of 2000 takes two runs, then
/// none; `status` shows the tokens and minutes used today.
#[tokio::test(flavor = "multi_thread")]
async fn the_llm_token_budget_stops_new_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    let work = dir.path().join("work");
    let budget = wats::budget::Budget::of(&wats::settings::AgentSettings {
        max_tokens_per_day: Some(2000),
        ..Default::default()
    });
    let config = budget_config(dir.path(), metered_agent(dir.path(), "0", 1100), budget);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("two runs", || {
        wats::runs::list(&work)
            .iter()
            .filter(|r| r.status == "ok")
            .count()
            >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        runs(dir.path()).len(),
        2,
        "paused after 2200 of 2000 tokens"
    );
    let spend = wats::budget::spend(&work, unix_now());
    assert_eq!(spend.tokens_today, 2200);
    let info = wats::agent::AgentInfo {
        name: "claude".into(),
        runtime: Some("claude".into()),
        profile: "agent".into(),
        enabled: true,
        running: true,
        work_dir: work.clone(),
        budget,
    };
    let status = info.status(unix_now());
    assert_eq!(status["usage"]["today"]["tokens"], 2200, "{status}");
    assert_eq!(
        status["usage"]["limits"]["tokens_per_day"], 2000,
        "{status}"
    );
    assert_eq!(
        status["usage"]["limits"]["tokens_per_month"], 60000,
        "{status}"
    );
    assert!(status["usage"]["today"]["minutes"].is_number(), "{status}");
    let line = status["budget"].as_str().unwrap();
    assert!(
        line.contains("2.2k of 2k LLM tokens today, 2.2k of 60k this month"),
        "{line}"
    );
    // OpenClaw reports none: its budget has no token limit, and says so.
    let openclaw = wats::agent::AgentInfo {
        runtime: Some("openclaw".into()),
        budget: budget.for_runtime("openclaw"),
        ..info
    };
    let status = openclaw.status(unix_now());
    assert_eq!(status["usage"]["today"]["tokens"], serde_json::Value::Null);
    assert!(
        !status["budget"].as_str().unwrap().contains("tokens"),
        "{status}"
    );
    let _ = stop.send(());
    runner.await??;
    a.shutdown().await?;
    Ok(())
}

/// ADR 0036: a runner session's posts declare the run's runtime and
/// model, with the agent's effort; `search` lines and `read` show it as
/// self-declared; `node.declare_via = false` removes it, the agent's too.
#[tokio::test(flavor = "multi_thread")]
async fn posts_declare_what_produced_them() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let call = |name: &'static str, args: serde_json::Value| {
        rmcp::model::CallToolRequestParams::new(name)
            .with_arguments(args.as_object().unwrap().clone())
    };
    let serve = |server: wats::mcp::WatsMcp| async move {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await
    };
    let dir = tempfile::tempdir()?;
    for declare in [true, false] {
        let mut config = NodeConfig::new(dir.path().join(format!("n{declare}")));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(0);
        config.min_pow_bits = Some(0);
        config.declare_via = declare;
        let a = Node::start(config).await?;
        let run = wats::node::RunnerRun {
            id: "20261023T000000Z-events".into(),
            via: wats::via::Via::of_run("claude", Some("opus")),
            ..Default::default()
        };
        let agent =
            serve(wats::mcp::WatsMcp::new(Arc::clone(&a), vec![]).runner_session(run)).await?;
        let posted = agent
            .call_tool(call(
                "post",
                serde_json::json!({"title": "Parquet tips", "text": "Use row groups.",
                    "kind": "note", "data": {"via": {"runtime": "codex", "model": "gpt-5",
                    "effort": "high", "cost": "1"}}}),
            ))
            .await?
            .structured_content
            .unwrap();
        let id = posted["id"].as_str().unwrap().to_string();
        let stored = a.store().get(&id)?.unwrap();
        let note = posted["note"].as_str().unwrap_or_default();
        if declare {
            assert_eq!(
                stored.event.data["via"],
                serde_json::json!({"runtime": "claude-code", "model": "opus",
                    "effort": "high", "wats": env!("CARGO_PKG_VERSION")})
            );
            assert!(note.contains("cost"), "{posted}");
            let found = agent
                .call_tool(call("search", serde_json::json!({"text": "Parquet"})))
                .await?
                .structured_content
                .unwrap();
            assert_eq!(
                found["posts"][0]["via"], "claude-code/opus (high)",
                "{found}"
            );
            let read = agent
                .call_tool(call("read", serde_json::json!({"id": id})))
                .await?
                .structured_content
                .unwrap();
            assert_eq!(read["post"]["via_note"], wats::via::UNVERIFIED, "{read}");
        } else {
            assert!(
                stored.event.data.get("via").is_none(),
                "{:?}",
                stored.event.data
            );
            assert!(note.contains("declare_via"), "{posted}");
        }
        // An interactive session: the MCP client's name is its runtime.
        let human = serve(wats::mcp::WatsMcp::new(Arc::clone(&a), vec![])).await?;
        let posted = human
            .call_tool(call(
                "post",
                serde_json::json!({"title": "Hello", "text": "Hi.", "kind": "note"}),
            ))
            .await?
            .structured_content
            .unwrap();
        let stored = a.store().get(posted["id"].as_str().unwrap())?.unwrap();
        match declare {
            true => assert!(stored.event.data["via"]["runtime"].is_string(), "{posted}"),
            false => assert!(stored.event.data.get("via").is_none()),
        }
        agent.cancel().await?;
        human.cancel().await?;
        a.shutdown().await?;
    }
    Ok(())
}
