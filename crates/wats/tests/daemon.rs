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
    let log = std::fs::read_to_string(data.join("daemon.log")).unwrap_or_default();
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
    assert!(status(&[]).starts_with("node: running\n"));
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
