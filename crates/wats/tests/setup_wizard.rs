//! The setup wizard of a bare `wats` (ADR 0027): run with no argument at
//! all on a terminal, for a data directory never set up, and the setup
//! revision. Hermetic: everything through WATS_* variables (an argument
//! would skip the wizard), WATS_INFRA=none and WATS_NO_DEFAULT_BOOTSTRAP, a
//! throwaway HOME, XDG directories, VIBE_HOME and registry, fake agent CLIs
//! on a PATH of their own (they record their arguments, nothing else);
//! `WATS_TEST_INTERACTIVE` stands for the terminal. The only nodes stopped
//! are those these tests started, in their throwaway directories.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A throwaway machine: home, registry, PATH of fake CLIs, data directory.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for dir in [
            "home",
            "bin",
            "vibe",
            "xdg/config",
            "xdg/data",
            "xdg/state",
            "xdg/cache",
        ] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        // `wats stop` signals with kill(1): the only real command on PATH.
        let kill = ["/bin/kill", "/usr/bin/kill"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .expect("kill(1)");
        std::os::unix::fs::symlink(kill, tmp.path().join("bin/kill")).unwrap();
        Machine { tmp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn data(&self) -> PathBuf {
        self.path("data")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.path("calls.log")).unwrap_or_default()
    }

    /// A fake agent CLI on this machine's PATH: it records its arguments
    /// and exits 0.
    fn fake_cli(&self, name: &str) {
        let path = self.path("bin").join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> {}\n",
                self.path("calls.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `wats` with this machine's environment and `args` (none: a bare
    /// `wats`); `terminal`: as if on one.
    fn wats(&self, args: &[&str], terminal: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("WATS_INFRA", "none")
            .env("WATS_NO_DEFAULT_BOOTSTRAP", "1")
            .env("WATS_DIR", self.data())
            .env("WATS_RUN_DIR", self.path("run"))
            .env("HOME", self.path("home"))
            .env("VIBE_HOME", self.path("vibe"))
            .env("XDG_CONFIG_HOME", self.path("xdg/config"))
            .env("XDG_DATA_HOME", self.path("xdg/data"))
            .env("XDG_STATE_HOME", self.path("xdg/state"))
            .env("XDG_CACHE_HOME", self.path("xdg/cache"))
            .env("PATH", self.path("bin"))
            .env("USER", "alice")
            // No desktop session: no notify.desktop question.
            .env("SSH_CONNECTION", "test")
            // A UTF-8 locale unless a test says otherwise.
            .env("LANG", "en_US.UTF-8")
            .env_remove("LC_ALL")
            .env_remove("LC_CTYPE")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("WATS_CLAUDE")
            .env_remove("WATS_CODEX")
            .env_remove("WATS_OPENCLAW")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_MCP_SOCKET")
            .env_remove("WATS_RUNNER_TOKEN")
            .env_remove("WATS_TEST_IGNORE_STOP")
            .env_remove("WATS_TEST_INTERACTIVE")
            .env_remove("RUST_LOG")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if terminal {
            command.env("WATS_TEST_INTERACTIVE", "1");
        }
        command
    }

    /// Start `command` with `input` on its stdin (then closed).
    fn spawn(&self, mut command: Command, input: &str) -> Child {
        command.stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child
    }

    fn ps(&self) -> Vec<Value> {
        let out = self.wats(&["ps", "--json"], false).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    /// The node running in the data directory, from `wats ps`.
    fn running(&self) -> Option<Value> {
        let data = self.data();
        self.ps()
            .into_iter()
            .find(|e| e["data_dir"].as_str() == Some(data.to_str().unwrap()))
    }

    fn stop(&self) {
        let out = self.wats(&["stop"], false).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    /// `child`, a foreground node: wait until `wats ps` lists it, stop it,
    /// and return its output and how `ps` listed it.
    fn foreground(&self, mut child: Child) -> (Output, Value) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let entry = loop {
            if let Some(entry) = self.running()
                && self.data().join("mcp.sock").exists()
            {
                break entry;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("exited: {status}: {:?}", child.wait_with_output());
            }
            assert!(Instant::now() < deadline, "the node is not up after 60 s");
            std::thread::sleep(Duration::from_millis(200));
        };
        self.stop();
        (child.wait_with_output().unwrap(), entry)
    }

    fn revision(&self) -> Option<String> {
        std::fs::read_to_string(self.data().join("setup-revision")).ok()
    }
}

impl Drop for Machine {
    /// No node outlives its test.
    fn drop(&mut self) {
        let _ = self
            .wats(&["stop", "--all", "--timeout", "2s"], false)
            .output();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

const BACKGROUND: &str = "Run the node in the background (`wats --detach`";
/// Step 3's question, Enter for the background (ADR 0027, 0.4.4).
const WHERE: &str = "[Y = background, recommended / d = choose]: ";
/// Step 1's quick or detailed question.
const MODE: &str = "Recommended settings, or choose each one? [quick/detailed, Enter: quick]: ";
/// Step 2's question.
const HOSTS: &str = "MCP server + session hook? [Y = all / d = one by one / n = none]: ";
/// The welcome screen, shown only by the wizard.
const WELCOME: &str = "Every node is the swarm.";
/// The line of a node's very first start in a data directory (stderr).
const FIRST_START: &str = "You are the swarm. Welcome: this is the first start of the node in ";

/// The first bare `wats` on a terminal: the main choices (detailed),
/// then each MCP host found on PATH one by one (Claude Code: registered
/// through its CLI, with the session hook in the throwaway HOME; Codex:
/// skipped; Vibe: its config.toml), then the background (Enter): a
/// detached node.
#[test]
fn first_bare_wats_runs_the_wizard_then_the_node_in_the_background() {
    let machine = Machine::new();
    for name in ["claude", "codex", "vibe"] {
        machine.fake_cli(name);
    }
    // No agent, detailed: the default disk budget and networks; security
    // updates: yes; one by one: Claude Code: yes, its hook: yes; Codex:
    // no; Vibe: yes (Enter); the background: Enter.
    let child = machine.spawn(machine.wats(&[], true), "0\nd\n\n\ny\nd\ny\ny\nn\n\n\n");
    let out = child.wait_with_output().unwrap();
    let (said, err) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{out:?}");
    // The welcome, then the three steps, then the closing screen.
    assert!(said.contains("You are the swarm."), "{said}");
    // The welcome greeted: the node's first start says nothing more.
    assert!(!err.contains(FIRST_START), "{err}");
    assert!(said.contains("Welcome to WeAreTheSwarm · wats "), "{said}");
    assert!(said.contains(WELCOME), "{said}");
    assert!(said.contains("This setup takes three steps:"), "{said}");
    assert!(said.contains("First run in "), "{said}");
    for header in [
        "── Step 1 of 3 · The node's main choices ──",
        "── Step 2 of 3 · Your own agent sessions ──",
        "── Step 3 of 3 · Where the node runs ──",
    ] {
        assert!(said.contains(header), "{header}: {said}");
    }
    assert!(
        !said.contains("wats setup: the node's main choices"),
        "{said}"
    );
    assert!(said.contains("All set."), "{said}");
    assert!(
        said.contains("sessions   Claude Code, Mistral Vibe"),
        "{said}"
    );
    assert!(said.contains("node       runs in the background"), "{said}");
    assert!(
        said.contains("from Claude Code    \"search the swarm for …\""),
        "{said}"
    );
    assert!(
        said.contains("Agent the node runs (agent.runtime) [claude]"),
        "{said}"
    );
    assert!(said.contains("(storage.max) [10G]"), "{said}");
    assert!(said.contains(MODE), "{said}");
    assert!(said.contains(HOSTS), "{said}");
    // ADR 0030: asked in the main choices, Yes by default.
    assert!(
        said.contains("Install security updates automatically? (update.security_auto) [Y/n]"),
        "{said}"
    );
    // The hints to run `wats setup <tool> --mcp` are questions here.
    assert!(
        !said.contains("wats setup vibe --mcp (running it"),
        "{said}"
    );
    assert!(
        said.contains(
            "Use the swarm from your own Claude Code sessions: register wats as its MCP server \
             (`wats setup claude --mcp`)? [Y/n]"
        ),
        "{said}"
    );
    assert!(
        said.contains("Claude Code: wats MCP server registered (profile alice)"),
        "{said}"
    );
    assert!(
        said.contains("Codex: skipped (later: wats setup codex --mcp)"),
        "{said}"
    );
    assert!(
        said.contains("Mistral Vibe: wats MCP server added to"),
        "{said}"
    );
    assert!(said.contains(BACKGROUND), "{said}");
    assert!(err.contains("running in the background · pid "), "{err}");
    let node = machine.running().expect("ps lists it");
    assert_eq!(node["mode"], "detached", "{node}");

    // What it wrote: config.toml (no agent), the revision, the client
    // configs of the throwaway HOME, nothing for Codex.
    let config = std::fs::read_to_string(machine.data().join("config.toml")).unwrap();
    assert!(config.contains("# [agents.claude]"), "{config}");
    assert!(config.contains("\nsecurity_auto = true\n"), "{config}");
    assert!(config.contains("\nsecurity_delay = \"1d\"\n"), "{config}");
    assert_eq!(machine.revision().as_deref(), Some("2\n"));
    let calls = machine.calls();
    assert!(
        calls.contains("claude mcp add --scope user wats -- "),
        "{calls}"
    );
    assert!(!calls.contains("codex mcp"), "{calls}");
    let hooks = std::fs::read_to_string(machine.path("home/.claude/settings.json")).unwrap();
    assert!(hooks.contains("notify --hook"), "{hooks}");
    let vibe = std::fs::read_to_string(machine.path("vibe/config.toml")).unwrap();
    assert!(vibe.contains("name = \"wats\""), "{vibe}");
    machine.stop();

    // Set up: the next bare `wats` asks nothing and runs the node.
    let child = machine.spawn(machine.wats(&[], true), "y\n");
    let (out, node) = machine.foreground(child);
    assert!(!text(&out.stdout).contains("]: "), "{out:?}");
    assert!(!text(&out.stdout).contains(WELCOME), "{out:?}");
    assert!(!text(&out.stderr).contains(FIRST_START), "{out:?}");
    assert_eq!(node["mode"], "foreground", "{node}");
}

/// Enter for every question: no agent CLI it can run (Vibe only: an MCP
/// host), quick, Vibe registered with the others (all), and the node in
/// the background, recommended (ADR 0027, 0.4.4). Not a UTF-8 locale, and
/// NO_COLOR: the screens in plain ASCII.
#[test]
fn wizard_defaults_run_the_node_in_the_background() {
    let machine = Machine::new();
    machine.fake_cli("vibe");
    let mut command = machine.wats(&[], true);
    command.env("LANG", "C").env("NO_COLOR", "1");
    let out = machine.spawn(command, "").wait_with_output().unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains("Welcome to WeAreTheSwarm - wats "), "{said}");
    assert!(
        said.contains("-- Step 2 of 3 - Your own agent sessions --"),
        "{said}"
    );
    assert!(said.contains("node       runs in the background"), "{said}");
    let screens = &said[..said.find("Agent CLIs found").unwrap()];
    assert!(screens.is_ascii() && !said.contains('\x1b'), "{said}");
    assert!(said.contains("No agent CLI found"), "{said}");
    assert!(said.contains(MODE), "{said}");
    // Quick: neither the disk budget nor the networks are asked.
    assert!(!said.contains("(storage.max)"), "{said}");
    assert!(
        said.contains("Set up wats in all the agent CLIs found (Mistral Vibe)"),
        "{said}"
    );
    assert!(
        said.contains("Mistral Vibe: wats MCP server added to"),
        "{said}"
    );
    assert!(said.contains(WHERE), "{said}");
    let node = machine.running().expect("ps lists it");
    assert_eq!(node["mode"], "detached", "{node}");
    assert_eq!(machine.revision().as_deref(), Some("2\n"));
    // Quick wrote the recommended security updates, nothing else set.
    let config = std::fs::read_to_string(machine.data().join("config.toml")).unwrap();
    assert!(config.contains("\nsecurity_auto = true\n"), "{config}");
    assert!(config.contains("# max = \"10G\""), "{config}");
    machine.stop();
}

/// Step 3, detailed: the node here, in this terminal, as a bare `wats`
/// ran before 0.4.4; or not at all for now.
#[test]
fn step_three_detailed_runs_the_node_here_or_not_now() {
    let machine = Machine::new();
    machine.fake_cli("vibe");
    // Quick (Enter), all (Enter), choose (d), here (2).
    let child = machine.spawn(machine.wats(&[], true), "\n\nd\n2\n");
    let (out, node) = machine.foreground(child);
    let said = text(&out.stdout);
    assert!(
        said.contains("  2) here, in this terminal, until Ctrl-C"),
        "{said}"
    );
    assert!(
        said.contains("node       starts in this terminal now"),
        "{said}"
    );
    assert_eq!(node["mode"], "foreground", "{node}");

    let later = Machine::new();
    let out = later
        .spawn(later.wats(&[], true), "\nd\n3\n")
        .wait_with_output()
        .unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("node       not running: start it with wats --dir "),
        "{said}"
    );
    assert!(later.running().is_none());
}

/// Step 1, quick or detailed, with an agent: quick asks only the budget
/// (how the agent is paid, then the daily line, edited here), detailed
/// every question; both write only what differs from the defaults. Step
/// 2, all at once: Claude Code and Codex registered with their session
/// hooks, no question each.
#[test]
fn quick_and_detailed_with_an_agent() {
    let machine = Machine::new();
    for name in ["claude", "codex"] {
        machine.fake_cli(name);
    }
    // Claude (Enter), quick (Enter), a subscription (Enter), the line
    // edited; all (Enter); not now (d, 3).
    let out = machine
        .spawn(machine.wats(&[], true), "\n\n\nmin=90 usd=1\n\nd\n3\n")
        .wait_with_output()
        .unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains(MODE), "{said}");
    assert!(said.contains("How is this agent paid? [1]: "), "{said}");
    assert!(
        said.contains(
            "Daily budget: 120 min, no USD limit, no LLM token limit (monthly: 30×). Enter to \
             accept"
        ),
        "{said}"
    );
    assert!(
        said.contains(
            "set it to what you're willing to give the swarm from your subscription, \
                       machine or API spend"
        ),
        "{said}"
    );
    assert!(
        said.contains("  daily budget: 90 min, $1, no LLM token limit"),
        "{said}"
    );
    assert!(!said.contains("(agent.timeout)"), "{said}");
    assert!(
        said.contains("Set up wats in all the agent CLIs found (Claude Code, Codex)"),
        "{said}"
    );
    // All: no question per host, nor per session hook.
    assert!(!said.contains("register wats as its MCP server"), "{said}");
    assert!(!said.contains("Add a session hook"), "{said}");
    assert!(
        said.contains("Claude Code: wats MCP server registered (profile alice)"),
        "{said}"
    );
    let calls = machine.calls();
    assert!(
        calls.contains("claude mcp add --scope user wats -- "),
        "{calls}"
    );
    assert!(calls.contains("codex mcp add wats -- "), "{calls}");
    let hooks = std::fs::read_to_string(machine.path("home/.claude/settings.json")).unwrap();
    assert!(hooks.contains("notify --hook"), "{hooks}");
    assert!(machine.running().is_none());
    let config = std::fs::read_to_string(machine.data().join("config.toml")).unwrap();
    let set: Vec<&str> = config
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['))
        .collect();
    assert_eq!(
        set,
        [
            "runtime = \"claude\"",
            "max_usd_per_day = 1.0",
            "max_minutes_per_day = 90",
            "security_auto = true",
            "security_delay = \"1d\""
        ],
        "{config}"
    );
    assert!(config.contains("# timeout = \"25m\""), "{config}");
    assert!(config.contains("# max_turns = 50"), "{config}");

    // `wats setup`, detailed: every question, its current value as the
    // default; Enter for each keeps the file as it is. Hosts: none; not
    // now.
    let before = std::fs::read_to_string(machine.data().join("config.toml")).unwrap();
    let out = machine
        .spawn(
            machine.wats(&["setup"], true),
            "\nd\n\n\n\n\n\n\n\n\nn\nn\n",
        )
        .wait_with_output()
        .unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains("(agent.timeout) [25m]"), "{said}");
    assert!(said.contains("(agent.max_turns) [50]"), "{said}");
    assert!(
        said.contains("Daily budget: 90 min, $1, no LLM token limit"),
        "{said}"
    );
    assert!(
        said.contains("Claude Code: skipped (later: wats setup claude --mcp)"),
        "{said}"
    );
    assert_eq!(
        std::fs::read_to_string(machine.data().join("config.toml")).unwrap(),
        before
    );
}

/// A config.toml of 0.4.3 that sets the old defaults (timeout 10m,
/// max_turns 10): `wats status` says so in one line, and so does the
/// node's start; `wats config unset` takes both keys; the file is never
/// rewritten by itself.
#[test]
fn old_defaults_are_noticed() {
    let machine = Machine::new();
    std::fs::create_dir_all(machine.data()).unwrap();
    let old = "[agents.claude]\nruntime = \"claude\"\nenabled = false\ntimeout = \"10m\"\n\
               max_turns = 10\n";
    std::fs::write(machine.data().join("config.toml"), old).unwrap();
    let notice = "config.toml sets agent.timeout = 10m and agent.max_turns = 10: the defaults of \
                  wats 0.4.3 and older; the defaults are now 25m and 50. To use them: wats setup, \
                  or wats config unset agent.timeout agent.max_turns";
    let out = machine
        .wats(
            &["--infra", "none", "--no-default-bootstrap", "status"],
            false,
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out.stdout).contains(&format!("old_defaults: {notice}")),
        "{out:?}"
    );
    // At the node's start, in the log.
    let child = machine.spawn(
        machine.wats(&["--infra", "none", "--no-default-bootstrap"], false),
        "",
    );
    let (out, _) = machine.foreground(child);
    assert!(text(&out.stderr).contains(notice), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(machine.data().join("config.toml")).unwrap(),
        old
    );
    let out = machine
        .wats(
            &[
                "--infra",
                "none",
                "--no-default-bootstrap",
                "config",
                "unset",
                "agent.timeout",
                "agent.max_turns",
            ],
            false,
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = machine
        .wats(
            &["--infra", "none", "--no-default-bootstrap", "status"],
            false,
        )
        .output()
        .unwrap();
    assert!(!text(&out.stdout).contains("old_defaults"), "{out:?}");
}

/// `wats setup` while a node runs (a throwaway one, started here): the
/// changed setup offers to restart it, which stops it and starts it again
/// in the background (Enter); `n` leaves it running, with the commands.
#[test]
fn setup_offers_to_restart_a_running_node() {
    let machine = Machine::new();
    let out = machine
        .wats(
            &["--infra", "none", "--no-default-bootstrap", "--detach"],
            false,
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let before = machine.running().expect("it runs");
    let pid = before["pid"].as_u64().unwrap();
    // No agent CLI: quick (Enter, which sets update.security_auto: a
    // change), then n: not restarted.
    let out = machine
        .spawn(machine.wats(&["setup"], true), "\nn\n")
        .wait_with_output()
        .unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains(&format!(
            "a wats node already runs here (pid {pid}, detached)"
        )),
        "{said}"
    );
    let flag = format!(" --dir {}", machine.data().display());
    assert!(
        said.contains(&format!(
            "Restart it now with this setup, in the background (wats stop{flag}; wats \
             --detach{flag})? [Y/n]"
        )),
        "{said}"
    );
    assert!(
        said.contains(&format!(
            "To apply this setup, restart it: wats stop{flag}; wats --detach{flag}"
        )),
        "{said}"
    );
    assert_eq!(machine.running().unwrap()["pid"].as_u64(), Some(pid));
    // Changed again (detailed: a disk budget of 20G), and Enter: restarted.
    let out = machine
        .spawn(machine.wats(&["setup"], true), "d\n20G\n\n\n\n")
        .wait_with_output()
        .unwrap();
    let (said, err) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains(&format!("stopped wats pid {pid} (detached")),
        "{said}"
    );
    assert!(err.contains("running in the background · pid "), "{err}");
    let after = machine.running().expect("it runs again");
    assert_ne!(after["pid"].as_u64(), Some(pid), "{after}");
    assert_eq!(after["mode"], "detached", "{after}");
    assert!(said.contains("node       runs in the background"), "{said}");
    machine.stop();
}

/// A node of a wats older than 0.3.3 holds the data directory: its lock
/// file names no role, and it answers no `Info`. `wats ps` says what it
/// is, `older wats, v0.3.2`, not `unknown`. (A lock held by this test for
/// a `sleep` it started: nothing else is signalled.)
#[test]
fn an_older_node_is_named_by_its_version() {
    let machine = Machine::new();
    std::fs::create_dir_all(machine.data()).unwrap();
    let mut sleeper = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(machine.data().join("lock"))
        .unwrap();
    lock.lock().unwrap();
    std::fs::write(
        machine.data().join("lock"),
        format!(
            "{{\"pid\":{},\"version\":\"0.3.2\",\"exe\":\"/old/wats\",\"started\":\"2026-01-01 \
             00:00:00Z\"}}",
            sleeper.id()
        ),
    )
    .unwrap();
    let out = machine.wats(&["ps"], false).output().unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains("older wats, v0.3.2"), "{said}");
    assert!(!said.contains("unknown"), "{said}");
    assert_eq!(machine.running().unwrap()["mode"], "older");
    drop(lock);
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

/// Without a terminal, a bare `wats` does what it always did: the defaults
/// written, commented, no question, no client touched, the node.
#[test]
fn bare_wats_without_a_terminal_runs_the_node_as_before() {
    let machine = Machine::new();
    machine.fake_cli("claude");
    let child = machine.spawn(machine.wats(&[], false), "y\ny\ny\n");
    let (out, node) = machine.foreground(child);
    let said = text(&out.stdout);
    assert!(!said.contains("]: "), "{said}");
    assert!(!said.contains(WELCOME), "{said}");
    assert!(
        text(&out.stderr).contains("first run: wrote the default config"),
        "{out:?}"
    );
    assert_eq!(node["mode"], "foreground", "{node}");
    assert!(machine.calls().is_empty(), "{}", machine.calls());
    // Defaults: no agent, and no revision (never asked).
    let config = std::fs::read_to_string(machine.data().join("config.toml")).unwrap();
    assert!(config.contains("# [agents.claude]"), "{config}");
    // Not asked, so update.security_auto is not set: off (ADR 0030).
    assert!(!config.contains("security_auto"), "{config}");
    assert_eq!(machine.revision(), None);
    // The very first start of a node here: the headline, on stderr, once.
    assert!(text(&out.stderr).contains(FIRST_START), "{out:?}");
    let again = machine
        .foreground(machine.spawn(machine.wats(&[], false), ""))
        .0;
    assert!(!text(&again.stderr).contains(FIRST_START), "{again:?}");
    // A node that ran before 0.4.3 (a store, no marker): not greeted.
    std::fs::remove_file(machine.data().join("greeted")).unwrap();
    let upgraded = machine
        .foreground(machine.spawn(machine.wats(&[], false), ""))
        .0;
    assert!(
        !text(&upgraded.stderr).contains(FIRST_START),
        "{upgraded:?}"
    );
    assert!(machine.data().join("greeted").exists());
}

/// Any argument keeps today's `wats`, even on a terminal: here (hermetic)
/// the defaults written and the node, no wizard.
#[test]
fn any_argument_skips_the_wizard() {
    let machine = Machine::new();
    machine.fake_cli("vibe");
    let child = machine.spawn(machine.wats(&["--infra", "none"], true), "y\ny\ny\n");
    let (out, _) = machine.foreground(child);
    assert!(!text(&out.stdout).contains("]: "), "{out:?}");
    assert!(!text(&out.stdout).contains(WELCOME), "{out:?}");
    assert!(text(&out.stderr).contains(FIRST_START), "{out:?}");
    assert!(machine.calls().is_empty(), "{}", machine.calls());
    assert!(!machine.path("vibe/config.toml").exists());
}

/// A data directory set up by 0.4.2 or older (config.toml, no revision
/// recorded) is set up: a bare `wats` on a terminal asks nothing (without
/// an agent, the budget question of revision 2 has nothing to ask: the
/// revision is recorded as is).
#[test]
fn a_configured_data_dir_without_a_revision_is_not_asked_again() {
    let machine = Machine::new();
    std::fs::create_dir_all(machine.data()).unwrap();
    std::fs::write(machine.data().join("config.toml"), "nets = [\"public\"]\n").unwrap();
    let child = machine.spawn(machine.wats(&[], true), "20G\n");
    let (out, _) = machine.foreground(child);
    assert!(!text(&out.stdout).contains("]: "), "{out:?}");
    assert!(!text(&out.stderr).contains("new question"), "{out:?}");
    assert!(!text(&out.stdout).contains(WELCOME), "{out:?}");
    assert_eq!(machine.revision().as_deref(), Some("2\n"));
}

/// A data directory set up at an older revision (0 here: older than every
/// question of today): a bare `wats` on a terminal asks the questions
/// added since, not the MCP hosts nor the background, records the
/// revision and runs the node; without a terminal, or with an argument,
/// one log line says so and nothing is asked.
#[test]
fn an_older_revision_asks_only_the_new_questions() {
    let machine = Machine::new();
    machine.fake_cli("vibe");
    std::fs::create_dir_all(machine.data()).unwrap();
    std::fs::write(machine.data().join("config.toml"), "nets = [\"public\"]\n").unwrap();
    let older = |machine: &Machine| {
        std::fs::write(machine.data().join("setup-revision"), "0\n").unwrap();
    };
    let hint = "`wats setup` has 5 new question(s) since this data directory was set up";
    for (args, terminal) in [(&[][..], false), (&["--infra", "none"][..], true)] {
        older(&machine);
        let child = machine.spawn(machine.wats(args, terminal), "20G\n");
        let (out, _) = machine.foreground(child);
        assert!(!text(&out.stdout).contains("]: "), "{args:?}: {out:?}");
        assert!(!text(&out.stdout).contains(WELCOME), "{args:?}: {out:?}");
        assert!(text(&out.stderr).contains(hint), "{args:?}: {out:?}");
        assert_eq!(machine.revision().as_deref(), Some("0\n"));
    }

    let child = machine.spawn(machine.wats(&[], true), "20G\n\n");
    let (out, node) = machine.foreground(child);
    let said = text(&out.stdout);
    assert!(
        said.contains("wats setup has new questions since this data directory was set up"),
        "{said}"
    );
    assert!(said.contains("(storage.max) [10G]"), "{said}");
    assert!(!said.contains("Mistral Vibe"), "{said}");
    // A one-line header, not the welcome.
    assert!(
        !said.contains(WELCOME) && !said.contains("All set."),
        "{said}"
    );
    assert!(!said.contains(BACKGROUND), "{said}");
    assert_eq!(node["mode"], "foreground", "{node}");
    assert_eq!(machine.revision().as_deref(), Some("2\n"));
    let config = std::fs::read_to_string(machine.data().join("config.toml")).unwrap();
    assert!(config.contains("max = \"20G\""), "{config}");
    assert!(!machine.path("vibe/config.toml").exists());
}

/// `wats setup` with no tool on a terminal is the wizard too: the main
/// choices, the MCP hosts found, then the background offered (Enter:
/// yes; `n`: not now).
#[test]
fn wats_setup_with_no_tool_is_the_wizard() {
    let machine = Machine::new();
    machine.fake_cli("vibe");
    let out = machine
        .spawn(machine.wats(&["setup"], true), "\n\nn\n")
        .wait_with_output()
        .unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(said.contains(WELCOME), "{said}");
    assert!(
        said.contains("3. The node: whether to start it now, in the background."),
        "{said}"
    );
    assert!(!said.contains("First run in "), "{said}");
    assert!(
        said.contains("── Step 1 of 3 · The node's main choices ──"),
        "{said}"
    );
    assert!(
        said.contains("Mistral Vibe: wats MCP server added to"),
        "{said}"
    );
    assert!(said.contains(WHERE), "{said}");
    assert!(said.contains("All set."), "{said}");
    assert!(
        said.contains("node       not running: start it with wats --dir "),
        "{said}"
    );
    assert!(machine.running().is_none());
    assert_eq!(machine.revision().as_deref(), Some("2\n"));
    // Without a terminal: the main choices only, and the hint; no screens.
    std::fs::remove_file(machine.path("vibe/config.toml")).unwrap();
    let out = machine.wats(&["setup"], false).output().unwrap();
    let said = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("wats setup: the node's main choices"),
        "{said}"
    );
    assert!(said.contains("wats setup vibe --mcp (running it"), "{said}");
    assert!(
        !said.contains(WELCOME) && !said.contains("All set."),
        "{said}"
    );
    assert!(!machine.path("vibe/config.toml").exists());
}
