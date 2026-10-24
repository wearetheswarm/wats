//! Mistral Vibe as an agent runtime (ADR 0033), against fake `vibe`
//! scripts only: never the real CLI. The runner's command line, per-run
//! config and environment at `swarm-only` and `inherit`, the streaming
//! output and run export, the budget from the export's cost, the exit
//! codes, the process-group kill at the timeout, the version check and the
//! refusals (`workspace`, arguments that undo the level); then `wats setup
//! vibe --agent` and `wats --agent vibe`, with a throwaway HOME, XDG
//! directories and VIBE_HOME.

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

/// A fake `vibe`: `--version` prints `vibe <version>`; a run records its
/// arguments, working directory, Vibe variables, per-run config (when
/// VIBE_HOME is set) and prompt in `out`, finds its `--output-dir` in
/// `$dir`, then runs `body` (a shell snippet).
fn fake(dir: &Path, version: &str, body: &str) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = dir.join("vibe");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "vibe {version}"; exit 0; fi
echo "$@" > {out}/args
pwd > {out}/cwd
env | grep -E '^(VIBE_[A-Z_]*|MISTRAL_VIBE_SESSION_ROOT)=' | grep -vE '^VIBE_(MCP_SERVERS|TOOLS)=' | sort > {out}/env
printf '%s' "$VIBE_MCP_SERVERS" > {out}/servers.json
printf '%s' "$VIBE_TOOLS" > {out}/tools.json
if [ -n "$VIBE_HOME" ]; then cp "$VIBE_HOME/config.toml" {out}/config.toml; fi
cat > {out}/prompt
dir=
for a in "$@"; do case "$a" in --output-dir=*) dir="${{a#--output-dir=}}";; esac; done
{body}
"#,
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// One good run: an assistant message and a wats tool call on stdout
/// (`--output streaming`), the keychain notice on stderr, and the run
/// export with its cost (Vibe's estimate) and steps, plus a session copy.
const GOOD_RUN: &str = r#"echo '{"type":"message","id":"e1","sessionId":"s1","createdAt":1,"updatedAt":1,"generationStatus":"completed","role":"assistant","content":[{"type":"text","text":"Nothing to do."}]}'
echo '{"type":"effect","id":"e2","sessionId":"s1","createdAt":2,"updatedAt":2,"generationStatus":"completed","title":"wats_who","state":{"status":"completed","outputText":"{\"agents\":[]}"}}'
echo 'Reading API keys from the macOS Keychain (set them as environment variables to skip this)' >&2
mkdir -p "$dir/session"
echo '{}' > "$dir/session/journal.jsonl"
cat > "$dir/export.json" <<'EXPORT'
{"outcome":"finished","stop_reason":null,"error":null,"session_id":"s1","usage":{"input_tokens":1000,"output_tokens":20,"cached_input_tokens":0,"total_tokens":1020},"cost_usd":0.0125,"steps":2,"config":{"active_model":"devstral-2"},"schema_version":1,"vibe_version":"3.0.1","journal_dir":"session","started_at":"x","ended_at":"y","limits":{"max_turns":7},"exit_code":0}
EXPORT"#;

fn config(dir: &Path, command: PathBuf, access: Access) -> AgentConfig {
    let work = dir.join("data/agent/vibe");
    AgentConfig {
        runtime: "vibe".into(),
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
        extra_args: vec!["--max-tokens=50000".into()],
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
        model: Some("devstral-2".into()),
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

/// Run the agent until a run is indexed with `status`; returns its record.
async fn one_run(dir: &Path, config: AgentConfig, status: &str) -> wats::runs::RunRecord {
    let a = node(&dir.join("node")).await.unwrap();
    let work = config.work_dir.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(Arc::clone(&a), config, async {
        let _ = stopped.await;
    }));
    eventually(&format!("a Vibe run is indexed as {status}"), || {
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A `swarm-only` run: headless with the prompt on stdin, NDJSON output,
/// the run export, the turn, price and time limits, the exact wats tools
/// (`--enabled-tools`, once per tool), in
/// an empty directory, with the agent's own Vibe home whose per-run config
/// holds the wats server only (the run's runner token, the wats tools
/// approved, every built-in tool disabled); its cost and steps come from
/// the export, which is then removed.
#[tokio::test(flavor = "multi_thread")]
async fn swarm_only_run_is_locked_down() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path(), "3.0.1", GOOD_RUN);
    let config = config(dir.path(), fake, Access::SwarmOnly);
    let work = config.work_dir.clone();
    let record = one_run(dir.path(), config, "ok").await;

    let args = read(dir.path(), "args");
    let tools: Vec<String> = wats::vibe::run_tool_names()
        .iter()
        .map(|t| format!("--enabled-tools={t}"))
        .collect();
    assert_eq!(
        args.trim(),
        format!(
            "--prompt= --output=streaming --output-dir={} --max-turns=7 --max-price=2.00 \
             --time-limit=9 {} --max-tokens=50000",
            work.join("vibe-run").display(),
            tools.join(" ")
        ),
        "{args}"
    );
    assert!(!args.contains('*'), "{args}");
    assert_eq!(cwd(dir.path()), work.join("locked").canonicalize().unwrap());
    assert!(read(dir.path(), "prompt").contains("WeAreTheSwarm"));
    let env = read(dir.path(), "env");
    let home = work.join("vibe-home");
    for line in [
        format!("VIBE_HOME={}", home.display()),
        format!(
            "MISTRAL_VIBE_SESSION_ROOT={}",
            home.join("logs/session").display()
        ),
        "VIBE_SESSION_LOGGING__ENABLED=false".into(),
        "VIBE_ENABLE_UPDATE_CHECKS=false".into(),
        "VIBE_ACTIVE_MODEL=devstral-2".into(),
    ] {
        assert!(env.lines().any(|l| l == line), "missing {line} in {env}");
    }
    assert!(read(dir.path(), "servers.json").is_empty());
    let run_config: toml::Value = toml::from_str(&read(dir.path(), "config.toml")).unwrap();
    assert_eq!(
        run_config["enabled_tools"],
        toml::Value::Array(
            wats::vibe::run_tool_names()
                .into_iter()
                .map(toml::Value::from)
                .collect()
        )
    );
    assert_eq!(
        run_config["disabled_skills"],
        toml::Value::Array(vec!["*".into()])
    );
    assert!(
        run_config["disabled_tools"]
            .as_array()
            .unwrap()
            .contains(&"bash".into())
    );
    assert_eq!(
        run_config["tools"]["wats_post"]["permission"].as_str(),
        Some("always")
    );
    let servers = run_config["mcp_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["name"].as_str(), Some("wats"));
    assert_eq!(servers[0]["command"].as_str(), Some("/opt/wats"));
    assert!(
        servers[0]["env"]["WATS_RUNNER_TOKEN"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "{run_config}"
    );
    // The file stays owner-only; the export directory is gone.
    let mode = std::fs::metadata(home.join("config.toml"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!work.join("vibe-run").exists());

    assert_eq!(record.turns, Some(2));
    assert_eq!(record.cost_usd, Some(0.0125));
    assert_eq!(record.model, "devstral-2");
    assert_eq!(record.access, "swarm-only");
    let spend = wats::budget::spend(&work, now());
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
    assert!(rendered.contains("→ wats_who"), "{rendered}");
    assert!(rendered.contains("  Nothing to do."), "{rendered}");
    assert!(rendered.contains("Keychain"), "{rendered}");
    assert!(
        rendered.contains("✓ done · 2 steps · $0.0125 (Vibe's estimate)"),
        "{rendered}"
    );
}

/// `inherit`: the user's own Vibe (no VIBE_HOME of the runner's, no
/// `--enabled-tools`), in the directory `wats` started in, with the wats
/// server and the wats tools' approvals added through VIBE_MCP_SERVERS
/// and VIBE_TOOLS; still no session log.
#[tokio::test(flavor = "multi_thread")]
async fn inherit_adds_the_wats_server_to_the_users_vibe() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path(), "3.0.1", GOOD_RUN);
    let mut config = config(dir.path(), fake, Access::Inherit);
    config.model = None;
    let work = config.work_dir.clone();
    one_run(dir.path(), config, "ok").await;

    let args = read(dir.path(), "args");
    assert!(!args.contains("--enabled-tools"), "{args}");
    assert!(args.contains("--max-turns=7"), "{args}");
    assert_eq!(
        cwd(dir.path()),
        std::env::current_dir().unwrap().canonicalize().unwrap()
    );
    let env = read(dir.path(), "env");
    assert!(
        !env.contains(&format!("VIBE_HOME={}", work.join("vibe-home").display())),
        "{env}"
    );
    assert!(!env.contains("VIBE_ACTIVE_MODEL="), "{env}");
    assert!(env.contains("VIBE_SESSION_LOGGING__ENABLED=false"), "{env}");
    assert!(!work.join("vibe-home/config.toml").exists());
    let servers: Value = serde_json::from_str(&read(dir.path(), "servers.json")).unwrap();
    assert_eq!(servers[0]["name"], "wats");
    assert_eq!(
        servers[0]["args"],
        serde_json::json!(["--dir", "/d", "mcp", "--profile", "agent"])
    );
    assert!(servers[0]["env"]["WATS_RUNNER_TOKEN"].is_string());
    let tools: Value = serde_json::from_str(&read(dir.path(), "tools.json")).unwrap();
    assert_eq!(tools["wats_wait"]["permission"], "always");
}

/// A run Vibe could not start (exit 1, `config_error`: no API key) is
/// `error` with Vibe's message; a run stopped at its turn limit (exit 3)
/// is `error` with the outcome; a run whose export has no cost (a model
/// without prices) counts against the runs per day.
#[tokio::test(flavor = "multi_thread")]
async fn errors_limits_and_runs_without_a_price() {
    let dir = tempfile::tempdir().unwrap();
    let failing = fake(
        dir.path(),
        "3.0.1",
        r#"echo 'not json'
echo 'Error: Missing API key: set MISTRAL_API_KEY' >&2
echo '{"outcome":"config_error","error":{"message":"Missing API key for provider mistral","code":"unauthorized"},"cost_usd":null,"steps":0,"exit_code":1}' > "$dir/export.json"
exit 1"#,
    );
    let record = one_run(
        dir.path(),
        config(dir.path(), failing, Access::SwarmOnly),
        "error",
    )
    .await;
    assert_eq!(record.exit, Some(1));
    assert_eq!(
        record.error.as_deref(),
        Some("Missing API key for provider mistral")
    );

    let dir = tempfile::tempdir().unwrap();
    let limited = fake(
        dir.path(),
        "3.0.1",
        r#"echo 'Stopped: turn_limit' >&2
echo '{"outcome":"turn_limit","stop_reason":"limit","error":null,"cost_usd":0,"steps":7,"exit_code":3}' > "$dir/export.json"
exit 3"#,
    );
    let config = config(dir.path(), limited, Access::SwarmOnly);
    let work = config.work_dir.clone();
    let record = one_run(dir.path(), config, "error").await;
    assert_eq!(record.exit, Some(3));
    assert_eq!(record.error.as_deref(), Some("turn_limit"));
    assert_eq!(record.turns, Some(7));
    assert_eq!(record.cost_usd, None);
    assert!(wats::budget::spend(&work, now()).uncosted_today >= 1);
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// At the timeout the runner sends SIGTERM to Vibe; a Vibe that ignores
/// it is killed with its whole process group after the grace: what it
/// started (MCP servers; here a `sleep`) goes too. A run that wrote no
/// export is charged its --max-price (here what was left of the day's
/// budget, $2), so that timeouts cannot spend unseen.
#[tokio::test(flavor = "multi_thread")]
async fn the_timeout_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let fake = fake(
        dir.path(),
        "3.0.1",
        &format!(
            "trap '' TERM\nsleep 300 &\necho $! > {}/grandchild\nsleep 300",
            out.display()
        ),
    );
    let mut config = config(dir.path(), fake, Access::SwarmOnly);
    // Room for the fake CLI to start on a loaded CI runner.
    config.run_timeout = Duration::from_secs(5);
    let work = config.work_dir.clone();
    let record = one_run(dir.path(), config, "timeout").await;
    assert_eq!(record.cost_usd, Some(2.0));
    assert!((wats::budget::spend(&work, now()).usd_today - 2.0).abs() < 1e-9);
    let pid = read(dir.path(), "grandchild").trim().to_string();
    assert!(!pid.is_empty());
    eventually("the grandchild is gone", || !alive(&pid)).await;
}

/// Vibe handles SIGTERM: it writes its run export (and stops its app
/// server) before it exits. The runner then records that cost, not the
/// cap. It exited in its grace and was reaped, so its process group id may
/// already be another's: nothing is killed afterwards (here the `sleep` it
/// left behind lives on, and the test stops it).
#[tokio::test(flavor = "multi_thread")]
async fn a_timeout_lets_vibe_write_its_export() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let fake = fake(
        dir.path(),
        "3.0.1",
        &format!(
            r#"trap 'echo got-term > {out}/term; echo "{{\"outcome\":\"terminated\",\"cost_usd\":0.03,\"steps\":1,\"exit_code\":3}}" > "$dir/export.json"; exit 3' TERM
sleep 300 &
echo $! > {out}/grandchild
wait $!"#,
            out = out.display()
        ),
    );
    let mut config = config(dir.path(), fake, Access::SwarmOnly);
    // Room for the fake CLI to start on a loaded CI runner.
    config.run_timeout = Duration::from_secs(5);
    let record = one_run(dir.path(), config, "timeout").await;
    assert_eq!(read(dir.path(), "term").trim(), "got-term");
    assert_eq!(record.cost_usd, Some(0.03));
    assert_eq!(record.turns, Some(1));
    let pid = read(dir.path(), "grandchild").trim().to_string();
    assert!(!pid.is_empty());
    // Well after the run was finished: still there, nothing was killed.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let still = alive(&pid);
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status();
    assert!(still, "the group was killed after its leader was reaped");
}

/// Below Vibe 3.0.0, at `workspace`, or with arguments that undo the
/// level, the runner does not start.
#[tokio::test(flavor = "multi_thread")]
async fn old_versions_workspace_and_undoing_arguments_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir.path().join("node")).await.unwrap();
    let old = fake(dir.path(), "2.26.1", "exit 0");
    let e = wats::agent::run(
        Arc::clone(&a),
        config(dir.path(), old, Access::SwarmOnly),
        async {},
    )
    .await
    .unwrap_err();
    assert!(format!("{e:#}").contains("2.26.1"), "{e:#}");

    let current = fake(dir.path(), "3.0.1", "exit 0");
    let e = wats::agent::run(
        Arc::clone(&a),
        config(dir.path(), current.clone(), Access::Workspace),
        async {},
    )
    .await
    .unwrap_err();
    assert!(format!("{e:#}").contains("cannot keep its tools"), "{e:#}");

    for extra in [
        ["--auto-approve"],
        ["--enabled-tools=bash"],
        ["-c"],
        ["--check-upgrade"],
    ] {
        let mut config = config(dir.path(), current.clone(), Access::SwarmOnly);
        config.extra_args = extra.map(String::from).to_vec();
        let e = wats::agent::run(Arc::clone(&a), config, async {})
            .await
            .unwrap_err();
        let flag = extra[0].split('=').next().unwrap();
        assert!(format!("{e:#}").contains(flag), "{e:#}");
    }
    let mut config = config(dir.path(), current, Access::Inherit);
    config.extra_args = vec!["--output=json".into()];
    let e = wats::agent::run(Arc::clone(&a), config, async {})
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("set by the runner"), "{e:#}");
    assert!(read(dir.path(), "args").is_empty(), "no run started");
    a.shutdown().await.unwrap();
}

/// A throwaway machine for the CLI: HOME, XDG directories, VIBE_HOME, run
/// directory.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["home", "xdg-config", "xdg-data", "vibe", "run", "bin"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        Machine { tmp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn wats(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("WATS_RUN_DIR", self.path("run"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("XDG_DATA_HOME", self.path("xdg-data"))
            .env("VIBE_HOME", self.path("vibe"))
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_DIR")
            .env_remove("MISTRAL_API_KEY")
            .args(["--infra", "none", "--no-default-bootstrap", "--dir"])
            .arg(self.path("data"))
            .args(args)
            .stdin(std::process::Stdio::null());
        command
    }

    fn setup(&self, args: &[&str]) -> std::process::Output {
        let mut all = vec!["setup", "vibe"];
        all.extend_from_slice(args);
        self.wats(&all).output().unwrap()
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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `wats setup vibe --agent` enables a swarm-only Vibe agent and writes
/// nothing in HOME, the XDG directories or VIBE_HOME; `--access
/// workspace` is refused before anything is written; `--access inherit`
/// says the warning, and that the runner needs agent.allow and
/// agent.review; with no flag, setup only says what each does.
#[test]
fn setup_enables_a_swarm_only_agent() {
    let m = Machine::new();
    let out = m.setup(&[]);
    assert!(out.status.success(), "{out:?}");
    let said = text(&out.stdout);
    assert!(said.contains("wats setup vibe --mcp"), "{said}");
    assert!(said.contains("wats setup vibe --agent"), "{said}");
    assert!(!m.path("data/config.toml").exists());

    let out = m.setup(&["--agent", "--access", "workspace"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        text(&out.stderr).contains("cannot keep its tools"),
        "{out:?}"
    );
    assert!(!m.path("data/config.toml").exists());

    let out = m.setup(&["--agent"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}{}", text(&out.stderr));
    let config = std::fs::read_to_string(m.path("data/config.toml")).unwrap();
    assert!(config.contains("[agents.vibe]"), "{config}");
    assert!(config.contains("runtime = \"vibe\""), "{config}");
    assert!(config.contains("access = \"swarm-only\""), "{config}");
    assert!(said.contains("Mistral Vibe agent: swarm-only"), "{said}");
    assert!(said.contains("MISTRAL_API_KEY"), "{said}");
    for dir in ["home", "xdg-config", "xdg-data", "vibe"] {
        assert!(m.files(dir).is_empty(), "{dir}: {:?}", m.files(dir));
    }
    // `wats config set` refuses workspace for a Vibe agent too, and says
    // agent.tools does not apply to it.
    let set = |key: &str, value: &str| m.wats(&["config", "set", key, value]).output().unwrap();
    let out = set("agents.vibe.access", "workspace");
    assert!(!out.status.success(), "{out:?}");
    assert!(
        text(&out.stderr).contains("cannot keep its tools"),
        "{out:?}"
    );
    let out = set("agents.vibe.tools", "read_file");
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out.stderr).contains("Mistral Vibe has no workspace level"),
        "{out:?}"
    );
    let out = set("agents.other.access", "workspace");
    assert!(out.status.success(), "{out:?}");
    let out = set("agents.other.runtime", "vibe");
    assert!(!out.status.success(), "{out:?}");
    let config = std::fs::read_to_string(m.path("data/config.toml")).unwrap();
    assert!(config.contains("access = \"swarm-only\""), "{config}");
    assert!(
        !config.contains("access = \"workspace\"\nruntime"),
        "{config}"
    );

    let out = m.setup(&["--agent", "--access", "inherit"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out.stdout).contains(wats::vibe::INHERIT_WARNING),
        "{out:?}"
    );
    assert!(text(&out.stderr).contains("agent.review = true"), "{out:?}");
    let config = std::fs::read_to_string(m.path("data/config.toml")).unwrap();
    assert!(config.contains("access = \"inherit\""), "{config}");
    // Either guardrail is enough for Vibe (ADR 0033 §2): review alone.
    let set = m
        .wats(&["config", "set", "agents.vibe.review", "true"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{set:?}");
    let out = m.setup(&["--agent", "--access", "inherit"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !text(&out.stderr).contains("agent.review = true"),
        "{out:?}"
    );
}

/// `wats --agent vibe`: the node starts the runner with the agent's
/// settings (here swarm-only by default), and a heartbeat run reaches the
/// fake `vibe`; `--access workspace`, and `inherit` without agent.allow and
/// agent.review, stop it at start.
#[test]
fn the_node_runs_a_vibe_agent() {
    let m = Machine::new();
    let bin = m.path("bin");
    let fake = fake(&bin, "3.0.1", GOOD_RUN);
    let fake = fake.to_str().unwrap();
    let refused = |access: &str, why: &str| {
        let out = m
            .wats(&[
                "--loopback",
                "--agent",
                "vibe",
                "--access",
                access,
                "--agent-command",
                fake,
            ])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{access}: {out:?}");
        assert!(text(&out.stderr).contains(why), "{access}: {out:?}");
    };
    refused("workspace", "cannot keep its tools");
    refused("inherit", "agent.review = true");

    let mut node = m
        .wats(&[
            "--loopback",
            "--agent",
            "vibe",
            "--heartbeat",
            "1s",
            "--agent-command",
            fake,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while read(&bin, "args").is_empty() {
        if let Some(status) = node.try_wait().unwrap() {
            panic!("the node exited: {status}: {:?}", node.wait_with_output());
        }
        assert!(std::time::Instant::now() < deadline, "no run after 60 s");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::process::Command::new("kill")
        .args(["-INT", &node.id().to_string()])
        .status();
    let output = node.wait_with_output().unwrap();
    let args = read(&bin, "args");
    assert!(args.contains("--enabled-tools=wats_post"), "{args}");
    let run_config = read(&bin, "config.toml");
    assert!(
        run_config.contains("\"--profile\", \"alice-agent\""),
        "{run_config}"
    );
    let stderr = text(&output.stderr);
    assert!(stderr.contains("not ~/.vibe"), "{stderr}");
    // Nothing of the user's Vibe was touched.
    assert!(m.files("vibe").is_empty(), "{:?}", m.files("vibe"));
}
