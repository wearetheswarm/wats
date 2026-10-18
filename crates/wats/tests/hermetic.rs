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
    // No terminal and neither --apply nor --print: the block is printed,
    // and --apply is pointed to.
    assert!(said.contains("`--apply` applies this block"), "{said}");
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

/// `wats setup openclaw --agent --apply`, against a fake `openclaw` that
/// keeps a fake openclaw.json: a backup of that file, the existing hooks
/// lists merged with ours, the SecretRef `config set` for the hook token,
/// the same patch on stdin to the dry run then the real `config patch`,
/// then `config validate` (with the token in its environment). A failing
/// validate restores the file byte for byte and fails. Without SecretRefs,
/// the patch carries the `${VAR}` reference. `--print` patches nothing.
#[test]
fn setup_openclaw_agent_applies_through_config_patch() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let bin = d.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(d.join("oc")).unwrap();
    let log = d.join("calls.log");
    let cfg = d.join("oc").join("openclaw.json");
    let fake = bin.join("openclaw");
    let script = r#"#!/bin/sh
printf '%s\n' "$@" >> {d}/calls.log
echo --- >> {d}/calls.log
case "$1 $2" in
'mcp set') printf '%s\n' "$4" > {d}/entry-$3.json;;
'mcp show') cat {d}/entry-$3.json;;
'config file') echo '~/oc/openclaw.json';;
'config get') if [ -f "{d}/get-$3" ]; then cat "{d}/get-$3"; else echo "Config path not found: $3" >&2; exit 1; fi;;
'config set') if [ "$3" = --help ]; then [ -f {d}/norefs ] || echo '  --ref-source <source>  SecretRef source'; else echo "// set $3" >> {d}/oc/openclaw.json; fi;;
'config patch') if [ "$4" = --dry-run ]; then cat > {d}/dry.json; else cat > {d}/real.json; echo '// patched' >> {d}/oc/openclaw.json; fi;;
'config validate') if [ -f {d}/invalid ]; then echo 'Invalid config: hooks.token' >&2; exit 1; fi; [ -n "$WATS_OPENCLAW_HOOKS_TOKEN" ] || exit 2;;
esac
"#;
    std::fs::write(&fake, script.replace("{d}", &d.display().to_string())).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let original = "{\n  // the user's\n  hooks: { allowedAgentIds: [\"main\"] },\n}\n";
    std::fs::write(&cfg, original).unwrap();
    std::fs::write(d.join("get-hooks.allowedAgentIds"), "[\n  \"main\"\n]\n").unwrap();
    let data = d.join("data");
    let path = format!("{}:/bin:/usr/bin", bin.display());
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", d)
            .env("PATH", &path)
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_OPENCLAW")
            .env_remove("WATS_OPENCLAW_HOOKS_TOKEN")
            .arg("--dir")
            .arg(&data)
            .args(["setup", "openclaw", "--agent", "--yes"])
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
    let json = |name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(d.join(name)).unwrap()).unwrap()
    };
    let backups = || -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(d.join("oc"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("openclaw.json.bak-"))
            .collect();
        found.sort();
        found
    };

    let out = run(&["--apply"]);
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{out:?}");
    let all = calls();
    let config: Vec<&[String]> = all[4..].iter().map(Vec::as_slice).collect();
    assert_eq!(
        config,
        [
            &["config", "file"][..],
            &["config", "get", "hooks.allowedAgentIds"],
            &["config", "get", "hooks.allowedSessionKeyPrefixes"],
            &["config", "set", "--help"],
            &[
                "config",
                "set",
                "hooks.token",
                "--ref-source",
                "env",
                "--ref-id",
                "WATS_OPENCLAW_HOOKS_TOKEN"
            ],
            &["config", "patch", "--stdin", "--dry-run"],
            &["config", "patch", "--stdin"],
            &["config", "validate"],
        ],
        "{all:?}"
    );
    // The backup: the file as it was, and said.
    let backup = backups();
    assert_eq!(backup.len(), 1, "{backup:?}");
    assert_eq!(std::fs::read_to_string(&backup[0]).unwrap(), original);
    assert!(said.contains(&backup[0].display().to_string()), "{said}");
    // The same patch to the dry run and the real one, shown first.
    let patch = json("real.json");
    assert_eq!(json("dry.json"), patch);
    assert_eq!(
        patch["hooks"]["allowedAgentIds"],
        serde_json::json!(["main", "wats"])
    );
    assert_eq!(
        patch["hooks"]["allowedSessionKeyPrefixes"],
        serde_json::json!(["wats:"])
    );
    assert_eq!(patch["hooks"]["enabled"], true);
    assert!(patch["hooks"].get("token").is_none(), "{patch}");
    assert_eq!(
        patch["agents"]["entries"]["wats"]["tools"]["allow"],
        serde_json::json!(["wats-agent__*"])
    );
    assert!(said.contains("\"allowedAgentIds\": ["), "{said}");
    assert!(said.contains("applied"), "{said}");
    let hooks = std::fs::read_to_string(data.join("openclaw/hooks.token")).unwrap();
    assert!(
        said.contains(&format!(
            "export WATS_OPENCLAW_HOOKS_TOKEN={}",
            hooks.trim()
        )),
        "{said}"
    );
    assert!(
        !std::fs::read_to_string(&cfg)
            .unwrap()
            .contains(hooks.trim())
    );
    let toml = std::fs::read_to_string(data.join("config.toml")).unwrap();
    assert!(toml.contains("runtime = \"openclaw\""), "{toml}");

    // validate fails: the file is restored byte for byte, non-zero exit.
    std::fs::write(d.join("invalid"), "").unwrap();
    let before = std::fs::read(&cfg).unwrap();
    let out = run(&["--apply"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(err.contains("restored from"), "{err}");
    assert!(err.contains("Invalid config: hooks.token"), "{err}");
    assert_eq!(std::fs::read(&cfg).unwrap(), before);
    assert_eq!(backups().len(), 2);
    std::fs::remove_file(d.join("invalid")).unwrap();

    // No SecretRefs: the token is the `${VAR}` reference in the patch.
    std::fs::write(d.join("norefs"), "").unwrap();
    let start = calls().len();
    let out = run(&["--apply"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !calls()[start..]
            .iter()
            .any(|c| c.contains(&"hooks.token".into())),
        "{:?}",
        calls()
    );
    assert_eq!(
        json("real.json")["hooks"]["token"],
        "${WATS_OPENCLAW_HOOKS_TOKEN}"
    );

    // --print: the block, and no `config patch`.
    let start = calls().len();
    let out = run(&["--print"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        said.contains("token: \"${WATS_OPENCLAW_HOOKS_TOKEN}\""),
        "{said}"
    );
    assert!(!said.contains("`--apply` applies"), "{said}");
    let later = calls();
    assert!(
        !later[start..]
            .iter()
            .any(|c| c.get(1).is_some_and(|a| a == "patch")),
        "{later:?}"
    );
    assert_eq!(later.last().unwrap(), &["config", "validate"]);

    // --apply belongs to the agent half.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("PATH", &path)
        .arg("--dir")
        .arg(&data)
        .args(["setup", "openclaw", "--mcp", "--apply"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// `wats` with `stdin` written to its standard input.
fn wats_stdin(data: &std::path::Path, args: &[&str], stdin: &[u8]) -> std::process::Output {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .arg("--dir")
        .arg(data)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn guardian_keys_and_offline_events() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    let key = dir.path().join("guardian.key");
    let out = wats(
        &data,
        &["guardian", "keygen", "--out", key.to_str().unwrap()],
    );
    assert!(out.status.success(), "{out:?}");
    let printed = String::from_utf8(out.stdout).unwrap();
    let public = printed
        .lines()
        .find_map(|l| l.strip_prefix("public key: "))
        .unwrap()
        .to_string();
    assert_eq!(public.len(), 64);
    assert!(printed.contains("delete this clear copy"), "{printed}");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Never over an existing key.
    let seed = std::fs::read(&key).unwrap();
    let again = wats(
        &data,
        &["guardian", "keygen", "--out", key.to_str().unwrap()],
    );
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("never replaced"),
        "{again:?}"
    );
    assert_eq!(std::fs::read(&key).unwrap(), seed);

    // The key from standard input (`age -d … | wats guardian halt --key -`).
    let out = wats_stdin(
        &data,
        &[
            "--pow-bits=0",
            "guardian",
            "halt",
            "--key",
            "-",
            "--offline",
            "--reason",
            "test halt",
        ],
        &seed,
    );
    assert!(out.status.success(), "{out:?}");
    let event: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(event["kind"], "x.wearetheswarm.halt");
    assert_eq!(event["author"], public.as_str());
    assert_eq!(event["data"]["reason"], "test halt");
    assert_eq!(event["data"]["issued_at"], event["ts"]);
    assert_eq!(event["refs"], serde_json::json!([]));
    // Not in this release's (empty) guardian set: refused for publishing.
    let file = dir.path().join("halt.json");
    std::fs::write(&file, &out.stdout).unwrap();
    let out = wats(&data, &["guardian", "publish", file.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a guardian of this release"),
        "{out:?}"
    );
    let empty = wats_stdin(
        &data,
        &[
            "guardian",
            "approve",
            "--key",
            "-",
            "--offline",
            &"ab".repeat(32),
        ],
        b"\n",
    );
    assert!(!empty.status.success(), "{empty:?}");
}

#[test]
fn resume_clears_the_halt_marker_and_status_shows_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    let halt = "ab".repeat(32);
    std::fs::write(
        data.join("halted.json"),
        serde_json::json!({"halt": halt, "reason": "runaway", "signed_by": ["cd".repeat(32)],
                           "since": 1000, "at": 2000})
        .to_string(),
    )
    .unwrap();
    let out = wats(&data, &["status", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["halt"]["accept"], true, "{status}");
    assert_eq!(status["halt"]["marker"]["halt"], halt.as_str(), "{status}");

    let out = wats(&data, &["resume"]);
    assert!(out.status.success(), "{out:?}");
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains(&format!("overriding halt {halt}: runaway")),
        "{printed}"
    );
    assert!(!data.join("halted.json").exists());
    let overrides = std::fs::read_to_string(data.join("halt-overrides.json")).unwrap();
    assert!(overrides.contains(&halt), "{overrides}");
    let out = wats(&data, &["resume"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not halted"));

    // halt.accept: settable in the file and with `wats config`, never listed.
    assert!(
        wats(&data, &["config", "set", "halt.accept", "false"])
            .status
            .success()
    );
    let list = String::from_utf8(wats(&data, &["config", "list"]).stdout).unwrap();
    assert!(!list.contains("halt.accept"), "{list}");
    assert!(list.contains("halt.serve"), "{list}");
    let out = wats(&data, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        status["halt"],
        serde_json::json!({"accept": false}),
        "{status}"
    );
    // Its daemon warns at every start.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .arg("--dir")
        .arg(&data)
        .arg("--quiet")
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("this node ignores guardian halts (halt.accept = false)"),
        "{err}"
    );
}

/// Safety settings: recorded silently the first time, any change (an
/// agent editing config.toml, or `wats config set`) shown by `wats status`
/// and warned at startup until `wats config ack`.
#[test]
fn safety_changes_are_flagged_until_acked() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    let status = |data: &std::path::Path| -> serde_json::Value {
        let out = wats(data, &["status", "--json"]);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    };
    // First run: the baseline, silently.
    assert!(status(&data).get("safety").is_none());
    assert!(data.join("safety.json").exists());

    let set = wats(&data, &["config", "set", "halt.accept", "false"]);
    assert!(set.status.success(), "{set:?}");
    assert!(
        String::from_utf8_lossy(&set.stdout)
            .contains("safety setting changed: run `wats config ack` to acknowledge"),
        "{set:?}"
    );
    // Not a safety key: nothing to acknowledge.
    let set = wats(&data, &["config", "set", "agent.heartbeat", "2h"]);
    assert!(!String::from_utf8_lossy(&set.stdout).contains("safety"));
    // As an agent with a shell would: edit the file.
    let path = data.join("config.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[update]\nauto = true\n");
    std::fs::write(&path, text).unwrap();

    let notice = status(&data)["safety"].as_str().unwrap().to_string();
    assert!(
        notice.starts_with("safety settings changed on "),
        "{notice}"
    );
    assert!(notice.contains("halt.accept default→false"), "{notice}");
    assert!(notice.contains("update.auto default→true"), "{notice}");
    assert!(!notice.contains("agent.heartbeat"), "{notice}");
    assert!(
        notice.contains("run `wats config ack` in a terminal; agents must not run it"),
        "{notice}"
    );
    let text = String::from_utf8_lossy(&wats(&data, &["status"]).stdout).into_owned();
    assert!(
        text.contains("safety: safety settings changed on "),
        "{text}"
    );

    // The daemon warns at startup.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .arg("--dir")
        .arg(&data)
        .arg("--quiet")
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // The running node's MCP `status` tool shows it too.
    let mut running = status(&data);
    for _ in 0..40 {
        if running["node"] == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        running = status(&data);
    }
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("safety settings changed on "), "{err}");
    assert_eq!(running["node"], "running", "{running}");
    assert_eq!(
        running["safety"].as_str(),
        Some(notice.as_str()),
        "{running}"
    );

    let ack = wats(&data, &["config", "ack"]);
    assert!(ack.status.success(), "{ack:?}");
    let printed = String::from_utf8_lossy(&ack.stdout);
    assert!(
        printed.contains("halt.accept") && printed.contains("update.auto"),
        "{printed}"
    );
    assert!(status(&data).get("safety").is_none());
    let ack = wats(&data, &["config", "ack"]);
    assert!(String::from_utf8_lossy(&ack.stdout).contains("no safety setting changed"));

    // A workspace that is, or contains, the data directory is refused.
    for workdir in [".", ".."] {
        let out = wats(&data, &["config", "set", "agent.workdir", workdir]);
        assert!(!out.status.success(), "{workdir}: {out:?}");
    }
    std::fs::write(
        &path,
        "[agent]\nruntime = \"claude\"\naccess = \"workspace\"\nworkdir = \"..\"\ncommand = \"/nonexistent\"\n",
    )
    .unwrap();
    let out = wats(&data, &["--quiet"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("contains the data directory"),
        "{out:?}"
    );
}

/// `wats setup vibe`: wats edits Mistral Vibe's config.toml itself (a
/// throwaway HOME and VIBE_HOME; no `vibe` ever runs). A fresh VIBE_HOME
/// gets the file with the entry; an existing file is backed up and keeps
/// every other byte; a second run changes nothing; another profile
/// replaces the entry; `--remove` takes it out again; without VIBE_HOME it
/// is ~/.vibe, with `--project` ./.vibe. `wats setup` lists a `vibe` on
/// PATH as MCP-only.
#[test]
fn setup_vibe_edits_its_config() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    let vibe_home = dir.path().join("v");
    let config = vibe_home.join("config.toml");
    let run = |vibe_home: Option<&std::path::Path>, cwd: &std::path::Path, args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_wats"));
        command
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path())
            .env("PATH", "/bin:/usr/bin")
            .env("USER", "alice")
            .env_remove("WATS_PROFILE")
            .env_remove("VIBE_HOME")
            .current_dir(cwd)
            .arg("--dir")
            .arg(&data)
            .args(["setup", "vibe"])
            .args(args)
            .stdin(std::process::Stdio::null());
        if let Some(home) = vibe_home {
            command.env("VIBE_HOME", home);
        }
        let out = command.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let backups = || -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(&vibe_home)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("config.toml.bak-"))
            .collect();
        found.sort();
        found
    };
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_wats")).unwrap();
    let entry = format!(
        "[[mcp_servers]]\nname = \"wats\"\ntransport = \"stdio\"\ncommand = \"{}\"\n\
         args = [\"--dir\", \"{}\", \"mcp\", \"--profile\", \"alice\"]\n",
        exe.display(),
        data.display()
    );
    let home = dir.path();

    // No flag: points to --mcp, writes nothing.
    let said = run(Some(&vibe_home), home, &[]);
    assert!(said.contains("wats setup vibe --mcp"), "{said}");
    assert!(!vibe_home.exists());

    // A fresh VIBE_HOME: the file is created, no backup.
    let said = run(Some(&vibe_home), home, &["--mcp"]);
    assert!(
        said.contains("added to") && said.contains("(new file)"),
        "{said}"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), entry);
    assert!(backups().is_empty());

    // An existing config with another server and comments.
    let original = "# my Vibe settings\nactive_model = \"devstral-2\"\n\n\
                    [[mcp_servers]]\n# web pages\nname = \"fetch\"\ntransport = \"stdio\"\n\
                    command = \"uvx\"\nargs = [\"mcp-server-fetch\"]  # pinned later\n\n\
                    [tools.bash]\npermission = \"ask\"\n";
    std::fs::write(&config, original).unwrap();
    let said = run(Some(&vibe_home), home, &["--mcp"]);
    assert!(said.contains("added to"), "{said}");
    let found = backups();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(said.contains(&found[0].display().to_string()), "{said}");
    assert_eq!(std::fs::read_to_string(&found[0]).unwrap(), original);
    let text = std::fs::read_to_string(&config).unwrap();
    let added = format!("\n{entry}");
    assert_eq!(text.matches(&added).count(), 1, "{text}");
    assert_eq!(text.replacen(&added, "", 1), original);

    // Again: nothing changes, no new backup.
    let said = run(Some(&vibe_home), home, &["--mcp"]);
    assert!(said.contains("already in"), "{said}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert_eq!(backups().len(), 1);

    // Another profile: the entry is replaced, not duplicated.
    let said = run(Some(&vibe_home), home, &["--profile", "ops", "--mcp"]);
    assert!(said.contains("updated in"), "{said}");
    let text = std::fs::read_to_string(&config).unwrap();
    assert_eq!(text.matches("name = \"wats\"").count(), 1, "{text}");
    assert!(text.contains("\"--profile\", \"ops\"]"), "{text}");
    assert_eq!(backups().len(), 2);

    // --remove: the original comes back, with a backup.
    let said = run(Some(&vibe_home), home, &["--remove"]);
    assert!(said.contains("removed from"), "{said}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    assert_eq!(backups().len(), 3);
    let said = run(Some(&vibe_home), home, &["--remove"]);
    assert!(said.contains("nothing to remove"), "{said}");

    // Without VIBE_HOME: ~/.vibe. With --project: ./.vibe.
    run(None, home, &["--mcp"]);
    let home_config = home.join(".vibe").join("config.toml");
    assert_eq!(std::fs::read_to_string(&home_config).unwrap(), entry);
    let project = home.join("p");
    std::fs::create_dir_all(&project).unwrap();
    run(Some(&vibe_home), &project, &["--mcp", "--project"]);
    assert_eq!(
        std::fs::read_to_string(project.join(".vibe").join("config.toml")).unwrap(),
        entry
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);

    // Detection: a fake `vibe` on PATH (never run).
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    fake_cli(&bin, "vibe");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("HOME", home)
        .env("PATH", &bin)
        .arg("--dir")
        .arg(home.join("d2"))
        .arg("setup")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(
        said.contains("(agent: not supported yet; MCP: wats setup vibe --mcp)"),
        "{said}"
    );
    assert!(said.contains("Mistral Vibe found"), "{said}");
}
