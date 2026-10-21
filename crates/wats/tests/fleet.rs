//! `wats fleet` (ADR 0028): several nodes on one machine, started,
//! checked and stopped together. Hermetic: loopback only, no
//! infrastructure, a throwaway home and registry, a fake agent CLI.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

/// A throwaway home, registry, template and fleet directory.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        Machine { tmp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn base(&self) -> PathBuf {
        self.path("fleet")
    }

    /// `wats` with this machine's home and registry.
    fn wats(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("HOME", self.path("home"))
            .env("WATS_RUN_DIR", self.path("run"))
            .env_remove("WATS_DIR")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_PORT")
            .env_remove("WATS_BOOTSTRAP")
            .env_remove("WATS_FLEET_BASE")
            .env_remove("WATS_MCP_SOCKET")
            .env_remove("WATS_AGENT_COMMAND")
            .env_remove("WATS_RUNNER_TOKEN")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        command
    }

    /// `wats --infra none … fleet --base <base> <args>`.
    fn fleet(&self, args: &[&str]) -> Output {
        self.wats()
            .args([
                "--infra",
                "none",
                "--pow-bits=0",
                "--min-pow-bits=0",
                "--dir",
            ])
            .arg(self.path("template"))
            .arg("fleet")
            .arg("--base")
            .arg(self.base())
            .args(args)
            .output()
            .unwrap()
    }

    fn ps(&self) -> Vec<Value> {
        let out = self.wats().args(["ps", "--json"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    /// Every node's log, for a failure message.
    fn logs(&self) -> String {
        let mut text = String::new();
        for i in 1..=3 {
            let log = self.base().join(format!("node-{i}/logs/node.log"));
            text.push_str(&format!(
                "--- node-{i}\n{}\n",
                std::fs::read_to_string(&log).unwrap_or_default()
            ));
        }
        text
    }
}

impl Drop for Machine {
    /// No node outlives its test, even when it fails.
    fn drop(&mut self) {
        let _ = self.fleet(&["stop", "--timeout", "2s"]);
        let _ = self
            .wats()
            .args(["stop", "--all", "--timeout", "2s"])
            .output();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// An agent CLI that answers every run at once, as a successful one.
fn fake_agent(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-claude");
    std::fs::write(
        &script,
        "#!/bin/sh\n[ \"$1\" = mcp ] && exit 0\ncat > /dev/null\necho \
         '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\
         \"total_cost_usd\":0.0}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn up_check_logs_restart_and_stop_a_fleet_of_three() {
    let machine = Machine::new();
    let template = machine.path("template");
    std::fs::create_dir_all(&template).unwrap();
    let agent = fake_agent(machine.tmp.path());
    std::fs::write(
        template.join("config.toml"),
        format!(
            "# the template\n[agents.claude]\nruntime = \"claude\"\ncommand = \"{}\"\n\
             heartbeat = \"1h\"\nworkdir = \"/nonexistent/shared\"\n",
            agent.display()
        ),
    )
    .unwrap();

    let out = machine.fleet(&[
        "up",
        "-n",
        "3",
        "--stagger",
        "0s",
        "--storage",
        "1G",
        "--check",
        "--wait",
        "40s",
    ]);
    let (said, err) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{said}\n{err}\n{}", machine.logs());
    for i in 1..=3 {
        assert!(said.contains(&format!("node-{i}  started  pid ")), "{said}");
    }
    assert!(said.contains("3 of 3 node(s) ok"), "{said}");
    assert!(said.contains("met 2/2"), "{said}");
    assert!(said.contains("1/1 (claude)"), "{said}");
    // The shared workdir is replaced by one per node.
    assert!(
        err.contains("agents.claude.workdir is one directory"),
        "{err}"
    );

    // Each node: its own key, port and workspace; the others as peers.
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(machine.base().join("fleet.json")).unwrap())
            .unwrap();
    let nodes = manifest["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    let ids: std::collections::HashSet<&str> = nodes
        .iter()
        .map(|n| n["endpoint_id"].as_str().unwrap())
        .collect();
    let ports: std::collections::HashSet<u64> =
        nodes.iter().map(|n| n["port"].as_u64().unwrap()).collect();
    assert_eq!((ids.len(), ports.len()), (3, 3), "{manifest}");
    assert!(
        manifest["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--infra=none"),
        "{manifest}"
    );
    for i in 1..=3 {
        let dir = machine.base().join(format!("node-{i}"));
        let config = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(config.contains("# the template"), "{config}");
        assert!(!config.contains("workdir"), "{config}");
        assert!(config.contains("max = \"1G\""), "{config}");
        assert_eq!(config.matches("@127.0.0.1:").count(), 2, "{config}");
        assert!(dir.join("secret.key").exists());
    }
    // The template keeps its key to itself: it has none.
    assert!(!template.join("secret.key").exists());

    // `wats ps` lists them like any other node.
    let listed = machine.ps();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert!(listed.iter().all(|n| n["mode"] == "detached"), "{listed:?}");

    // `check --json`, as cron would read it.
    let out = machine.fleet(&["check", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], true, "{report}");
    for node in report["nodes"].as_array().unwrap() {
        assert_eq!(node["met"], 2, "{node}");
        assert!(node["neighbors"].as_u64().unwrap() >= 1, "{node}");
        assert_eq!(node["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(node["agents_running"][0], "claude", "{node}");
    }

    // Logs, each line prefixed with its node.
    let out = machine.fleet(&["logs", "-n", "3"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    for i in 1..=3 {
        assert!(said.contains(&format!("node-{i} | ")), "{said}");
    }
    let out = machine.fleet(&["logs", "--node", "2", "-n", "1"]);
    let said = text(&out.stdout);
    assert!(
        said.contains("node-2 | ") && !said.contains("node-1 | "),
        "{said}"
    );

    // One node down: the check fails, with a non-zero exit code.
    let node2 = machine.base().join("node-2");
    let out = machine
        .wats()
        .arg("--dir")
        .arg(&node2)
        .args(["stop"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = machine.fleet(&["check"]);
    let said = text(&out.stdout);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("node-2: not running"), "{said}");
    assert!(said.contains("2 of 3 node(s) ok"), "{said}");

    // `up` again starts it, and only it; the mesh comes back.
    let out = machine.fleet(&["up", "--stagger", "0s", "--check", "--wait", "40s"]);
    let said = text(&out.stdout);
    assert!(
        out.status.success(),
        "{said}\n{}\n{}",
        text(&out.stderr),
        machine.logs()
    );
    assert!(said.contains("node-1  already running"), "{said}");
    assert!(said.contains("node-2  started"), "{said}");
    assert!(said.contains("3 of 3 node(s) ok"), "{said}");

    // Stop them all.
    let out = machine.fleet(&["stop"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(said.matches("stopped wats pid").count(), 3, "{said}");
    assert!(machine.ps().is_empty());
    let out = machine.fleet(&["stop"]);
    assert!(text(&out.stdout).contains("node-1  not running"), "{out:?}");
}

#[test]
fn fleet_refusals() {
    let machine = Machine::new();
    // No fleet yet.
    let out = machine.fleet(&["check"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("no fleet in"), "{out:?}");
    let out = machine.fleet(&["up"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("how many nodes?"), "{out:?}");
    // One port, one key: each node has its own.
    let out = machine
        .wats()
        .args(["--port", "4000", "fleet", "--base"])
        .arg(machine.base())
        .args(["up", "-n", "2"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--port-base"), "{out:?}");
    // The fleet inside its template.
    let out = machine
        .wats()
        .arg("--dir")
        .arg(machine.tmp.path())
        .args(["fleet", "--base"])
        .arg(machine.base())
        .args(["up", "-n", "2"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("must not contain each other"),
        "{out:?}"
    );
    assert!(!machine.base().exists());
}
