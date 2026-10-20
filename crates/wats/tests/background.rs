//! Running the node in the background (ADR 0023): `wats --detach`,
//! `wats ps`, `wats attach` and `wats stop`. Hermetic: loopback only, no
//! infrastructure, a throwaway home and registry.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

/// A throwaway home and registry of running nodes.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        Machine { tmp }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn run_dir(&self) -> PathBuf {
        self.tmp.path().join("run")
    }

    /// `wats` with this machine's home and registry.
    fn wats(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("HOME", self.dir("home"))
            .env("WATS_RUN_DIR", self.run_dir())
            .env_remove("WATS_DIR")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_MCP_SOCKET")
            .env_remove("WATS_RUNNER_TOKEN")
            .env_remove("WATS_TEST_IGNORE_STOP")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        command
    }

    /// `wats --detach` for `dir`; returns once the parent exited.
    fn detach(&self, dir: &Path, extra: &[&str]) -> Output {
        self.detach_with(dir, extra, &[])
    }

    fn detach_with(&self, dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.wats();
        command
            .args([
                "--infra",
                "none",
                "--pow-bits=0",
                "--min-pow-bits=0",
                "--dir",
            ])
            .arg(dir)
            .args(["--no-agent", "--detach"])
            .args(extra);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn ps(&self, all: bool) -> Vec<Value> {
        let mut command = self.wats();
        command.args(["ps", "--json"]);
        if all {
            command.arg("--all");
        }
        let out = command.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    /// The running node of `dir`, from `wats ps`.
    fn running(&self, dir: &Path) -> Option<Value> {
        self.ps(false)
            .into_iter()
            .find(|e| e["data_dir"].as_str() == Some(dir.to_str().unwrap()))
    }

    fn stop(&self, args: &[&str]) -> Output {
        self.wats().arg("stop").args(args).output().unwrap()
    }

    /// `wats --dir <dir> drop <id>`: the node logs "body dropped".
    fn drop_body(&self, dir: &Path, n: u32) -> String {
        let id = format!("{n:064x}");
        let out = self
            .wats()
            .arg("--dir")
            .arg(dir)
            .args(["drop", &id])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        id
    }
}

impl Drop for Machine {
    /// No node outlives its test.
    fn drop(&mut self) {
        let _ = self.stop(&["--all", "--timeout", "2s"]);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `wats attach` running, its stdout read line by line.
struct Attached {
    child: std::process::Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Attached {
    fn start(machine: &Machine, dir: &Path) -> Self {
        let mut child = machine
            .wats()
            .arg("--dir")
            .arg(dir)
            .args(["attach", "-n", "3"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Attached {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Wait for a line containing `needle`.
    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.seen.iter().any(|l| l.contains(needle)) {
                return;
            }
            if let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
                self.seen.push(line);
            }
        }
        panic!("no line with {needle:?} in:\n{}", self.seen.join("\n"));
    }

    /// Ctrl-C: the attach ends, cleanly.
    fn interrupt(mut self) -> String {
        Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "attach exited with {status}");
                break;
            }
            assert!(Instant::now() < deadline, "attach did not end on SIGINT");
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut self.child.stderr.take().unwrap(), &mut stderr).unwrap();
        stderr
    }
}

#[test]
fn detach_ps_attach_stop() {
    let machine = Machine::new();
    let a = machine.dir("a");
    let out = machine.detach(&a, &[]);
    let err = text(&out.stderr);
    assert!(out.status.success(), "{err}");
    // The parent prints the banner, where the log is, and how to attach
    // and stop.
    assert!(err.contains("· profile default"), "{err}");
    assert!(err.contains("running in the background · pid "), "{err}");
    assert!(err.contains("logs/node.log"), "{err}");
    assert!(
        err.contains("wats attach --dir ") && err.contains("wats stop --dir "),
        "{err}"
    );

    let node = machine.running(&a).expect("ps lists it");
    let pid = node["pid"].as_u64().unwrap();
    assert!(pid > 0);
    assert_eq!(node["mode"], "detached", "{node}");
    assert_eq!(node["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(node["profile"], "default");
    assert!(node["uptime_secs"].as_u64().is_some(), "{node}");
    assert_eq!(node["sessions"], 0, "{node}");
    assert!(node["log"].as_str().unwrap().ends_with("logs/node.log"));
    // The table too.
    let table = machine.wats().arg("ps").output().unwrap();
    let table = text(&table.stdout);
    assert!(table.starts_with("PID "), "{table}");
    assert!(
        table.contains(&pid.to_string()) && table.contains("detached"),
        "{table}"
    );

    // One node per data directory.
    let again = machine.detach(&a, &[]);
    assert!(!again.status.success());
    assert!(text(&again.stderr).contains("already runs in"), "{again:?}");

    // Attach: the banner, then what the node logs now.
    let mut attached = Attached::start(&machine, &a);
    attached.wait_for("· profile default");
    attached.wait_for(&format!("attached to pid {pid} (detached"));
    let id = machine.drop_body(&a, 7);
    attached.wait_for(&id);
    // Ctrl-C ends the attach only.
    let said = attached.interrupt();
    assert!(said.contains("the node keeps running"), "{said}");
    assert_eq!(machine.running(&a).unwrap()["pid"], pid);

    // Stop: graceful.
    let out = machine.stop(&["--dir", a.to_str().unwrap()]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains(&format!("stopped wats pid {pid}")), "{said}");
    assert!(machine.running(&a).is_none());
    let log = std::fs::read_to_string(a.join("logs").join("node.log")).unwrap();
    assert!(log.contains("stopping: asked by `wats stop`"), "{log}");
    // Stopping it again: nothing runs there.
    let out = machine.stop(&["--dir", a.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no wats node runs in"),
        "{out:?}"
    );
}

#[test]
fn stop_kills_a_node_that_does_not_stop() {
    let machine = Machine::new();
    let a = machine.dir("a");
    let out = machine.detach_with(&a, &[], &[("WATS_TEST_IGNORE_STOP", "1")]);
    assert!(out.status.success(), "{out:?}");
    let pid = machine.running(&a).unwrap()["pid"].as_u64().unwrap();
    let started = Instant::now();
    let out = machine.stop(&["--dir", a.to_str().unwrap(), "--timeout", "1s"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains(&format!("wats pid {pid}")) && said.contains("killed (SIGKILL)"),
        "{said}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(machine.running(&a).is_none());
}

#[test]
fn a_stale_lock_and_registry_entry_show_as_stopped_and_are_cleaned() {
    let machine = Machine::new();
    let dir = machine.dir("stale");
    std::fs::create_dir_all(&dir).unwrap();
    // A node that died without cleaning up: its holder record names a pid
    // that no longer runs, and the registry still lists it.
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    std::fs::write(
        dir.join("lock"),
        serde_json::json!({
            "pid": dead, "version": "0.3.3", "exe": "/x/wats", "started": "2026-01-01 00:00:00Z",
            "role": "daemon", "started_secs": 1, "log": dir.join("logs/node.log"),
        })
        .to_string(),
    )
    .unwrap();
    wats::background::register(&machine.run_dir(), &dir).unwrap();
    let listed = |all| {
        machine
            .ps(all)
            .into_iter()
            .find(|e| e["data_dir"].as_str() == Some(dir.to_str().unwrap()))
    };
    let entry = listed(true).expect("listed with --all");
    assert_eq!(entry["running"], false);
    assert_eq!(entry["mode"], "stopped");
    assert_eq!(entry["pid"], Value::Null);
    // The stale entry is gone from the registry: listed no more.
    assert!(wats::background::registered(&machine.run_dir()).is_empty());
    assert!(listed(true).is_none());
    assert!(machine.ps(false).is_empty());
    let out = machine.stop(&["--dir", dir.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no wats node runs in"),
        "{out:?}"
    );
    let out = machine
        .wats()
        .arg("--dir")
        .arg(&dir)
        .arg("attach")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no wats node runs in"),
        "{out:?}"
    );
}

#[test]
fn two_nodes_stop_one_then_all() {
    let machine = Machine::new();
    let (a, b) = (machine.dir("a"), machine.dir("b"));
    assert!(machine.detach(&a, &[]).status.success());
    assert!(machine.detach(&b, &["--profile", "bob"]).status.success());
    let listed = machine.ps(false);
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(machine.running(&b).unwrap()["profile"], "bob");

    let out = machine.stop(&["--dir", a.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(machine.running(&a).is_none());
    assert!(machine.running(&b).is_some(), "b still runs");

    // --profile picks the node running as that profile.
    let out = machine.stop(&["--profile", "nobody"]);
    assert!(!out.status.success());
    assert!(machine.detach(&a, &[]).status.success());
    let out = machine.stop(&["--all"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(said.matches("stopped wats pid").count(), 2, "{said}");
    assert!(machine.ps(false).is_empty());
}

#[test]
fn attach_follows_the_log_across_rotations() {
    let machine = Machine::new();
    let a = machine.dir("a");
    let out = machine.detach(&a, &["--log-max-size", "1K", "--log-keep", "2"]);
    assert!(out.status.success(), "{out:?}");
    let mut attached = Attached::start(&machine, &a);
    attached.wait_for("attached to pid");
    // Each "body dropped" line is about 200 bytes: a rotation every 5.
    let mut ids = Vec::new();
    for n in 1..=12 {
        ids.push(machine.drop_body(&a, n));
        std::thread::sleep(Duration::from_millis(250));
    }
    for id in &ids {
        attached.wait_for(id);
    }
    attached.interrupt();
    let log = a.join("logs").join("node.log");
    assert!(log.with_extension("log.1").exists());
    assert!(
        !log.with_extension("log.3").exists(),
        "two rotated files at most"
    );
    assert!(std::fs::metadata(&log).unwrap().len() < 2048);
}

/// `wats --detach` takes over the background node an MCP session started
/// (ADR 0021 §4), as `wats` does; `ps` shows which is which.
#[tokio::test(flavor = "multi_thread")]
async fn detach_takes_over_an_mcp_background_node() -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;

    let machine = Machine::new();
    let a = machine.dir("a");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.env("WATS_HERMETIC", "1")
        .env("HOME", machine.dir("home"))
        .env("WATS_RUN_DIR", machine.run_dir())
        .env_remove("WATS_RUNNER_TOKEN")
        .env_remove("WATS_DIR")
        .args([
            "--infra",
            "none",
            "--pow-bits=0",
            "--min-pow-bits=0",
            "--dir",
        ])
        .arg(&a)
        .args(["mcp", "--idle-exit", "30"]);
    let client = ().serve(TokioChildProcess::new(cmd)?).await?;
    let status = || async {
        client
            .call_tool(CallToolRequestParams::new("status"))
            .await
            .ok()
            .and_then(|r| r.structured_content)
            .unwrap_or(Value::Null)
    };
    assert_eq!(status().await["mode"], "background");
    let background = machine.running(&a).expect("ps lists the MCP node");
    assert_eq!(background["mode"], "mcp-background", "{background}");
    assert_eq!(background["sessions"], 1, "{background}");

    let out = machine.detach(&a, &[]);
    let err = text(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("took over from the background node"), "{err}");
    let detached = machine.running(&a).unwrap();
    assert_eq!(detached["mode"], "detached", "{detached}");
    assert_ne!(detached["pid"], background["pid"]);
    // The session carries on, served by the detached daemon.
    let mut mode = Value::Null;
    for _ in 0..100 {
        mode = status().await["mode"].clone();
        if mode == "daemon" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(mode, "daemon");
    // Stopping it warns about the session, and still stops it.
    let out = machine.stop(&["--dir", a.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out.stderr).contains("1 MCP session(s) use the node"),
        "{out:?}"
    );
    client.cancel().await?;
    Ok(())
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// `wats mcp` for `dir`, as an MCP host starts it, with this machine's
/// home and registry.
async fn mcp_session(machine: &Machine, dir: &Path) -> anyhow::Result<Client> {
    use rmcp::ServiceExt;
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.env("WATS_HERMETIC", "1")
        .env("HOME", machine.dir("home"))
        .env("WATS_RUN_DIR", machine.run_dir())
        .env_remove("WATS_RUNNER_TOKEN")
        .env_remove("WATS_DIR")
        .env_remove("WATS_PROFILE")
        .env_remove("WATS_MCP_SOCKET")
        .args([
            "--infra",
            "none",
            "--pow-bits=0",
            "--min-pow-bits=0",
            "--dir",
        ])
        .arg(dir)
        .args(["mcp", "--idle-exit", "30"]);
    Ok(
        ().serve(rmcp::transport::TokioChildProcess::new(cmd)?)
            .await?,
    )
}

/// The `status` tool: its result, or the text of its error.
async fn status(client: &Client) -> Result<Value, String> {
    let result = client
        .call_tool(rmcp::model::CallToolRequestParams::new("status"))
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error == Some(true) {
        return Err(serde_json::to_string(&result.content).unwrap());
    }
    Ok(result.structured_content.unwrap_or(Value::Null))
}

/// SIGKILL to a node this test started.
fn kill(pid: u64) {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn node_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("logs").join("node.log")).unwrap_or_default()
}

/// A node killed under its MCP sessions is started again, once for all of
/// them, and they carry on (ADR 0023 §6).
#[tokio::test(flavor = "multi_thread")]
async fn sessions_start_a_killed_node_again_once() -> anyhow::Result<()> {
    let machine = Machine::new();
    let a = machine.dir("a");
    let first = mcp_session(&machine, &a).await?;
    assert_eq!(status(&first).await.unwrap()["mode"], "background");
    let second = mcp_session(&machine, &a).await?;
    assert_eq!(status(&second).await.unwrap()["mode"], "background");
    let old = machine.running(&a).expect("ps lists the node");
    assert_eq!(old["sessions"], 2, "{old}");
    let old_pid = old["pid"].as_u64().unwrap();

    kill(old_pid);
    let (one, two) = tokio::join!(status(&first), status(&second));
    assert_eq!(one.unwrap()["mode"], "background");
    assert_eq!(two.unwrap()["mode"], "background");
    // One new node, serving both.
    let nodes = machine.ps(false);
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    let new = machine.running(&a).expect("a node runs again");
    assert_ne!(new["pid"], old["pid"]);
    assert_eq!(new["mode"], "mcp-background", "{new}");
    assert_eq!(new["sessions"], 2, "{new}");
    // Its log says why, once.
    let log = node_log(&a);
    assert_eq!(log.matches("respawn: ").count(), 1, "{log}");
    assert!(
        log.contains(&format!(
            "respawn: the node (pid {old_pid}) exited without `wats stop` (killed or crashed)"
        )),
        "{log}"
    );
    first.cancel().await?;
    second.cancel().await?;
    Ok(())
}

/// `wats stop` is not undone by the sessions: they say the node was
/// stopped until it runs again, then reconnect (ADR 0023 §6).
#[tokio::test(flavor = "multi_thread")]
async fn a_session_does_not_start_a_stopped_node_again() -> anyhow::Result<()> {
    let machine = Machine::new();
    let a = machine.dir("a");
    assert!(machine.detach(&a, &[]).status.success());
    let client = mcp_session(&machine, &a).await?;
    assert_eq!(status(&client).await.unwrap()["mode"], "daemon");

    let out = machine.stop(&["--dir", a.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out.stderr).contains("1 MCP session(s) use the node"),
        "{out:?}"
    );
    let record = wats::background::read_stop_record(&a).expect("the stop is recorded");
    assert_eq!(record.reason, wats::background::StopReason::Stop);
    assert_eq!(record.by, "`wats stop`");
    for _ in 0..3 {
        let said = status(&client).await.unwrap_err();
        assert!(
            said.contains("for profile default was stopped with `wats stop` at "),
            "{said}"
        );
        assert!(
            said.contains(&format!("wats --dir {} --detach", a.display())),
            "{said}"
        );
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(machine.running(&a).is_none(), "not started again");
    assert!(!node_log(&a).contains("respawn: "));

    // Started again by hand: the session reconnects on its next call.
    assert!(machine.detach(&a, &[]).status.success());
    assert!(wats::background::read_stop_record(&a).is_none());
    assert_eq!(status(&client).await.unwrap()["mode"], "daemon");
    client.cancel().await?;
    Ok(())
}

/// A node that fails at start is started again a bounded number of times,
/// with a growing wait, then the session says so (ADR 0023 §6).
#[tokio::test(flavor = "multi_thread")]
async fn a_session_gives_up_on_a_node_that_fails_at_start() -> anyhow::Result<()> {
    let machine = Machine::new();
    let a = machine.dir("a");
    assert!(machine.detach(&a, &[]).status.success());
    let client = mcp_session(&machine, &a).await?;
    assert_eq!(status(&client).await.unwrap()["mode"], "daemon");
    let pid = machine.running(&a).unwrap()["pid"].as_u64().unwrap();

    // From now on a node fails at start.
    std::fs::write(a.join("config.toml"), "not [valid toml")?;
    kill(pid);
    let started = Instant::now();
    let said = status(&client).await.unwrap_err();
    assert!(said.contains("keeps stopping"), "{said}");
    assert!(said.contains("started it 5 times in 10 minutes"), "{said}");
    assert!(said.contains("exited at start"), "{said}");
    // 1 + 2 + 4 + 8 s between the five starts.
    assert!(started.elapsed() >= Duration::from_secs(15));
    let log = node_log(&a);
    assert_eq!(log.matches("respawn: ").count(), 5, "{log}");
    assert!(log.contains("invalid "), "{log}");
    assert!(machine.running(&a).is_none());
    // Given up: the next call is answered at once, the same way.
    let again = Instant::now();
    let said = status(&client).await.unwrap_err();
    assert!(said.contains("keeps stopping"), "{said}");
    assert!(again.elapsed() < Duration::from_secs(5));
    assert_eq!(node_log(&a).matches("respawn: ").count(), 5);
    client.cancel().await?;
    Ok(())
}

/// `wats setup` offers to start the node in the background: asked on a
/// terminal (`WATS_TEST_INTERACTIVE` asks on a pipe), Enter keeps the
/// default no, `y` starts it as `wats --detach` does; without a terminal,
/// only `--detach` starts it (also as `wats --detach setup …`); when a
/// node runs, nothing is asked and it says whether to restart it. With
/// `setup vibe` (a throwaway VIBE_HOME): no agent in config.toml.
#[test]
fn setup_offers_to_start_the_node_in_the_background() {
    use std::io::Write;
    let machine = Machine::new();
    let a = machine.dir("a");
    let question = "Start the node in the background now (`wats --detach`)? [y/N]";
    let setup = |before: &[&str], args: &[&str], input: Option<&str>| -> Output {
        let mut command = machine.wats();
        command
            .env("VIBE_HOME", machine.dir("vibe"))
            .env("USER", "alice")
            .args([
                "--infra",
                "none",
                "--pow-bits=0",
                "--min-pow-bits=0",
                "--dir",
            ])
            .arg(&a)
            .args(before)
            .arg("setup")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let Some(input) = input else {
            return command.output().unwrap();
        };
        command
            .env("WATS_TEST_INTERACTIVE", "1")
            .stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let stop = || {
        let out = machine.stop(&["--dir", a.to_str().unwrap()]);
        assert!(out.status.success(), "{out:?}");
        assert!(machine.running(&a).is_none());
    };

    // On a terminal, Enter: no node.
    let out = setup(&[], &["vibe", "--mcp"], Some("\n"));
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("Mistral Vibe: wats MCP server added"),
        "{said}"
    );
    assert!(said.contains(question), "{said}");
    assert!(machine.running(&a).is_none());

    // Without a terminal: no question, no node.
    let out = setup(&[], &["vibe", "--mcp"], None);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(!said.contains("Start the node"), "{said}");
    assert!(machine.running(&a).is_none());

    // `y`: a detached node, and where its log is.
    let out = setup(&[], &["vibe", "--mcp"], Some("y\n"));
    let err = text(&out.stderr);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains(question), "{out:?}");
    assert!(err.contains("running in the background · pid "), "{err}");
    assert!(err.contains("logs/node.log"), "{err}");
    assert!(err.contains("wats attach --dir "), "{err}");
    let node = machine.running(&a).expect("ps lists it");
    assert_eq!(node["mode"], "detached", "{node}");
    let pid = node["pid"].as_u64().unwrap();

    // A node runs: no question; config.toml unchanged, nothing to restart.
    let out = setup(&[], &["vibe", "--mcp"], Some("y\n"));
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(!said.contains("Start the node"), "{said}");
    assert!(
        said.contains(&format!(
            "a wats node already runs here (pid {pid}, detached)"
        )) && said.contains("nothing to restart"),
        "{said}"
    );
    // An agent added to config.toml: restart it to apply (`--detach` does
    // not start a second node). `--agent` alone never runs `claude`.
    let out = setup(
        &[],
        &["claude", "--agent", "--detach", "--claude", "/nonexistent"],
        None,
    );
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("To apply this setup, restart it: wats stop --dir ")
            && said.contains("; wats --detach --dir "),
        "{said}"
    );
    assert_eq!(machine.running(&a).unwrap()["pid"], pid);
    stop();
    std::fs::remove_file(a.join("config.toml")).unwrap();

    // Without a terminal, --detach starts it; before `setup` too.
    for (before, args) in [
        (&[][..], &["vibe", "--mcp", "--detach"][..]),
        (&["--detach"][..], &["vibe", "--mcp"][..]),
    ] {
        let out = setup(before, args, None);
        let err = text(&out.stderr);
        assert!(out.status.success(), "{out:?}");
        assert!(!text(&out.stdout).contains("Start the node"), "{out:?}");
        assert!(err.contains("running in the background · pid "), "{err}");
        assert_eq!(machine.running(&a).expect("started")["mode"], "detached");
        stop();
    }

    // A removal starts nothing.
    let out = setup(&[], &["vibe", "--remove", "--detach"], None);
    assert!(!out.status.success(), "{out:?}");
    assert!(text(&out.stderr).contains("not with --remove"), "{out:?}");
}

/// `wats setup --detach` after `wats stop`: the node it starts clears the
/// stop, and a session that said so reconnects; a node a session started
/// again after a crash is a background node that setup reports as such
/// (ADR 0023 §6).
#[tokio::test(flavor = "multi_thread")]
async fn setup_detach_brings_back_a_stopped_session() -> anyhow::Result<()> {
    let machine = Machine::new();
    let a = machine.dir("a");
    let setup = |args: &[&str]| -> Output {
        machine
            .wats()
            .env("VIBE_HOME", machine.dir("vibe"))
            .env("USER", "alice")
            .args([
                "--infra",
                "none",
                "--pow-bits=0",
                "--min-pow-bits=0",
                "--dir",
            ])
            .arg(&a)
            .arg("setup")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(machine.detach(&a, &[]).status.success());
    let client = mcp_session(&machine, &a).await?;
    assert_eq!(status(&client).await.unwrap()["mode"], "daemon");
    assert!(
        machine
            .stop(&["--dir", a.to_str().unwrap()])
            .status
            .success()
    );
    let said = status(&client).await.unwrap_err();
    assert!(said.contains("stopped with `wats stop`"), "{said}");

    let out = setup(&["vibe", "--mcp", "--detach"]);
    assert!(out.status.success(), "{out:?}");
    assert!(wats::background::read_stop_record(&a).is_none());
    assert_eq!(status(&client).await.unwrap()["mode"], "daemon");

    // Killed: the session starts a background node, which setup reports
    // as one `wats --detach` takes over.
    let pid = machine.running(&a).unwrap()["pid"].as_u64().unwrap();
    kill(pid);
    assert_eq!(status(&client).await.unwrap()["mode"], "background");
    let out = setup(&["vibe", "--mcp"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("a wats node already runs here (pid ") && said.contains("mcp-background"),
        "{said}"
    );
    assert_eq!(machine.running(&a).unwrap()["mode"], "mcp-background");
    client.cancel().await?;
    Ok(())
}
