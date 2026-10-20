//! MCP-only awareness: a `wats mcp` session on a background node knows it,
//! and can hand over to a daemon (`start_daemon`) without the MCP client
//! noticing.

#![cfg(unix)]

use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> Result<Value, String> {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error == Some(true) {
        return Err(format!("{:?}", result.content));
    }
    Ok(result.structured_content.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn background_session_hands_over_to_a_daemon() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("d");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.env("WATS_HERMETIC", "1")
        .env_remove("WATS_RUNNER_TOKEN")
        .args(["--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args(["mcp", "--idle-exit", "5"]);
    let client = ().serve(TokioChildProcess::new(cmd)?).await?;

    let status = call(&client, "status", json!({})).await.unwrap();
    assert_eq!(status["mode"], "background", "{status}");
    // Hermetic: relay and lookup cannot apply, nothing to bootstrap from.
    let mut health = status["health"].clone();
    let connectivity = health.as_object_mut().unwrap().remove("connectivity");
    assert_eq!(
        health,
        json!({"state": "ok", "checks": {
            "relay": "off", "lookup": "off", "bootstrap": "off", "peers": "off"}}),
        "{status}"
    );
    // Nowhere to dial: never "reconnecting".
    let connectivity = connectivity.unwrap();
    assert_eq!(connectivity["peers"], 0, "{status}");
    assert!(connectivity.get("attempt").is_none(), "{status}");
    assert!(
        status["hint"]
            .as_str()
            .unwrap()
            .contains("only uses the swarm")
    );
    let first = call(&client, "search", json!({})).await.unwrap();
    assert!(first.get("hint").is_some(), "once: {first}");
    let second = call(&client, "search", json!({})).await.unwrap();
    assert!(second.get("hint").is_none(), "not twice: {second}");

    assert!(
        call(&client, "start_daemon", json!({"user_agreed": false}))
            .await
            .is_err()
    );
    let started = call(&client, "start_daemon", json!({"user_agreed": true}))
        .await
        .unwrap();
    let pid = started["pid"].as_u64().unwrap();

    // The same MCP session, now served by the daemon.
    let mut mode = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(status) = call(&client, "status", json!({})).await {
            mode = status["mode"].clone();
            if mode == "daemon" {
                break;
            }
        }
    }
    let log = std::fs::read_to_string(data.join("logs").join("node.log")).unwrap_or_default();
    assert_eq!(mode, "daemon", "daemon log:\n{log}");

    // `wats status` shows the running daemon, as the MCP tool does.
    let status = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .arg("--dir")
            .arg(&data)
            .arg("status")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let shown: Value = serde_json::from_str(&status(&["--json"])).unwrap();
    assert_eq!(shown["node"], "running", "{shown}");
    assert_eq!(shown["mode"], "daemon");
    assert_eq!(shown["nets"], json!(["public"]));
    assert_eq!(shown["health"]["state"], "ok", "{shown}");
    let human = status(&[]);
    assert!(human.starts_with("node: running\n"));
    assert!(human.contains("\nhealth: ok\n"), "{human}");
    let posted = call(
        &client,
        "post",
        json!({"title": "after the handover", "text": "still works", "kind": "note"}),
    )
    .await
    .unwrap();
    assert!(posted["id"].is_string(), "{posted}");
    assert!(
        call(&client, "start_daemon", json!({"user_agreed": true}))
            .await
            .is_err(),
        "already a daemon"
    );
    client.cancel().await?;
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()?;
    Ok(())
}

/// `wats` started by hand while an MCP session holds a background node
/// takes over (ADR 0021 §4): the background node hands over, the daemon
/// takes the lock, and the session carries on, served by the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_started_by_hand_takes_over_a_background_node() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("d");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.env("WATS_HERMETIC", "1")
        .env_remove("WATS_RUNNER_TOKEN")
        .args(["--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args(["mcp", "--idle-exit", "30"]);
    let client = ().serve(TokioChildProcess::new(cmd)?).await?;
    let status = call(&client, "status", json!({})).await.unwrap();
    assert_eq!(status["mode"], "background", "{status}");

    let log = std::fs::File::create(dir.path().join("daemon.log"))?;
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .args(["--pow-bits=0", "--min-pow-bits=0", "--dir"])
        .arg(&data)
        .args(["--no-agent", "--quiet"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()?;

    // The same session, now served by the daemon, with no error on the way.
    let mut mode = Value::Null;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        match call(&client, "status", json!({})).await {
            Ok(status) => {
                mode = status["mode"].clone();
                if mode == "daemon" {
                    break;
                }
            }
            Err(e) => panic!("a read call failed during the takeover: {e}"),
        }
    }
    let log = std::fs::read_to_string(dir.path().join("daemon.log")).unwrap_or_default();
    assert_eq!(mode, "daemon", "daemon log:\n{log}");
    assert!(log.contains("asking it to hand over"), "{log}");
    assert!(daemon.try_wait()?.is_none(), "the daemon runs: {log}");
    let found = call(&client, "search", json!({})).await.unwrap();
    assert!(found.get("posts").is_some(), "{found}");

    client.cancel().await?;
    let _ = daemon.kill();
    let _ = daemon.wait();
    Ok(())
}

/// A daemon does not take over from another daemon: today's error, naming
/// the holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_does_not_take_over_another_daemon() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("d");
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .args(["--pow-bits=0", "--min-pow-bits=0", "--dir"])
            .arg(&data)
            .args(["--no-agent", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
    };
    let mut first = run()?;
    // Wait for its socket.
    for _ in 0..100 {
        if std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .arg("--dir")
            .arg(&data)
            .args(["status", "--json"])
            .output()?
            .stdout
            .starts_with(b"{")
            && std::fs::read_to_string(data.join("lock")).is_ok_and(|l| l.contains("daemon"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let second = run()?.wait_with_output()?;
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success());
    assert!(err.contains("another wats node is using"), "{err}");
    assert!(first.try_wait()?.is_none(), "the first daemon still runs");
    let _ = first.kill();
    let _ = first.wait();
    Ok(())
}
