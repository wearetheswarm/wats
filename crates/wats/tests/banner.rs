//! The startup banner is for a person at a terminal only: it never shows up
//! in `wats mcp` (stdout is the MCP protocol) nor when the output is piped.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Duration;

const BANNER_LINE: &str = "Every node is the swarm.";

fn wats(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.env("WATS_HERMETIC", "1")
        .env_remove("NO_COLOR")
        .args(["--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(dir);
    cmd
}

#[test]
fn no_banner_when_piped() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = wats(dir.path())
        .arg("node")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait for the node to be up, then stop it.
    let stdout = child.stdout.take().unwrap();
    let mut ready = String::new();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if line.contains("\"type\":\"ready\"") {
            ready = line;
            break;
        }
    }
    assert!(!ready.is_empty(), "the node never got ready");
    child.kill().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    child.wait().unwrap();
    assert!(!stderr.contains(BANNER_LINE), "{stderr}");
    assert!(!stderr.contains("\x1b[33m"), "{stderr}");
    // The status lines are still there.
    assert!(stderr.contains("Ctrl-C to stop"), "{stderr}");
}

#[test]
fn no_banner_in_wats_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = wats(dir.path())
        .args(["mcp", "--idle-exit", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // An MCP initialize; once it is answered, close stdin: the session ends.
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    {
        use std::io::Write;
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"test","version":"0"}}}}}}"#
        )
        .unwrap();
    }
    let mut lines = vec![
        rx.recv_timeout(Duration::from_secs(30))
            .expect("an MCP answer"),
    ];
    drop(stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
        lines.push(line);
    }
    let stdout = lines.join("\n");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stdout.contains("\"jsonrpc\""),
        "an MCP answer: {stdout} / {stderr}"
    );
    for out in [&stdout, &stderr] {
        assert!(!out.contains(BANNER_LINE), "{out}");
        assert!(!out.contains("WeAreTheSwarm · wats"), "{out}");
    }
    // stdout carries the protocol only: every line is JSON.
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "not JSON on stdout: {line}"
        );
    }
}
