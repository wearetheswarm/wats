//! OpenCode as an agent runtime (ADR 0031), against fake `opencode`
//! scripts only: never the real CLI. The runner's command line, per-run
//! config and environment for each access level, its JSON events, the
//! budget from `step_finish` costs, the process-group kill at the timeout,
//! the version check and the refused arguments; then `wats setup opencode`
//! with a throwaway HOME and XDG directories.

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

/// A fake `opencode`: `--version` prints `version`; a run records its
/// arguments, working directory, per-run config, OpenCode variables and
/// prompt in `out`, then runs `body` (a shell snippet).
fn fake(dir: &Path, version: &str, body: &str) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = dir.join("opencode");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "{version}"; exit 0; fi
echo "$@" > {out}/args
pwd > {out}/cwd
printf '%s' "$OPENCODE_CONFIG_CONTENT" > {out}/config.json
env | grep -E '^(XDG_CONFIG_HOME|OPENCODE_[A-Z_]*)=' | grep -v '^OPENCODE_CONFIG_CONTENT=' | sort > {out}/env
cat > {out}/prompt
{body}
"#,
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The events of one good run: a wats tool call, text, one step at
/// $0.0125 (OpenCode's estimate), and a rejected permission on stderr.
const GOOD_RUN: &str = r#"echo '{"type":"step_start","timestamp":1,"sessionID":"ses_1","part":{"type":"step-start"}}'
echo '{"type":"tool_use","timestamp":2,"sessionID":"ses_1","part":{"type":"tool","tool":"wats_who","state":{"status":"completed","input":{},"output":"{\"agents\":[]}"}}}'
echo '{"type":"text","timestamp":3,"sessionID":"ses_1","part":{"type":"text","text":"Nothing to do."}}'
echo '{"type":"step_finish","timestamp":4,"sessionID":"ses_1","part":{"type":"step-finish","reason":"stop","cost":0.0125,"tokens":{"input":10,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}}}'
echo 'permission requested: bash (ls); auto-rejecting' >&2"#;

fn config(dir: &Path, command: PathBuf, access: Access) -> AgentConfig {
    let work = dir.join("data/agent/opencode");
    AgentConfig {
        runtime: "opencode".into(),
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
        extra_args: vec!["--variant=low".into()],
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
        model: Some("azure/gpt-test".into()),
        work_dir: work,
        data_dir: dir.join("data"),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    }
}

/// Run the agent until a run is indexed with `status`; returns its record.
async fn one_run(dir: &Path, config: AgentConfig, status: &str) -> wats::runs::RunRecord {
    let a = node(&dir.join("node")).await.unwrap();
    let work = config.work_dir.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually(&format!("an OpenCode run is indexed as {status}"), || {
        wats::runs::list(&work).iter().any(|r| r.status == status)
    })
    .await;
    let _ = stop.send(());
    runner.await.unwrap().unwrap();
    a.shutdown().await.unwrap();
    wats::runs::list(&work)
        .into_iter()
        .find(|r| r.status == status)
        .unwrap()
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join("out").join(name)).unwrap_or_default()
}

/// The run's working directory, canonical (macOS: /var is /private/var).
fn cwd(dir: &Path) -> PathBuf {
    PathBuf::from(read(dir, "cwd").trim_end())
        .canonicalize()
        .unwrap()
}

/// A `swarm-only` run: `opencode run --format json --agent wats`, the
/// prompt on stdin, the per-run config in OPENCODE_CONFIG_CONTENT (every
/// tool denied, `wats_*` allowed last, the same rules at the top level,
/// `steps` from max_turns, the run's runner token), the user's config
/// isolated, and the cost of its `step_finish` events in the budget.
#[tokio::test(flavor = "multi_thread")]
async fn swarm_only_run_is_locked_down() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path(), "1.19.3", GOOD_RUN);
    let config = config(dir.path(), fake, Access::SwarmOnly);
    let work = config.work_dir.clone();
    let record = one_run(dir.path(), config, "ok").await;

    let args = read(dir.path(), "args");
    assert_eq!(
        args.trim(),
        "run --format json --agent wats --model=azure/gpt-test --variant=low",
        "{args}"
    );
    assert_eq!(cwd(dir.path()), work.join("locked").canonicalize().unwrap());
    assert!(read(dir.path(), "prompt").contains("WeAreTheSwarm"));
    let env = read(dir.path(), "env");
    for line in [
        format!("XDG_CONFIG_HOME={}", work.join("opencode-config").display()),
        "OPENCODE_DB=:memory:".into(),
        "OPENCODE_DISABLE_PROJECT_CONFIG=1".into(),
        "OPENCODE_DISABLE_CLAUDE_CODE=1".into(),
        "OPENCODE_DISABLE_AUTOUPDATE=1".into(),
    ] {
        assert!(env.lines().any(|l| l == line), "missing {line} in {env}");
    }
    let run_config: Value = serde_json::from_str(&read(dir.path(), "config.json")).unwrap();
    assert_eq!(run_config["default_agent"], "wats");
    assert_eq!(run_config["autoupdate"], false);
    let agent = &run_config["agent"]["wats"];
    assert_eq!(agent["steps"], 7);
    assert_eq!(agent["permission"]["*"], "deny");
    assert_eq!(agent["permission"]["bash"], "deny");
    assert_eq!(agent["permission"]["task"], "deny");
    assert_eq!(agent["permission"]["wats_*"], "allow");
    assert_eq!(run_config["permission"], agent["permission"]);
    let raw = read(dir.path(), "config.json");
    let permission = raw.split("\"permission\":").nth(1).unwrap();
    assert!(
        permission.find("\"*\"") < permission.find("\"wats_*\""),
        "wats_* comes after *: {permission}"
    );
    let server = &run_config["mcp"]["wats"];
    assert_eq!(
        server["command"],
        serde_json::json!(["/opt/wats", "--dir", "/d", "mcp", "--profile", "agent"])
    );
    assert_eq!(server["timeout"], 30_000);
    assert!(
        server["environment"]["WATS_RUNNER_TOKEN"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "{server}"
    );
    // The agent's own OpenCode directory, with its stub.
    assert!(
        work.join("opencode-config/opencode/opencode.jsonc")
            .exists(),
        "agent dir stub"
    );

    assert_eq!(record.turns, Some(1));
    assert_eq!(record.cost_usd, Some(0.0125));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let spend = wats::budget::spend(&work, now);
    assert!(
        (spend.usd_today - 0.0125).abs() < 1e-9,
        "{}",
        spend.usd_today
    );
    let mut rendered = Vec::new();
    wats::runs::print_runs(
        &work,
        std::slice::from_ref(&record.id),
        wats::runs::Style { color: false },
        &mut rendered,
    )
    .unwrap();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains("→ wats_who {}"), "{rendered}");
    assert!(rendered.contains("  Nothing to do."), "{rendered}");
    assert!(rendered.contains("auto-rejecting"), "{rendered}");
}

/// `workspace`: the listed tools are allowed (bash with the deny patterns
/// after its allows), the workspace's MCP servers too, and project config
/// stays on; `task` is refused before any run.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_run_opens_what_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path(), "1.19.3", GOOD_RUN);
    let mut config = config(dir.path(), fake.clone(), Access::Workspace);
    config.tools = vec!["read".into(), "bash(git status*)".into()];
    std::fs::create_dir_all(&config.workdir).unwrap();
    std::fs::write(
        config.workdir.join("opencode.jsonc"),
        "{\n  // the docs server\n  \"mcp\": {\"docs\": {\"type\": \"local\", \"command\": [\"docs-mcp\"]}},\n}\n",
    )
    .unwrap();
    let workdir = config.workdir.clone();
    let data_dir = config.data_dir.clone();
    one_run(dir.path(), config.clone(), "ok").await;

    assert_eq!(cwd(dir.path()), workdir.canonicalize().unwrap());
    let env = read(dir.path(), "env");
    assert!(env.contains("XDG_CONFIG_HOME="), "{env}");
    assert!(!env.contains("OPENCODE_DISABLE_PROJECT_CONFIG"), "{env}");
    let run_config: Value = serde_json::from_str(&read(dir.path(), "config.json")).unwrap();
    let permission = &run_config["agent"]["wats"]["permission"];
    assert_eq!(permission["read"], "allow");
    assert_eq!(permission["grep"], "deny");
    assert_eq!(permission["docs_*"], "allow");
    assert_eq!(permission["bash"]["git status*"], "allow");
    assert_eq!(
        permission["bash"][format!("*{}*", data_dir.display())],
        "deny",
        "{permission}"
    );
    assert_eq!(permission["bash"]["*..*"], "deny");

    // `task` would hand the run to a subagent with OpenCode's own rules.
    let mut refused = config;
    refused.tools = vec!["task".into()];
    let a = node(&dir.path().join("node2")).await.unwrap();
    let e = wats::agent::run(Arc::clone(&a), refused, async {})
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("task"), "{e:#}");
    a.shutdown().await.unwrap();
}

/// A failed run (an `error` event, exit 1) is `error` with OpenCode's
/// message; a run that reports a cost of 0 counts as a run without cost.
#[tokio::test(flavor = "multi_thread")]
async fn errors_and_runs_without_a_price() {
    let dir = tempfile::tempdir().unwrap();
    let failing = fake(
        dir.path(),
        "1.19.3",
        r#"echo 'not json'
echo '{"type":"error","timestamp":1,"sessionID":"ses_1","error":{"name":"ProviderAuthError","data":{"message":"no credentials for azure"}}}'
exit 1"#,
    );
    let record = one_run(
        dir.path(),
        config(dir.path(), failing, Access::SwarmOnly),
        "error",
    )
    .await;
    assert_eq!(record.exit, Some(1));
    assert_eq!(record.error.as_deref(), Some("no credentials for azure"));

    let dir = tempfile::tempdir().unwrap();
    let unpriced = fake(
        dir.path(),
        "1.19.3",
        r#"echo '{"type":"step_finish","timestamp":4,"sessionID":"ses_1","part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"input":10,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}}}'"#,
    );
    let config = config(dir.path(), unpriced, Access::SwarmOnly);
    let work = config.work_dir.clone();
    let record = one_run(dir.path(), config, "ok").await;
    assert_eq!(record.cost_usd, None);
    assert_eq!(record.turns, Some(1));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(wats::budget::spend(&work, now).uncosted_today >= 1);
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// At the timeout the runner kills the run's whole process group: what
/// OpenCode started (here a `sleep`) goes too.
#[tokio::test(flavor = "multi_thread")]
async fn the_timeout_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let fake = fake(
        dir.path(),
        "1.19.3",
        &format!(
            "sleep 300 &\necho $! > {}/grandchild\nsleep 300",
            out.display()
        ),
    );
    let mut config = config(dir.path(), fake, Access::SwarmOnly);
    // Room for the fake CLI to start on a loaded CI runner.
    config.run_timeout = Duration::from_secs(5);
    one_run(dir.path(), config, "timeout").await;
    let pid = read(dir.path(), "grandchild").trim().to_string();
    assert!(!pid.is_empty());
    eventually("the grandchild is gone", || !alive(&pid)).await;
}

/// Below OpenCode 1.17.0, or with arguments that undo the access level,
/// the runner does not start.
#[tokio::test(flavor = "multi_thread")]
async fn old_versions_and_undoing_arguments_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir.path().join("node")).await.unwrap();
    let old = fake(dir.path(), "1.16.2", "exit 0");
    let e = wats::agent::run(
        Arc::clone(&a),
        config(dir.path(), old, Access::SwarmOnly),
        async {},
    )
    .await
    .unwrap_err();
    assert!(format!("{e:#}").contains("1.16.2"), "{e:#}");

    let current = fake(dir.path(), "1.19.3", "exit 0");
    let mut config = config(dir.path(), current, Access::SwarmOnly);
    config.extra_args = vec!["--agent".into(), "build".into()];
    let e = wats::agent::run(Arc::clone(&a), config, async {})
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("--agent"), "{e:#}");
    assert!(read(dir.path(), "args").is_empty(), "no run started");
    a.shutdown().await.unwrap();
}

/// A throwaway machine for `wats setup opencode`: HOME, XDG directories,
/// run directory, and a fake `opencode` that logs its calls.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new(version: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["home", "xdg-config", "xdg-data", "run", "bin"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        let fake = tmp.path().join("bin/opencode");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo {version}; exit 0; fi\necho \"$@\" >> {}\n",
                tmp.path().join("calls.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        Machine { tmp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn setup(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("WATS_RUN_DIR", self.path("run"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("XDG_DATA_HOME", self.path("xdg-data"))
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_OPENCODE")
            .env_remove("WATS_DIR")
            .args(["--infra", "none", "--no-default-bootstrap", "--dir"])
            .arg(self.path("data"))
            .args(["setup", "opencode", "--opencode"])
            .arg(self.path("bin/opencode"))
            .args(args)
            .output()
            .unwrap()
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.path("calls.log")).unwrap_or_default()
    }

    /// Files under `dir`, recursively.
    fn files(&self, dir: &str) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.path(dir), &mut out);
        out
    }
}

/// `wats setup opencode` registers wats through `opencode mcp add` (OpenCode
/// writes its own file; wats writes nothing in HOME or the XDG
/// directories), enables the agent and creates its OpenCode directory.
#[test]
fn setup_registers_through_opencode() {
    let m = Machine::new("1.19.3");
    let out = m.setup(&[]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = m.calls();
    let add = calls.lines().find(|l| l.starts_with("mcp add")).unwrap();
    assert!(add.starts_with("mcp add wats -- /"), "{add}");
    assert!(
        add.ends_with(&format!(
            "--dir {} mcp --profile alice",
            m.path("data").display()
        )),
        "{add}"
    );
    assert!(
        said.contains("OpenCode: wats MCP server registered"),
        "{said}"
    );
    assert!(
        said.contains(
            &m.path("xdg-config/opencode/opencode.json")
                .display()
                .to_string()
        ),
        "{said}"
    );
    let config = std::fs::read_to_string(m.path("data/config.toml")).unwrap();
    assert!(config.contains("runtime = \"opencode\""), "{config}");
    assert!(
        m.path("data/agent/opencode/opencode-config/opencode/opencode.jsonc")
            .exists(),
        "{said}"
    );
    for dir in ["home", "xdg-config", "xdg-data"] {
        assert!(m.files(dir).is_empty(), "{dir}: {:?}", m.files(dir));
    }
    // Again: the same command, nothing else.
    assert!(m.setup(&["--mcp"]).status.success());
    let adds: Vec<_> = m
        .calls()
        .lines()
        .filter(|l| l.starts_with("mcp add"))
        .map(String::from)
        .collect();
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[0], adds[1]);
}

/// An OpenCode without the non-interactive `mcp add` gets the block to
/// paste and no write; `--remove` says what to delete and changes nothing.
#[test]
fn old_opencode_gets_the_block_and_remove_only_prints() {
    let m = Machine::new("1.16.2");
    let out = m.setup(&["--mcp"]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("\"mcp\""), "{said}");
    assert!(said.contains("\"type\": \"local\""), "{said}");
    assert!(!m.calls().contains("mcp add"), "{}", m.calls());

    let out = m.setup(&["--remove"]);
    assert!(out.status.success());
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("no `mcp remove`"), "{said}");
    assert!(
        said.contains(
            &m.path("xdg-config/opencode/opencode.json")
                .display()
                .to_string()
        ),
        "{said}"
    );
    assert!(m.calls().is_empty(), "{}", m.calls());
    assert!(!m.path("data/config.toml").exists());
    for dir in ["home", "xdg-config", "xdg-data"] {
        assert!(m.files(dir).is_empty(), "{dir}");
    }
}

/// A config that would change the run's agent, added while the runner
/// runs, fails the next run before OpenCode starts (the check runs before
/// every run), with the reason in the run index; the run's token ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_added_between_runs_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path(), "1.19.3", GOOD_RUN);
    let config = config(dir.path(), fake, Access::Workspace);
    let workdir = config.workdir.clone();
    let work = config.work_dir.clone();
    let a = node(&dir.path().join("node")).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually("a first run", || {
        wats::runs::list(&work).iter().any(|r| r.status == "ok")
    })
    .await;
    let agent = workdir.join(".opencode/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("wats.md"), "---\npermission: allow\n---\n").unwrap();
    eventually("a refused run", || {
        wats::runs::list(&work).iter().any(|r| r.status == "failed")
    })
    .await;
    let _ = stop.send(());
    runner.await.unwrap().unwrap();
    let failed = wats::runs::list(&work)
        .into_iter()
        .find(|r| r.status == "failed")
        .unwrap();
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|e| e.contains("wats.md")),
        "{failed:?}"
    );
    a.shutdown().await.unwrap();
}

/// SIGHUP to a foreground `wats` (its terminal closed): the node stops as
/// on Ctrl-C, and the agent run in progress, in its own process group, is
/// killed with what it started.
#[test]
fn sighup_stops_the_node_and_its_runs_group() {
    let dir = tempfile::tempdir().unwrap();
    for d in ["home", "xdg-config", "xdg-data", "run"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let out = dir.path().join("out");
    let fake = fake(
        dir.path(),
        "1.19.3",
        &format!(
            "sleep 300 &\necho $! > {}/grandchild\nsleep 300",
            out.display()
        ),
    );
    let mut node = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("WATS_RUN_DIR", dir.path().join("run"))
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
        .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
        .env("USER", "alice")
        .env_remove("WATS_PROFILE")
        .env_remove("WATS_DIR")
        .args([
            "--infra",
            "none",
            "--no-default-bootstrap",
            "--loopback",
            "--dir",
        ])
        .arg(dir.path().join("data"))
        .args([
            "--agent",
            "opencode",
            "--access",
            "swarm-only",
            "--heartbeat",
            "1s",
            "--run-timeout",
            "10m",
            "--agent-command",
        ])
        .arg(&fake)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let pid = loop {
        let pid = read(dir.path(), "grandchild").trim().to_string();
        if !pid.is_empty() {
            break pid;
        }
        if let Some(status) = node.try_wait().unwrap() {
            panic!("the node exited: {status}: {:?}", node.wait_with_output());
        }
        assert!(std::time::Instant::now() < deadline, "no run after 60 s");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(alive(&pid));
    let hup = std::process::Command::new("kill")
        .args(["-HUP", &node.id().to_string()])
        .status()
        .unwrap();
    assert!(hup.success());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = node.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the node did not stop on SIGHUP"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let stderr = {
        use std::io::Read;
        let mut s = String::new();
        node.stderr.take().unwrap().read_to_string(&mut s).unwrap();
        s
    };
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("SIGHUP"), "{stderr}");
    let gone = std::time::Instant::now() + Duration::from_secs(20);
    while alive(&pid) {
        assert!(
            std::time::Instant::now() < gone,
            "the grandchild outlived the node"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A detached node (`wats --detach`) ignores SIGHUP, as it always did:
/// only a foreground node stops on it.
#[test]
fn a_detached_node_survives_sighup() {
    let dir = tempfile::tempdir().unwrap();
    for d in ["home", "xdg-config", "xdg-data", "run"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let fake = fake(dir.path(), "1.19.3", "sleep 300");
    let data = dir.path().join("data");
    let wats = |args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("WATS_RUN_DIR", dir.path().join("run"))
            .env("HOME", dir.path().join("home"))
            .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
            .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_DIR")
            .args(["--infra", "none", "--no-default-bootstrap", "--dir"])
            .arg(&data)
            .args(args)
            .stdin(std::process::Stdio::null());
        command
    };
    let out = wats(&[
        "--loopback",
        "--agent",
        "opencode",
        "--access",
        "swarm-only",
        "--heartbeat",
        "1h",
        "--agent-command",
        fake.to_str().unwrap(),
        "--detach",
    ])
    .output()
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    let ps = wats(&["ps", "--json"]).output().unwrap();
    assert!(ps.status.success(), "{ps:?}");
    let entries: Value = serde_json::from_slice(&ps.stdout).unwrap();
    let real = std::fs::canonicalize(&data).unwrap();
    let pid = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["data_dir"]
                .as_str()
                .is_some_and(|d| Path::new(d) == data || Path::new(d) == real)
        })
        .and_then(|e| e["pid"].as_u64())
        .unwrap_or_else(|| panic!("no detached node in {entries}"))
        .to_string();
    assert!(alive(&pid));
    let hup = std::process::Command::new("kill")
        .args(["-HUP", &pid])
        .status()
        .unwrap();
    assert!(hup.success());
    std::thread::sleep(Duration::from_secs(3));
    let still = alive(&pid);
    let stop = wats(&["stop", "--timeout", "10s"]).output().unwrap();
    assert!(still, "the detached node stopped on SIGHUP");
    assert!(stop.status.success(), "{stop:?}");
}

/// `nohup wats`: a foreground node started with SIGHUP ignored keeps it
/// ignored (read before tokio registers any handler), and survives
/// `kill -HUP`.
#[test]
fn a_nohup_foreground_node_survives_sighup() {
    let dir = tempfile::tempdir().unwrap();
    for d in ["home", "xdg-config", "xdg-data", "run"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let fake = fake(dir.path(), "1.19.3", "sleep 300");
    let data = dir.path().join("data");
    let nohup = ["/usr/bin/nohup", "/bin/nohup"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("nohup(1)");
    let mut node = std::process::Command::new(nohup)
        .arg(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("WATS_RUN_DIR", dir.path().join("run"))
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
        .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
        .env("USER", "alice")
        .env_remove("WATS_PROFILE")
        .env_remove("WATS_DIR")
        .args([
            "--infra",
            "none",
            "--no-default-bootstrap",
            "--loopback",
            "--dir",
        ])
        .arg(&data)
        .args([
            "--agent",
            "opencode",
            "--access",
            "swarm-only",
            "--heartbeat",
            "1h",
            "--agent-command",
        ])
        .arg(&fake)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !wats::attach::socket_path(&data).exists() {
        if let Some(status) = node.try_wait().unwrap() {
            panic!("the node exited: {status}: {:?}", node.wait_with_output());
        }
        assert!(std::time::Instant::now() < deadline, "the node is not up");
        std::thread::sleep(Duration::from_millis(100));
    }
    // nohup execs wats: the same process.
    let pid = node.id().to_string();
    let hup = std::process::Command::new("kill")
        .args(["-HUP", &pid])
        .status()
        .unwrap();
    assert!(hup.success());
    std::thread::sleep(Duration::from_secs(3));
    let survived = node.try_wait().unwrap().is_none();
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status();
    let out = node.wait_with_output().unwrap();
    assert!(
        survived,
        "nohup wats stopped on SIGHUP: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
