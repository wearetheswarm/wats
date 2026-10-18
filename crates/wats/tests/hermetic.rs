//! `--hermetic` / `WATS_HERMETIC`: tests that can never reach the public
//! swarm, whatever config.toml says.

#![cfg(unix)]

use std::time::Duration;

fn wats(data: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env_remove("WATS_INFRA")
        .env_remove("WATS_BOOTSTRAP")
        .env_remove("WATS_PEERS")
        .arg("--dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn hermetic_refuses_remote_bootstraps() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    let router = "9132c3e3e9a3313114adecc9a45fba05721ec2125fcd3f51494a376acdd603d8";
    std::fs::write(
        data.join("config.toml"),
        format!("infra = \"wats\"\nbootstrap = [\"{router}@router.wearetheswarm.ai:7842\"]\n"),
    )
    .unwrap();
    let out = wats(&data, &["id"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--hermetic: bootstrap router.wearetheswarm.ai"),
        "{err}"
    );
    // `wats config` still works, to fix the file.
    assert!(
        wats(&data, &["config", "unset", "bootstrap"])
            .status
            .success()
    );
    std::fs::write(
        data.join("config.toml"),
        format!("bootstrap = [\"{router}@127.0.0.1:7842\"]\n"),
    )
    .unwrap();
    assert!(wats(&data, &["id"]).status.success());
}

#[test]
fn hermetic_daemon_is_loopback_only() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    // Even if the file asks for the public infrastructure.
    std::fs::write(data.join("config.toml"), "infra = \"wats\"\n").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .arg("--dir")
        .arg(&data)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("infra none") && err.contains("LOOPBACK ONLY"),
        "{err}"
    );
    assert!(
        !err.contains("bootstrap peer"),
        "no default bootstrap: {err}"
    );
}

#[test]
fn hermetic_never_looks_for_releases() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    for args in [&["upgrade", "--check"][..], &["upgrade"][..]] {
        let out = wats(&data, args);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("--hermetic: wats upgrade never checks or downloads anything"),
            "{err}"
        );
    }
    assert!(!data.join("update.json").exists());
}

/// A fake agent CLI: never run by these tests, only found on PATH.
fn fake_cli(dir: &std::path::Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// First run without a terminal (a service, a container): the node writes
/// the default config, commented, and starts; it runs no agent, which its
/// banner and `wats status` say.
#[test]
fn first_run_without_a_terminal_writes_the_defaults() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("HOME", dir.path())
        .arg("--dir")
        .arg(&data)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Read to the end: a closed pipe would end the node.
        let mut seen = String::new();
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            seen.push_str(&line);
            seen.push('\n');
            if line.starts_with("Ctrl-C to stop") {
                let _ = tx.send(std::mem::take(&mut seen));
            }
        }
        let _ = tx.send(seen);
    });
    let banner = rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
    // The daemon's `status`, once its socket answers.
    let mut status = String::new();
    for _ in 0..50 {
        status = String::from_utf8_lossy(&wats(&data, &["status"]).stdout).into_owned();
        if status.starts_with("node: running\n") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    child.kill().unwrap();
    let _ = child.wait();
    assert!(
        banner.contains("first run: wrote the default config to"),
        "{banner}"
    );
    assert!(
        banner.contains("agent: off · This node runs no agent"),
        "{banner}"
    );
    let text = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(text.contains("# nets = [\"public\"]"), "{text}");
    assert!(text.contains("# max = \"10G\""), "{text}");
    assert!(text.contains("# runtime = \"claude\""), "{text}");
    assert!(status.starts_with("node: running\n"), "{status}");
    assert!(
        status.contains("notice: This node runs no agent: nodes that don't contribute"),
        "{status}"
    );
    // Not running: `wats status` says what `wats` would run.
    let out = wats(&data, &["status"]);
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(status.starts_with("node: not running\n"), "{status}");
    assert!(
        status.contains("notice: This node runs no agent"),
        "{status}"
    );
}

/// `wats mcp` (and the background node it starts) never writes config.toml:
/// MCP-only users keep a data directory without one.
#[test]
fn mcp_never_writes_a_config() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    for args in [
        &["mcp", "--idle-exit", "1"][..],
        &["mcp", "--in-process"][..],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path())
            .env_remove("WATS_RUNNER_TOKEN")
            .arg("--dir")
            .arg(&data)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        // --in-process fails on a closed stdin (no MCP initialize).
        assert!(
            out.status.success() || args[1] == "--in-process",
            "{args:?}: {out:?}"
        );
        // The background node, until it exits on idle.
        for _ in 0..50 {
            if !data.join("mcp.sock").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    assert!(data.exists());
    assert!(!data.join("config.toml").exists());
}

/// `wats setup` lists the agent CLIs found on PATH (openclaw: not supported
/// yet), Enter takes the defaults; with none found it says how to get one.
#[test]
fn setup_detects_agent_clis_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["codex", "openclaw"] {
        fake_cli(&bin, name);
    }
    let setup = |data: &std::path::Path, path: &std::path::Path| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path())
            .env("PATH", path)
            .arg("--dir")
            .arg(data)
            .arg("setup")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let data = dir.path().join("d");
    let said = setup(&data, &bin);
    assert!(said.contains("1) codex"), "{said}");
    assert!(said.contains("2) openclaw"), "{said}");
    assert!(!said.contains("not supported yet"), "{said}");
    assert!(said.contains("wats setup openclaw --mcp"), "{said}");
    assert!(!said.contains("claude "), "{said}");
    assert!(
        said.contains("Agent the node runs (agent.runtime) [codex]"),
        "{said}"
    );
    let text = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(text.contains("\nruntime = \"codex\"\n"), "{text}");
    // Nothing found.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let said = setup(&dir.path().join("e"), &empty);
    assert!(
        said.contains(
            "No agent CLI found: install Claude Code (https://claude.com/claude-code) or Codex"
        ),
        "{said}"
    );
    assert!(said.contains("This node runs no agent"), "{said}");
}

/// `wats setup openclaw`: through OpenClaw's CLI only (a fake `openclaw`
/// on PATH that records its arguments and keeps the entry in a file).
/// `--mcp` sends `mcp set wats <json>` then `mcp show wats`, again on a
/// second run; `--remove` sends `mcp unset wats`; no flag points to
/// `--mcp`; a missing CLI gets the install hint; a failing `mcp set`
/// gives its stderr and a non-zero exit.
#[test]
fn setup_openclaw_mcp_through_its_cli() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.path().join("calls.log");
    let entry = dir.path().join("entry.json");
    let fail = dir.path().join("fail");
    let fake = bin.join("openclaw");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" >> {log}\necho --- >> {log}\n\
             case \"$2\" in\n\
             set) if [ -f {fail} ]; then echo 'Error: invalid MCP server config' >&2; exit 3; fi\n\
                  printf '%s\\n' \"$4\" > {entry};;\n\
             show) [ -f {entry} ] || {{ echo 'No MCP server named wats' >&2; exit 1; }}; cat {entry};;\n\
             unset) rm -f {entry};;\n\
             esac\n",
            log = log.display(),
            entry = entry.display(),
            fail = fail.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = dir.path().join("data");
    let path_with = format!("{}:/bin:/usr/bin", bin.display());
    let setup = |path: &str, args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path())
            .env("PATH", path)
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_OPENCLAW")
            .arg("--dir")
            .arg(&data)
            .arg("setup")
            .arg("openclaw")
            .args(args)
            .output()
            .unwrap()
    };
    let calls = || -> Vec<Vec<String>> {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .split("---\n")
            .filter(|c| !c.is_empty())
            .map(|c| c.lines().map(str::to_string).collect())
            .collect()
    };
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    let expected = serde_json::json!({
        "command": exe,
        "args": ["--dir", data, "mcp", "--profile", "alice"],
    });

    let out = setup(&path_with, &["--mcp"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("OpenClaw: wats MCP server set (profile alice)"),
        "{said}"
    );
    assert!(said.contains("checked: `openclaw mcp show wats`"), "{said}");
    let first = calls();
    assert_eq!(first.len(), 2, "{first:?}");
    assert_eq!(first[0][..3], ["mcp", "set", "wats"], "{first:?}");
    assert_eq!(first[0].len(), 4, "{first:?}");
    let sent: serde_json::Value = serde_json::from_str(&first[0][3]).unwrap();
    assert_eq!(sent, expected);
    assert_eq!(first[1], ["mcp", "show", "wats"]);

    // Again: the entry is set again (OpenClaw replaces it).
    let out = setup(&path_with, &["--mcp"]);
    assert!(out.status.success(), "{out:?}");
    let again = calls();
    assert_eq!(again.len(), 4, "{again:?}");
    assert_eq!(again[2], first[0]);
    let kept: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
    assert_eq!(kept, expected);

    // --remove.
    let out = setup(&path_with, &["--remove"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("removed"));
    assert_eq!(calls().last().unwrap(), &["mcp", "unset", "wats"]);
    assert!(!entry.exists());

    // No flag means both halves: without a terminal, the agent half wants
    // --yes for the spend caps, and nothing is run before.
    let before = calls().len();
    let out = setup(&path_with, &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("--yes"), "{err}");
    assert_eq!(calls().len(), before);
    assert!(!data.join("openclaw").exists());

    // Not on PATH.
    let out = setup("/bin:/usr/bin", &["--mcp"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        err.contains("OpenClaw CLI not found") && err.contains("https://docs.openclaw.ai"),
        "{err}"
    );

    // `mcp set` fails: its stderr, a non-zero exit, no `mcp show`.
    std::fs::write(&fail, "").unwrap();
    let before = calls().len();
    let out = setup(&path_with, &["--mcp"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("`openclaw mcp set wats` failed"), "{err}");
    assert!(err.contains("Error: invalid MCP server config"), "{err}");
    let after = calls();
    assert_eq!(after.len(), before + 1, "{after:?}");
}

/// `wats setup openclaw --agent --yes`: the hook and runner tokens (0600,
/// kept on a second run), the `wats` and `wats-agent` MCP entries through
/// a fake `openclaw` (the agent's with the runner token in its env), the
/// JSON5 block with the token as an environment reference, the token
/// printed once on its `export` line, `openclaw config validate`, and
/// config.toml. A non-loopback http URL and `inherit` without allow and
/// review are refused before anything runs. `wats status` then says the
/// budget is OpenClaw's.
#[test]
fn setup_openclaw_agent_tokens_entries_and_block() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.path().join("calls.log");
    let fake = bin.join("openclaw");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" >> {log}\necho --- >> {log}\n\
             case \"$2\" in\n\
             set) printf '%s\\n' \"$4\" > {dir}/entry-$3.json;;\n\
             show) cat {dir}/entry-$3.json;;\n\
             esac\n",
            log = log.display(),
            dir = dir.path().display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = dir.path().join("data");
    let path = format!("{}:/bin:/usr/bin", bin.display());
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path())
            .env("PATH", &path)
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_OPENCLAW")
            .arg("--dir")
            .arg(&data)
            .args(args)
            .output()
            .unwrap()
    };
    let calls = || -> Vec<Vec<String>> {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .split("---\n")
            .filter(|c| !c.is_empty())
            .map(|c| c.lines().map(str::to_string).collect())
            .collect()
    };

    // Refused before anything runs.
    let out = run(&[
        "setup",
        "openclaw",
        "--agent",
        "--yes",
        "--url",
        "http://192.168.1.2:18789",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("loopback"),
        "{out:?}"
    );
    assert!(calls().is_empty() && !data.join("openclaw").exists());

    let out = run(&["setup", "openclaw", "--agent", "--yes"]);
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{out:?}");
    let token = |name: &str| {
        let file = data.join("openclaw").join(name);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
        let token = std::fs::read_to_string(&file).unwrap().trim().to_string();
        assert_eq!(token.len(), 64, "{name}");
        token
    };
    let (hooks, runner) = (token("hooks.token"), token("runner.token"));
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    let all = calls();
    assert_eq!(all.len(), 5, "{all:?}");
    assert_eq!(all[0][..3], ["mcp", "set", "wats"]);
    assert_eq!(all[1], ["mcp", "show", "wats"]);
    assert_eq!(all[2][..3], ["mcp", "set", "wats-agent"]);
    let agent: serde_json::Value = serde_json::from_str(&all[2][3]).unwrap();
    assert_eq!(
        agent,
        serde_json::json!({
            "command": exe,
            "args": ["--dir", data, "mcp", "--profile", "alice-agent"],
            "env": {"WATS_RUNNER_TOKEN": runner},
        })
    );
    assert_eq!(all[3], ["mcp", "show", "wats-agent"]);
    assert_eq!(all[4], ["config", "validate"]);
    assert!(
        said.contains("token: \"${WATS_OPENCLAW_HOOKS_TOKEN}\""),
        "{said}"
    );
    assert!(said.contains("allowedAgentIds: [\"wats\"]"), "{said}");
    assert!(said.contains("allowRequestSessionKey: true"), "{said}");
    assert!(
        said.contains("allowedSessionKeyPrefixes: [\"wats:\"]"),
        "{said}"
    );
    assert!(said.contains("allow: [\"wats-agent__*\"]"), "{said}");
    assert!(said.contains("\"exec\", \"process\", \"write\""), "{said}");
    assert!(said.contains("Spend caps belong"), "{said}");
    assert_eq!(said.matches(&hooks).count(), 1, "{said}");
    assert!(
        said.contains(&format!("\nexport WATS_OPENCLAW_HOOKS_TOKEN={hooks}\n")),
        "{said}"
    );
    assert!(!said.contains(&runner), "{said}");
    let config = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(config.contains("runtime = \"openclaw\""), "{config}");
    assert!(
        config.contains("openclaw_url = \"http://127.0.0.1:18789\""),
        "{config}"
    );

    // Again: the same tokens.
    let out = run(&["setup", "openclaw", "--agent", "--yes"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        (token("hooks.token"), token("runner.token")),
        (hooks, runner)
    );

    // Not running: `wats status` shows who manages the budget.
    let status = String::from_utf8_lossy(&run(&["status"]).stdout).into_owned();
    assert!(status.contains("budget: managed by OpenClaw"), "{status}");

    // inherit hands the swarm to the user's own agent: allow and review
    // first.
    let set = run(&["config", "set", "agent.access", "inherit"]);
    assert!(set.status.success(), "{set:?}");
    let before = calls().len();
    let out = run(&["setup", "openclaw", "--agent", "--yes"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("agent.review"),
        "{out:?}"
    );
    assert_eq!(calls().len(), before);
}
