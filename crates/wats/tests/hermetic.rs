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
    assert!(
        said.contains("openclaw") && said.contains("(not supported yet)"),
        "{said}"
    );
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
