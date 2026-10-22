//! `wats mcp` as Claude Code runs it: a stdio child process per session.
//! Two sessions on one data directory share one node.

#![cfg(unix)]

use std::time::Duration;

mod common;
use common::ClearToolEnv;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn session(dir: &std::path::Path, cwd: &std::path::Path, profile: &str) -> Client {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    cmd.clear_tool_env()
        .args(["--loopback", "--pow-bits=4", "--profile", profile, "--dir"])
        .arg(dir)
        .args(["mcp", "--idle-exit", "1"])
        .current_dir(cwd);
    let transport = TokioChildProcess::new(cmd).expect("spawn wats mcp");
    ().serve(transport).await.expect("mcp handshake")
}

async fn call(client: &Client, tool: &'static str, args: Value) -> Value {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_share_one_node() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("wats");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("plan.md"), "# Plan\n").unwrap();

    let first = session(&dir, &project, "lead").await;
    assert!(dir.join("mcp.sock").exists(), "background node listens");
    let second = session(&dir, &project, "worker").await;

    // Files are shared relative to the session's working directory.
    let posted = call(
        &first,
        "post",
        json!({"title": "Sprint plan", "text": "Plan for the sprint.", "kind": "note", "blobs": [{"path": "plan.md"}]}),
    )
    .await;
    let found = call(&second, "search", json!({"text": "sprint"})).await;
    assert_eq!(found["posts"][0]["id"], posted["id"]);

    let reply = call(
        &second,
        "reply",
        json!({"post_id": posted["id"], "text": "On it."}),
    )
    .await;
    assert_ne!(reply["post"]["author"], posted["post"]["author"]);

    // The first session ends (its process is killed); the node lives on.
    first.cancel().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let read = call(&second, "read", json!({"id": posted["id"]})).await;
    assert_eq!(read["thread"].as_array().unwrap().len(), 2);
    second.cancel().await.unwrap();

    // Without sessions, the background node exits after its idle delay.
    for _ in 0..100 {
        if !dir.join("mcp.sock").exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("background node did not exit when idle");
}
