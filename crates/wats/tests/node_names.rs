//! Node names and `-n NAME` (ADR 0023 §8): each node of the machine has a
//! local name, `wats ps` shows it, and the commands about one node take
//! `-n NAME`. Hermetic: loopback only, no infrastructure, a throwaway home
//! and registry; `wats stop --all` only ever sees this test's nodes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use wats::event::{Draft, Kind};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

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

    fn home_wats(&self) -> PathBuf {
        self.dir("home").join(".wats")
    }

    fn run_dir(&self) -> PathBuf {
        self.tmp.path().join("run")
    }

    /// `wats --infra none --no-default-bootstrap` with this machine's home
    /// and registry.
    fn wats(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("HOME", self.dir("home"))
            .env("XDG_CONFIG_HOME", self.dir("home/.config"))
            .env("XDG_DATA_HOME", self.dir("home/.local/share"))
            .env("XDG_STATE_HOME", self.dir("home/.local/state"))
            .env("XDG_CACHE_HOME", self.dir("home/.cache"))
            .env("WATS_RUN_DIR", self.run_dir())
            .env_remove("WATS_DIR")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_MCP_SOCKET")
            .env_remove("WATS_RUNNER_TOKEN")
            .env_remove("WATS_TEST_IGNORE_STOP")
            .env_remove("RUST_LOG")
            .args(["--infra", "none", "--no-default-bootstrap"])
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.wats().args(args).output().unwrap()
    }

    /// `wats --detach`, in `dir` or (None) in `~/.wats`.
    fn detach(&self, dir: Option<&Path>) {
        let mut command = self.wats();
        command.args(["--pow-bits=0", "--min-pow-bits=0"]);
        if let Some(dir) = dir {
            command.arg("--dir").arg(dir);
        }
        let out = command.args(["--no-agent", "--detach"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    fn ps(&self, all: bool) -> Vec<Value> {
        let mut args = vec!["ps", "--json"];
        if all {
            args.push("--all");
        }
        let out = self.run(&args);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn entry(&self, dir: &Path, all: bool) -> Value {
        self.ps(all)
            .into_iter()
            .find(|e| e["data_dir"].as_str() == Some(dir.to_str().unwrap()))
            .unwrap_or_else(|| panic!("{} not in ps", dir.display()))
    }
}

impl Drop for Machine {
    /// No node outlives its test. Only this machine's registry and home:
    /// never another node of the real machine.
    fn drop(&mut self) {
        let _ = self.run(&["stop", "--all", "--timeout", "2s"]);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The first line of stderr: the header naming the node.
fn header(out: &Output) -> String {
    text(&out.stderr)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A drawn name: `adjective_creature`, from the lists.
fn assert_drawn(name: &str) {
    let (adjective, creature) = name.split_once('_').unwrap_or_else(|| panic!("{name}"));
    assert!(wats::naming::ADJECTIVES.contains(&adjective), "{name}");
    assert!(wats::naming::CREATURES.contains(&creature), "{name}");
}

#[cfg(unix)]
#[test]
fn names_in_ps_and_n_targets_a_node() {
    let machine = Machine::new();
    let home = machine.home_wats();
    let a = machine.dir("a");
    machine.detach(None);
    // One node on the machine, and neither -n nor --dir: no header line, so
    // that `2>&1` output stays as it was.
    let out = machine.run(&["config", "get", "profile"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(text(&out.stdout), "default (default)\n");
    machine.detach(Some(&a));

    // Drawn at the first start, stored in the data directory, different.
    let home_name = std::fs::read_to_string(home.join("node-name")).unwrap();
    let home_name = home_name.trim().to_string();
    let a_name = std::fs::read_to_string(a.join("node-name")).unwrap();
    let a_name = a_name.trim().to_string();
    assert_drawn(&home_name);
    assert_drawn(&a_name);
    assert_ne!(home_name, a_name);
    // Never in config.toml: wats 0.4.4 would refuse it.
    for dir in [&home, &a] {
        let config = std::fs::read_to_string(dir.join("config.toml")).unwrap_or_default();
        assert!(!config.contains("name ="), "{config}");
    }

    // `ps --json`: its name and endpoint id.
    let entry = machine.entry(&a, false);
    assert_eq!(entry["name"], a_name.as_str(), "{entry}");
    let id = entry["endpoint_id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 64, "{entry}");
    assert_eq!(machine.entry(&home, false)["name"], home_name.as_str());
    // The table: NAME first, the start of the endpoint id.
    let out = machine.run(&["ps"]);
    let table = text(&out.stdout);
    assert!(table.starts_with("NAME "), "{table}");
    assert!(table.contains(" ENDPOINT "), "{table}");
    let row = table.lines().find(|l| l.starts_with(&a_name)).unwrap();
    assert!(row.contains(&id[..12]) && !row.contains(&id[..13]), "{row}");
    // ps says nothing on stderr: it is about every node.
    assert!(out.stderr.is_empty(), "{out:?}");

    // Without -n: ~/.wats's node, said on stderr; stdout stays JSON.
    let out = machine.run(&["status", "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(header(&out), format!("node {home_name} (~/.wats)"));
    serde_json::from_slice::<Value>(&out.stdout).expect("JSON only on stdout");
    // -n: that node, wherever it is.
    let a_header = format!("node {a_name} ({})", a.display());
    for args in [
        vec!["status", "--json", "-n", &a_name],
        vec!["config", "-n", &a_name, "get", "profile"],
        vec!["config", "get", "profile", "--node", &a_name],
        vec!["agent", "-n", &a_name, "runs"],
        vec!["agent", "pending", "-n", &a_name],
    ] {
        let out = machine.run(&args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert_eq!(header(&out), a_header, "{args:?}");
    }
    let out = machine.run(&["config", "-n", &a_name, "get", "node.name"]);
    assert_eq!(
        text(&out.stdout).trim(),
        format!("{a_name} (drawn at its first start)")
    );

    // NODE/AGENT wherever --name takes an agent; plain --name still works.
    let out = machine.run(&[
        "config",
        "-n",
        &a_name,
        "set",
        "agents.reviewer.runtime",
        "claude",
    ]);
    assert!(out.status.success(), "{out:?}");
    let out = machine.run(&["agent", "--name", &format!("{a_name}/reviewer"), "runs"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(header(&out), format!("{a_header}, agent reviewer"));
    assert!(text(&out.stdout).contains("no agent runs yet"), "{out:?}");
    let out = machine
        .wats()
        .arg("--dir")
        .arg(&a)
        .args(["agent", "--name", "reviewer", "runs"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(header(&out), format!("{a_header}, agent reviewer"));
    let out = machine.run(&[
        "agent",
        "-n",
        &home_name,
        "--name",
        &format!("{a_name}/reviewer"),
        "runs",
    ]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("two different nodes"), "{out:?}");

    // --dir and -n: fine when they agree, an error otherwise.
    let with_dir = |name: &str| {
        machine
            .wats()
            .arg("--dir")
            .arg(&a)
            .args(["status", "-n", name])
            .output()
            .unwrap()
    };
    assert!(with_dir(&a_name).status.success());
    let out = with_dir(&home_name);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains(&format!("is the node {a_name}, not {home_name}")),
        "{out:?}"
    );
    // A name of no node, and a number (once the count of attach).
    let out = machine.run(&["status", "-n", "no_such_node"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no node named no_such_node"),
        "{out:?}"
    );
    let out = machine.run(&["attach", "-n", "50"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--lines"), "{out:?}");
    for command in ["logs", "runs"] {
        let out = machine.run(&["agent", command, "-n", "5"]);
        assert!(!out.status.success(), "{out:?}");
        assert!(text(&out.stderr).contains("--last"), "{out:?}");
    }
    // --all is every node: no -n with it.
    let out = machine.run(&["stop", "--all", "-n", &a_name]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("cannot be used with"), "{out:?}");
    assert_eq!(machine.ps(false).len(), 2, "nothing stopped");

    // Renamed: unique on the machine, effective at once.
    let out = machine.run(&["config", "set", "node.name", &a_name]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("already named"), "{out:?}");
    let out = machine.run(&["config", "-n", &a_name, "set", "node.name", "Not Valid"]);
    assert!(!out.status.success());
    let out = machine.run(&["config", "-n", &a_name, "set", "node.name", "hive-1"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(machine.entry(&a, false)["name"], "hive-1");
    assert_eq!(
        header(&machine.run(&["status", "-n", "hive-1"])),
        format!("node hive-1 ({})", a.display())
    );

    // Stopped by name; still found by name (and its name kept).
    let out = machine.run(&["stop", "-n", "hive-1"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(header(&out), format!("node hive-1 ({})", a.display()));
    let out = machine.run(&["status", "-n", "hive-1"]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out.stdout).contains("node: not running"), "{out:?}");
    assert_eq!(machine.entry(&a, true)["name"], "hive-1");

    // Restarts keep the names; unset, the drawn one is back.
    machine.detach(Some(&a));
    assert_eq!(machine.entry(&a, false)["name"], "hive-1");
    let out = machine.run(&["config", "-n", "hive-1", "unset", "node.name"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(machine.entry(&a, false)["name"], a_name.as_str());
    let out = machine.run(&["stop", "--dir", a.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    machine.detach(Some(&a));
    assert_eq!(machine.entry(&a, false)["name"], a_name.as_str());
    assert_eq!(
        std::fs::read_to_string(a.join("node-name")).unwrap().trim(),
        a_name
    );

    // stop --all: every node of this (throwaway) machine, each named.
    let out = machine.run(&["stop", "--all", "--timeout", "5s"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        text(&out.stdout).matches("stopped wats pid").count(),
        2,
        "{out:?}"
    );
    let said = text(&out.stderr);
    assert!(
        said.contains(&format!("node {home_name} (~/.wats)")),
        "{said}"
    );
    assert!(said.contains(&a_header), "{said}");
    assert!(machine.ps(false).is_empty());
}

/// A data directory no 0.4.5 node started in yet (an older node still
/// running there): `default` for `~/.wats`, else its directory's name,
/// in `ps` and for `-n`, until its next start draws a name.
#[cfg(unix)]
#[test]
fn an_unnamed_node_is_default_or_its_directory() {
    let machine = Machine::new();
    let old = machine.dir("old-node");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("secret.key"), format!("{}\n", "11".repeat(32))).unwrap();
    std::fs::create_dir_all(machine.home_wats()).unwrap();
    wats::background::register(&machine.run_dir(), &old).unwrap();
    assert_eq!(machine.entry(&old, true)["name"], "old-node");
    assert_eq!(machine.entry(&machine.home_wats(), true)["name"], "default");
    let out = machine.run(&["status", "-n", "old-node"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(header(&out), format!("node old-node ({})", old.display()));
    let out = machine.run(&["status", "-n", "default"]);
    assert_eq!(header(&out), "node default (~/.wats)");
    // Nothing was drawn by those commands.
    assert!(!old.join("node-name").exists());
    assert!(!machine.home_wats().join("node-name").exists());
    // Its next start names it.
    machine.detach(Some(&old));
    let name = machine.entry(&old, false)["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert_drawn(&name);
}

fn loopback(dir: &Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(8);
    config.min_pow_bits = Some(8);
    config
}

/// Every file under `dir`, read whole.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push((path, bytes));
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The name is local only: not in what a node says in `hello`, not in its
/// events, and nowhere in a peer's data directory.
#[tokio::test(flavor = "multi_thread")]
async fn the_name_never_leaves_its_node() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let (a_dir, b_dir) = (tmp.path().join("a"), tmp.path().join("b"));
    let name = "zesty_quelea_never_sent";
    wats::naming::write(&a_dir, name)?;
    std::fs::write(
        a_dir.join("config.toml"),
        "[node]\nname = \"plucky_krill_never_sent\"\n",
    )?;
    let a = Node::start(loopback(&a_dir)).await?;
    let mut b_config = loopback(&b_dir);
    b_config.peers.push(a.endpoint().addr());
    let b = Node::start(b_config).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    // Its own name is the one of config.toml.
    assert_eq!(
        wats::naming::stored(&a_dir).as_deref(),
        Some("plucky_krill_never_sent")
    );
    let names: [&[u8]; 2] = [name.as_bytes(), b"plucky_krill_never_sent"];

    // The hello, as A sends it and as B hears it.
    let hello = serde_json::to_vec(&a.our_info())?;
    let heard = serde_json::to_vec(&b.hello(a.endpoint_id()).await?)?;
    for needle in names {
        assert!(!contains(&hello, needle) && !contains(&heard, needle));
    }
    // A post: its signed event, and B's copy.
    let event = a
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("hello from a".into()),
                text: "a node with a name".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let sent = serde_json::to_vec(&event)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while b.store().get(&event.id)?.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "event on b");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // An agent of A announces itself: B gets the announce, without the name.
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(wats::agent::run(
        std::sync::Arc::clone(&a),
        agent_config(&a_dir),
        async {
            let _ = stopped.await;
        },
    ));
    let agent_id = wats::identity::author_id(&a.author_key("helper")?);
    let announces = || {
        b.store()
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while announces().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "announce on b");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = stop.send(());
    runner.await??;
    let announce = serde_json::to_vec(&announces()[0].event)?;
    assert!(contains(&announce, b"I answer questions about bees."));
    for needle in names {
        assert!(!contains(&announce, needle), "the announce holds the name");
    }
    a.shutdown().await?;
    b.shutdown().await?;
    for needle in names {
        assert!(!contains(&sent, needle));
    }
    // B's data directory: its own name only, nothing of A's.
    let b_name = wats::naming::stored(&b_dir).expect("b drew its own");
    assert!(!names.contains(&b_name.as_bytes()));
    for (path, bytes) in files(&b_dir) {
        for needle in names {
            assert!(
                !contains(&bytes, needle),
                "{} holds A's name",
                path.display()
            );
        }
    }
    // A's own store neither.
    for (path, bytes) in files(&a_dir) {
        if path.to_string_lossy().contains("events.sqlite") {
            for needle in names {
                assert!(!contains(&bytes, needle), "{}", path.display());
            }
        }
    }
    Ok(())
}

/// An agent that never runs (no heartbeat within the test), but announces
/// itself at its start.
fn agent_config(data_dir: &Path) -> wats::agent::AgentConfig {
    wats::agent::AgentConfig {
        runtime: "claude".into(),
        profile: "helper".into(),
        user: "alice".into(),
        about: "I answer questions about bees.".into(),
        caps: vec!["bees".into()],
        every: Duration::from_secs(3600),
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 1,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: wats::agent::DEFAULT_HEARTBEAT.into(),
        command: "/usr/bin/false".into(),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec!["mcp".into()],
        extra_args: vec![],
        run_timeout: Duration::from_secs(5),
        max_turns: 1,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        codex_enable: vec![],
        access: wats::access::Access::SwarmOnly,
        workdir: std::path::PathBuf::from("/nonexistent"),
        tools: vec![],
        servers: Vec::new(),
        mcp_format: Default::default(),
        mcp_file: None,
        env: Vec::new(),
        model: None,
        work_dir: data_dir.join("agent/helper"),
        data_dir: data_dir.to_path_buf(),
        keep_runs: 2,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    }
}
