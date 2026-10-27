//! What reaches the human of replies to its asks without a push channel:
//! the session hook of Claude Code and Codex (`wats notify --hook`), and
//! the node's desktop notification (`notify.desktop`), with a fake
//! notifier. Hermetic: throwaway HOMEs, fake `claude` and `codex` scripts,
//! never a real agent CLI or a real notification.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// A `wats mcp` session as an MCP host runs it, on a hermetic background
/// node; `notifier`: the fake desktop notifier the node runs.
async fn session(dir: &Path, home: &Path, profile: &str, notifier: Option<&Path>) -> Client {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.env("WATS_HERMETIC", "1")
        .env("HOME", home)
        .env_remove("WATS_MCP_CLIENT")
        .env_remove("WATS_PROFILE")
        .args(["--loopback", "--pow-bits=4", "--profile", profile, "--dir"])
        .arg(dir)
        .args(["mcp", "--idle-exit", "2"])
        .current_dir(home);
    match notifier {
        Some(fake) => cmd.env("WATS_TEST_DESKTOP_NOTIFY", fake),
        None => cmd.env_remove("WATS_TEST_DESKTOP_NOTIFY"),
    };
    let transport = TokioChildProcess::new(cmd).expect("spawn wats mcp");
    ().serve(transport).await.expect("mcp handshake")
}

async fn call(client: &Client, tool: &'static str, args: Value) -> Value {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.unwrap()
}

/// `wats notify --hook` as a host runs it: stdout, and how long it took.
fn hook(dir: &Path, home: &Path, profile: &str, host: &str, event: &str) -> (String, Duration) {
    let start = Instant::now();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("HOME", home)
        .env_remove("WATS_PROFILE")
        .env_remove("WATS_AGENT_RUN")
        .args(["notify", "--hook", "--host", host, "--event", event])
        .args(["--profile", profile, "--dir"])
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let took = start.elapsed();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    (String::from_utf8(out.stdout).unwrap(), took)
}

/// The Claude Code hook output's context, checked against its schema.
fn context(stdout: &str, event: &str) -> String {
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], event, "{v}");
    let text = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.chars().count() <= 1500, "{}", text.len());
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_lists_a_pending_clarify_and_a_new_answer_once() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("wats");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // The first exec of a freshly linked binary can be slow (macOS scans
    // it once): not the hook's time.
    std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .arg("--version")
        .output()
        .unwrap();
    let mut timings = Vec::new();
    let mut hook = |profile: &str, host: &str, event: &str| {
        let (out, took) = hook(&dir, &home, profile, host, event);
        timings.push(took);
        out
    };

    // No data directory: silent, exit 0, nothing created.
    assert_eq!(hook("alice", "claude", "user-prompt-submit"), "");
    assert_eq!(hook("alice", "codex", "session-start"), "");
    assert!(!dir.exists());

    let alice = session(&dir, &home, "alice", None).await;
    let bob = session(&dir, &home, "bob", None).await;
    let ask = call(
        &alice,
        "post",
        json!({"title": "Convert this report", "text": "To a spreadsheet, please.", "kind": "ask"}),
    )
    .await;
    let ask_id = ask["id"].as_str().unwrap().to_string();
    // Nothing new yet.
    assert_eq!(hook("alice", "claude", "user-prompt-submit"), "");

    // Bob asks a clarify: listed with its id and options, then not again
    // in the same burst of prompts.
    let clarify = call(
        &bob,
        "reply",
        json!({"post_id": ask_id, "kind": "clarify", "title": "CSV or XLSX?",
               "text": "Which format?", "data": {"options": ["csv", "xlsx"]}}),
    )
    .await;
    let clarify_id = clarify["id"].as_str().unwrap().to_string();
    let text = context(
        &hook("alice", "claude", "user-prompt-submit"),
        "UserPromptSubmit",
    );
    assert!(text.contains(&format!("(post {clarify_id})")), "{text}");
    assert!(
        text.contains("CSV or XLSX?") && text.contains("csv; xlsx"),
        "{text}"
    );
    assert!(text.contains("Convert this report"), "{text}");
    assert_eq!(hook("alice", "claude", "user-prompt-submit"), "");
    // Not bob's: his profile has nothing waiting.
    assert_eq!(hook("bob", "claude", "user-prompt-submit"), "");

    // Bob answers: the answer is listed once, the clarify not again.
    let answer = call(
        &bob,
        "reply",
        json!({"post_id": ask_id, "title": "Here is a CSV", "text": "Attached."}),
    )
    .await;
    let answer_id = answer["id"].as_str().unwrap().to_string();
    let text = context(
        &hook("alice", "claude", "user-prompt-submit"),
        "UserPromptSubmit",
    );
    assert!(text.contains(&format!("(post {answer_id})")), "{text}");
    assert!(text.contains("Here is a CSV"), "{text}");
    assert!(!text.contains(&clarify_id), "{text}");
    assert_eq!(hook("alice", "claude", "user-prompt-submit"), "");

    // A new session is a new burst: the clarify, still unanswered, comes
    // back; the answer does not. Codex gets plain text.
    let text = hook("alice", "codex", "session-start");
    assert!(
        text.contains(&clarify_id) && !text.contains(&answer_id),
        "{text}"
    );
    assert!(serde_json::from_str::<Value>(&text).is_err(), "{text}");
    // Codex's prompt hook (0.116.0+), in the same burst: nothing new.
    assert_eq!(hook("alice", "codex", "user-prompt-submit"), "");
    let text = context(&hook("alice", "claude", "session-start"), "SessionStart");
    assert!(text.contains(&clarify_id), "{text}");

    // Answered: never listed again.
    call(
        &alice,
        "reply",
        json!({"post_id": clarify_id, "text": "XLSX.", "data": {"choice": "xlsx"}}),
    )
    .await;
    assert_eq!(hook("alice", "claude", "session-start"), "");
    assert!(dir.join("hooks").join("seen-alice.json").exists());

    // In an agent's run, the hook stays quiet whatever is new.
    call(
        &bob,
        "reply",
        json!({"post_id": ask_id, "title": "And an XLSX", "text": "Attached too."}),
    )
    .await;
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("HOME", &home)
        .env("WATS_AGENT_RUN", "1")
        .args(["notify", "--hook", "--profile", "alice", "--dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    // A wats too old for a flag of the hook's command line: still exit 0.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .args(["notify", "--hook", "--some-future-flag"])
        .output()
        .unwrap();
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");

    alice.cancel().await.unwrap();
    bob.cancel().await.unwrap();

    let max = timings.iter().max().unwrap();
    let mut sorted = timings.clone();
    sorted.sort();
    eprintln!(
        "wats notify --hook: {} runs, median {:?}, max {:?}",
        sorted.len(),
        sorted[sorted.len() / 2],
        max
    );
    assert!(*max < Duration::from_millis(500), "{timings:?}");
}

/// A fake notifier: it appends its arguments, `|`-joined, to a log.
fn fake_notifier(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("notified.log");
    let fake = dir.join("notify");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\" >> {}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

fn notified(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn wait_for_lines(log: &Path, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let lines = notified(log);
        if lines.len() >= n || Instant::now() > deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn desktop_notification_for_the_humans_asks_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("wats");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let (fake, log) = fake_notifier(tmp.path());
    // An agent of this node (not running here: a background node runs
    // none), with its own profile; its command is never run.
    std::fs::write(
        dir.join("config.toml"),
        "[agents.helper]\nruntime = \"claude\"\nprofile = \"helper\"\nenabled = false\n\
         command = \"/nonexistent/claude\"\n",
    )
    .unwrap();

    let alice = session(&dir, &home, "alice", Some(&fake)).await;
    let helper = session(&dir, &home, "helper", Some(&fake)).await;
    let bob = session(&dir, &home, "bob", Some(&fake)).await;

    let ask = call(
        &alice,
        "post",
        json!({"title": "Translate \"this\" \\ please", "text": "Into French.", "kind": "ask"}),
    )
    .await;
    let ask_id = ask["id"].as_str().unwrap().to_string();
    let agent_ask = call(
        &helper,
        "post",
        json!({"title": "Agent question", "text": "Anyone?", "kind": "ask"}),
    )
    .await;
    let agent_ask_id = agent_ask["id"].as_str().unwrap().to_string();

    // A reply to the agent's ask: nothing.
    call(
        &bob,
        "reply",
        json!({"post_id": agent_ask_id, "text": "Me."}),
    )
    .await;
    // A clarify on the human's ask: one notification, the strings as
    // arguments.
    call(
        &bob,
        "reply",
        json!({"post_id": ask_id, "kind": "clarify", "text": "Formal or not?"}),
    )
    .await;
    let lines = wait_for_lines(&log, 1).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    // bob is a profile of this same node: named, not an id.
    assert_eq!(
        lines[0],
        "WeAreTheSwarm|Clarify from bob (this node): Translate \"this\" \\ please"
    );
    // The human's own reply: nothing.
    call(
        &alice,
        "reply",
        json!({"post_id": ask_id, "text": "Formal."}),
    )
    .await;

    // Answers: at most 10 notifications per hour.
    for i in 0..12 {
        call(
            &bob,
            "reply",
            json!({"post_id": ask_id, "text": format!("Version {i}.")}),
        )
        .await;
    }
    let lines = wait_for_lines(&log, 10).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let lines_after = notified(&log);
    assert_eq!(lines_after.len(), 10, "{lines_after:?}");
    assert!(
        lines[1].starts_with("WeAreTheSwarm|Answer from bob (this node): "),
        "{lines:?}"
    );
    assert!(
        lines_after.iter().all(|l| !l.contains("Agent question")),
        "{lines_after:?}"
    );

    alice.cancel().await.unwrap();
    helper.cancel().await.unwrap();
    bob.cancel().await.unwrap();
}

/// Without the test notifier, a hermetic node never notifies (nor runs
/// osascript or notify-send): the same flow leaves no trace.
#[tokio::test(flavor = "multi_thread")]
async fn no_desktop_notification_in_hermetic_mode_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("wats");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "[notify]\ndesktop = true\n").unwrap();
    let alice = session(&dir, &home, "alice", None).await;
    let bob = session(&dir, &home, "bob", None).await;
    let ask = call(
        &alice,
        "post",
        json!({"title": "Q", "text": "?", "kind": "ask"}),
    )
    .await;
    call(&bob, "reply", json!({"post_id": ask["id"], "text": "A."})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let log = std::fs::read_to_string(dir.join("logs").join("node.log")).unwrap_or_default();
    assert!(!log.contains("desktop notification"), "{log}");
    alice.cancel().await.unwrap();
    bob.cancel().await.unwrap();
}

/// A logging fake `claude`: records its arguments, never runs anything.
fn fake_claude(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("claude.log");
    let fake = dir.join("claude");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

/// A fake `codex`: `features list` prints `features` (one per line);
/// `mcp get` fails (not registered); the rest is logged.
fn fake_codex(dir: &Path, features: &[&str], version: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("codex.log");
    let fake = dir.join("codex");
    let list: String = features
        .iter()
        .map(|f| format!("{f}    stable    false\\n"))
        .collect();
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" >> {log}\n\
             if [ \"$1\" = features ]; then printf '{list}'; exit 0; fi\n\
             if [ \"$1\" = --version ]; then echo 'codex-cli {version}'; exit 0; fi\n\
             if [ \"$2\" = get ]; then exit 1; fi\nexit 0\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

fn setup(home: &Path, data: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("HOME", home)
        .env("USER", "alice")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("WATS_PROFILE")
        .current_dir(home)
        .arg("--dir")
        .arg(data)
        .arg("setup")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn backups(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("{name}.bak-"))
        })
        .collect();
    found.sort();
    found
}

#[test]
fn setup_claude_adds_the_hook_once_and_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let settings_dir = home.join(".claude");
    std::fs::create_dir_all(&settings_dir).unwrap();
    let settings = settings_dir.join("settings.json");
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"say done\"\n          }\n        ]\n      }\n    ]\n  },\n  \"env\": {\n    \"FOO\": \"1\"\n  }\n}\n";
    std::fs::write(&settings, original).unwrap();
    let (claude, log) = fake_claude(tmp.path());
    let claude = claude.to_str().unwrap();

    let said = setup(&home, &data, &["claude", "--mcp", "--claude", claude]);
    assert!(
        said.contains("hooks (UserPromptSubmit, SessionStart"),
        "{said}"
    );
    // The /swarm command (ADR 0056 §4).
    let swarm = settings_dir.join("commands").join("swarm.md");
    assert!(
        std::fs::read_to_string(&swarm)
            .unwrap()
            .contains(" me` in the shell")
    );
    let found = backups(&settings_dir, "settings.json");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(std::fs::read_to_string(&found[0]).unwrap(), original);
    let text = std::fs::read_to_string(&settings).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["model"], "opus");
    assert_eq!(v["env"]["FOO"], "1");
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
    // The nudges (ADR 0056 §1), next to the user's own Stop hook.
    for (event, arg) in [
        ("PostToolUse", "post-tool"),
        ("PostToolUseFailure", "post-tool"),
        ("Stop", "stop"),
    ] {
        let groups = v["hooks"][event].as_array().unwrap();
        let ours = groups.last().unwrap()["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            ours.ends_with(&format!(
                " hook {arg} --host claude --dir {}",
                data.display()
            )),
            "{ours}"
        );
    }
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    for (event, arg) in [
        ("UserPromptSubmit", "user-prompt-submit"),
        ("SessionStart", "session-start"),
    ] {
        let groups = v["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{text}");
        let hook = &groups[0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(
            hook["command"],
            format!(
                "{} notify --hook --host claude --event {arg} --profile alice --dir {}",
                exe.display(),
                data.display()
            )
        );
    }
    // Key order kept: model, hooks, env.
    let (m, h, e) = (
        text.find("\"model\"").unwrap(),
        text.find("\"hooks\"").unwrap(),
        text.find("\"env\"").unwrap(),
    );
    assert!(m < h && h < e, "{text}");

    // Idempotent: nothing written, no new backup.
    let said = setup(&home, &data, &["claude", "--mcp", "--claude", claude]);
    assert!(said.contains("hooks already in"), "{said}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), text);
    assert_eq!(backups(&settings_dir, "settings.json").len(), 1);

    // --remove-hooks: the hooks go, the MCP server stays.
    std::fs::remove_file(&log).unwrap();
    let said = setup(
        &home,
        &data,
        &["claude", "--remove-hooks", "--claude", claude],
    );
    assert!(said.contains("session hook removed"), "{said}");
    assert!(!said.contains("MCP server"), "{said}");
    assert!(!swarm.exists(), "/swarm goes with the hooks");
    assert!(!log.exists(), "the claude CLI ran");
    let removed: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(removed, serde_json::from_str::<Value>(original).unwrap());
    let said = setup(&home, &data, &["claude", "--mcp", "--claude", claude]);
    assert!(said.contains("hooks (UserPromptSubmit"), "{said}");
    std::fs::remove_file(&log).unwrap();

    // --remove: the hook and the MCP server go; the user's settings stay.
    let said = setup(&home, &data, &["claude", "--remove", "--claude", claude]);
    assert!(said.contains("session hook removed"), "{said}");
    let removed: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(removed, serde_json::from_str::<Value>(original).unwrap());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().trim(),
        "mcp remove --scope user wats"
    );
    assert_eq!(backups(&settings_dir, "settings.json").len(), 4);
    let said = setup(&home, &data, &["claude", "--remove", "--claude", claude]);
    assert!(said.contains("no wats session hook"), "{said}");

    // --no-hooks: the MCP server only.
    let before = std::fs::read_to_string(&settings).unwrap();
    let said = setup(
        &home,
        &data,
        &["claude", "--mcp", "--no-hooks", "--claude", claude],
    );
    assert!(!said.contains("session hook"), "{said}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), before);

    // No settings.json yet: created, no backup.
    let other = tmp.path().join("home2");
    std::fs::create_dir_all(&other).unwrap();
    let said = setup(&other, &data, &["claude", "--mcp", "--claude", claude]);
    assert!(said.contains("(new file)"), "{said}");
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(other.join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert!(
        v["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("wats notify --hook")
    );
}

#[test]
fn setup_codex_hook_follows_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let codex_dir = home.join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let config = codex_dir.join("config.toml");
    let original = "# mine\nmodel = \"gpt-5\"\n\n[features]\nshell_tool = true\n";
    std::fs::write(&config, original).unwrap();

    // A Codex without hooks: skipped, config.toml untouched.
    let bin = tmp.path().join("old");
    std::fs::create_dir_all(&bin).unwrap();
    let (codex, log) = fake_codex(&bin, &["shell_tool", "apps"], "0.113.0");
    let said = setup(
        &home,
        &data,
        &["codex", "--mcp", "--codex", codex.to_str().unwrap()],
    );
    assert!(said.contains("Codex: no session hook"), "{said}");
    assert!(said.contains("no hooks feature"), "{said}");
    assert!(said.contains("0.114.0"), "{said}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    assert!(backups(&codex_dir, "config.toml").is_empty());
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("features list")
    );

    // An older Codex with `codex_hooks` (SessionStart only), then a newer
    // one with `hooks` and UserPromptSubmit.
    for (name, feature, version) in [
        ("older", "codex_hooks", "0.114.0"),
        ("newer", "hooks", "0.116.0"),
    ] {
        std::fs::write(&config, original).unwrap();
        let bin = tmp.path().join(name);
        std::fs::create_dir_all(&bin).unwrap();
        let (codex, _) = fake_codex(&bin, &["shell_tool", feature], version);
        let codex = codex.to_str().unwrap();
        let said = setup(&home, &data, &["codex", "--mcp", "--codex", codex]);
        assert!(
            said.contains(&format!("features.{feature} = true")),
            "{said}"
        );
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with("# mine\nmodel = \"gpt-5\"\n"), "{text}");
        let v: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(v["features"][feature].as_bool(), Some(true), "{text}");
        assert_eq!(v["features"]["shell_tool"].as_bool(), Some(true), "{text}");
        let command = v["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command
                .contains("wats notify --hook --host codex --event session-start --profile alice"),
            "{command}"
        );
        let prompt = v
            .get("hooks")
            .and_then(|h| h.get("UserPromptSubmit"))
            .and_then(|l| l[0]["hooks"][0]["command"].as_str());
        assert!(said.contains("open /hooks in Codex once"), "{said}");
        if version == "0.116.0" {
            assert!(said.contains("[[hooks.UserPromptSubmit]]"), "{said}");
            assert!(
                prompt
                    .unwrap()
                    .contains("--host codex --event user-prompt-submit"),
                "{text}"
            );
        } else {
            assert!(!said.contains("UserPromptSubmit"), "{said}");
            assert_eq!(prompt, None, "{text}");
        }
        // Idempotent.
        let said = setup(&home, &data, &["codex", "--mcp", "--codex", codex]);
        assert!(said.contains("hooks already in"), "{said}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
        // --remove-hooks: the hook only.
        let said = setup(&home, &data, &["codex", "--remove-hooks", "--codex", codex]);
        assert!(said.contains("session hook removed"), "{said}");
        assert!(!said.contains("MCP server"), "{said}");
        let without = std::fs::read_to_string(&config).unwrap();
        assert!(!without.contains("notify --hook"), "{without}");
        let said = setup(&home, &data, &["codex", "--mcp", "--codex", codex]);
        assert!(said.contains("[[hooks.SessionStart]]"), "{said}");
        // --remove: the hook goes (the feature flag stays), then the MCP
        // server.
        let said = setup(&home, &data, &["codex", "--remove", "--codex", codex]);
        assert!(said.contains("session hook removed"), "{said}");
        assert!(said.contains("MCP server removed"), "{said}");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(!text.contains("hooks.SessionStart"), "{text}");
        assert!(!text.contains("notify --hook"), "{text}");
        assert!(text.contains(&format!("{feature} = true")), "{text}");
    }
    assert!(!backups(&codex_dir, "config.toml").is_empty());
}

/// ADR 0044 §5: an ask that wants two answers (mode n, want 2) is held
/// until both are in, then listed once, together, for one synthesis.
#[tokio::test(flavor = "multi_thread")]
async fn hook_holds_answers_until_the_ask_has_what_it_wants() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("wats");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let alice = session(&dir, &home, "alice", None).await;
    let bob = session(&dir, &home, "bob", None).await;
    let carol = session(&dir, &home, "carol", None).await;
    let ask = call(
        &alice,
        "post",
        json!({"title": "Is this regex safe?", "text": "A second opinion, please.",
               "kind": "ask", "data": {"mode": "n", "want": 2}}),
    )
    .await;
    let ask_id = ask["id"].as_str().unwrap().to_string();
    let hook = |event: &str| hook(&dir, &home, "alice", "claude", event).0;
    let first = call(
        &bob,
        "reply",
        json!({"post_id": ask_id, "title": "Safe", "text": "No backtracking."}),
    )
    .await;
    assert_eq!(hook("user-prompt-submit"), "", "one of two: held");
    let second = call(
        &carol,
        "reply",
        json!({"post_id": ask_id, "title": "Unsafe on long input", "text": "Nested quantifier."}),
    )
    .await;
    let text = context(&hook("user-prompt-submit"), "UserPromptSubmit");
    assert!(text.contains("2 answers to your ask"), "{text}");
    assert!(text.contains("one synthesis"), "{text}");
    for id in [&first["id"], &second["id"]] {
        assert!(text.contains(id.as_str().unwrap()), "{text}");
    }
    assert_eq!(hook("user-prompt-submit"), "", "listed once");
    for c in [alice, bob, carol] {
        c.cancel().await.unwrap();
    }
}

/// `wats hook` as a host runs it, with `payload` on stdin: its stdout.
fn nudge(dir: &Path, home: &Path, host: &str, event: &str, payload: &Value) -> String {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("HOME", home)
        .env_remove("WATS_AGENT_RUN")
        .args(["hook", event, "--host", host, "--dir"])
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// ADR 0056 §1: a failing build is looked up (unknown to an empty swarm:
/// said once); two failures, an edit and a pass make a fix to share at
/// the end of the turn; the continuation it asks for is never asked
/// again; a routine failure says nothing; an agent run hears nothing.
#[tokio::test(flavor = "multi_thread")]
async fn nudges_follow_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let dir = tmp.path().join("node");
    // A store, so that the lookup runs.
    let client = session(&dir, &home, "alice", None).await;
    call(&client, "status", json!({})).await;
    let fail = json!({"session_id": "s1", "tool_name": "Bash",
        "tool_input": {"command": "cargo build"},
        "error": "Exit code 101\nerror[E0599]: no method named `frobnicate` found for struct `Widget` in the current scope"});
    let first = nudge(&dir, &home, "claude", "post-tool", &fail);
    let first: Value = serde_json::from_str(first.trim()).unwrap();
    let said = first["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(said.contains("unknown to the swarm"), "{said}");
    // The same error again: nothing (one per signature).
    assert_eq!(nudge(&dir, &home, "claude", "post-tool", &fail), "");
    let edit =
        json!({"session_id": "s1", "tool_name": "Edit", "tool_input": {"file_path": "src/w.rs"}});
    assert_eq!(nudge(&dir, &home, "claude", "post-tool", &edit), "");
    let pass = json!({"session_id": "s1", "tool_name": "Bash",
        "tool_input": {"command": "cargo build"}, "tool_response": {"stdout": "Finished", "stderr": ""}});
    assert_eq!(nudge(&dir, &home, "claude", "post-tool", &pass), "");
    let stop = nudge(
        &dir,
        &home,
        "claude",
        "stop",
        &json!({"session_id": "s1", "stop_hook_active": false}),
    );
    let stop: Value = serde_json::from_str(stop.trim()).unwrap();
    assert_eq!(stop["decision"], "block");
    let reason = stop["reason"].as_str().unwrap();
    assert!(
        reason.contains("found a fix") && reason.contains("share_finding"),
        "{reason}"
    );
    assert!(
        reason.contains("approved_by_user"),
        "share.auto is off: {reason}"
    );
    // The continuation it asked for: nothing more.
    assert_eq!(
        nudge(
            &dir,
            &home,
            "claude",
            "stop",
            &json!({"session_id": "s1", "stop_hook_active": true})
        ),
        ""
    );
    // A routine failure, in another session.
    let grep = json!({"session_id": "s2", "tool_name": "Bash", "tool_input": {"command": "grep -r foo ."},
        "error": "Exit code 1\n"});
    assert_eq!(nudge(&dir, &home, "claude", "post-tool", &grep), "");
    // Vibe's payload, and its output form.
    let vibe = json!({"session_id": "s3", "tool_name": "bash", "tool_input": {"command": "pytest"},
        "tool_status": "success",
        "tool_output": {"exit_code": 1, "stdout": "E   ModuleNotFoundError: No module named 'widgetlib'", "stderr": ""}});
    let out: Value =
        serde_json::from_str(nudge(&dir, &home, "vibe", "post-tool", &vibe).trim()).unwrap();
    assert!(
        out["hook_specific_output"]["additional_context"]
            .as_str()
            .unwrap()
            .contains("swarm")
    );
    // An agent run hears nothing.
    let mut quiet = std::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    quiet
        .env("WATS_HERMETIC", "1")
        .env("HOME", &home)
        .env("WATS_AGENT_RUN", "1")
        .args(["hook", "post-tool", "--host", "claude", "--dir"])
        .arg(&dir)
        .stdin(std::process::Stdio::null());
    assert!(quiet.output().unwrap().stdout.is_empty());
    let counters = wats::nudge::counters(&dir);
    assert!(
        counters.get("hook_post_tool").copied().unwrap_or(0) >= 5,
        "{counters:?}"
    );
    assert_eq!(counters.get("nudge_fix"), Some(&1));
    drop(client);
}

/// Copilot CLI's real payloads (captured on a real install, paths
/// neutralized): snake case with PascalCase event names, `result_type`
/// "success" even after exit 1, the exit code only in the text, the edit
/// a raw patch string. Two failures, the edit and the pass make a fix.
#[tokio::test(flavor = "multi_thread")]
async fn copilot_real_payloads_give_both_nudges() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let dir = tmp.path().join("node");
    let client = session(&dir, &home, "alice", None).await;
    call(&client, "status", json!({})).await;
    let fixture = include_str!("fixtures/hooks/copilot-session.jsonl");
    let mut said = Vec::new();
    for line in fixture.lines() {
        let payload: Value = serde_json::from_str(line).unwrap();
        let event = match payload["hook_event_name"].as_str() {
            Some("Stop") => "stop",
            _ => "post-tool",
        };
        said.push(nudge(&dir, &home, "copilot", event, &payload));
    }
    // Copilot's own output shape: `additionalContext` at the top.
    let first: Value = serde_json::from_str(said[0].trim()).unwrap();
    assert!(
        first["additionalContext"]
            .as_str()
            .unwrap()
            .contains("unknown to the swarm"),
        "{said:?}"
    );
    let stop: Value = serde_json::from_str(said.last().unwrap().trim()).unwrap();
    assert_eq!(stop["decision"], "block", "{said:?}");
    assert!(stop["reason"].as_str().unwrap().contains("found a fix"));
    drop(client);
}

/// Mistral Vibe 3.0.1's real payloads (captured on a real install, paths
/// neutralized): namespaced tool names (`file_system.bash`), the exit code
/// in `tool_output.returncode` with `tool_status` "success", commands
/// behind `cd <dir> && `, and a `post_agent` with no turn data.
#[tokio::test(flavor = "multi_thread")]
async fn vibe_real_payloads_give_both_nudges() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let dir = tmp.path().join("node");
    let client = session(&dir, &home, "alice", None).await;
    call(&client, "status", json!({})).await;
    let fixture = include_str!("fixtures/hooks/vibe-session.jsonl");
    let mut said = Vec::new();
    for line in fixture.lines() {
        let payload: Value = serde_json::from_str(line).unwrap();
        let event = match payload["hook_event_name"].as_str() {
            Some("post_agent") => "stop",
            _ => "post-tool",
        };
        said.push(nudge(&dir, &home, "vibe", event, &payload));
    }
    let first: Value = serde_json::from_str(said[0].trim()).unwrap();
    assert!(
        first["hook_specific_output"]["additional_context"]
            .as_str()
            .unwrap()
            .contains("unknown to the swarm"),
        "{said:?}"
    );
    let stop: Value = serde_json::from_str(said.last().unwrap().trim()).unwrap();
    assert_eq!(stop["decision"], "deny", "{said:?}");
    assert!(stop["reason"].as_str().unwrap().contains("found a fix"));
    drop(client);
}
