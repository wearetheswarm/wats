//! Crash reports: a panicking node writes a redacted report; `wats report
//! show` prints it; `wats report send` delivers it to a collector that
//! accepts reports, and only to one.

#![cfg(unix)]

use std::time::Duration;

use wats::node::{NetworkMode, Node, NodeConfig};

async fn collector(dir: &std::path::Path, accept: bool) -> anyhow::Result<std::sync::Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.report_accept = accept;
    Node::start(config).await
}

fn wats(data: &std::path::Path, ticket: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env_remove("WATS_CRASH_TEST")
        .arg("--dir")
        .arg(data)
        .arg(format!("--peer={ticket}"))
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_report_show_and_send() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let collector_node = collector(&dir.path().join("collector"), true).await?;
    let refuser = collector(&dir.path().join("refuser"), false).await?;
    let ticket = collector_node.ticket().await.to_string();
    let data = dir.path().join("crasher");

    // The node panics after starting.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env("WATS_CRASH_TEST", "panic")
        .arg("--dir")
        .arg(&data)
        .arg(format!("--peer={ticket}"))
        .arg("node")
        .output()?;
    assert!(!out.status.success());
    let list = String::from_utf8(wats(&data, &ticket, &["report", "list"]).stdout)?;
    assert!(list.contains("panic") && list.contains("new"), "{list}");

    let shown = wats(&data, &ticket, &["report", "show"]);
    let report: serde_json::Value = serde_json::from_slice(&shown.stdout)?;
    assert_eq!(report["kind"], "panic");
    assert!(
        report["message"]
            .as_str()
            .unwrap()
            .contains("crash test requested")
    );
    assert!(!report["backtrace"].as_array().unwrap().is_empty());
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        !text.contains("127.0.0.1"),
        "IP addresses never leave: {text}"
    );
    let home = std::env::home_dir().unwrap().display().to_string();
    assert!(!text.contains(&home), "paths are ~-relative: {text}");

    // A node that does not collect refuses it.
    std::fs::write(
        data.join("config.toml"),
        format!("[report]\ncollector = \"{}\"\n", refuser.endpoint_id()),
    )?;
    let refused = wats(
        &data,
        &refuser.ticket().await.to_string(),
        &["report", "send"],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("does not collect"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // The collector stores it, under the sender's node id.
    std::fs::write(
        data.join("config.toml"),
        format!(
            "[report]\ncollector = \"{}\"\n",
            collector_node.endpoint_id()
        ),
    )?;
    let sent = wats(&data, &ticket, &["report", "send"]);
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let stored: Vec<_> = walk(&dir.path().join("collector/reports"));
    assert_eq!(stored.len(), 1, "{stored:?}");
    let list = String::from_utf8(wats(&data, &ticket, &["report", "list"]).stdout)?;
    assert!(list.contains("sent"), "{list}");

    let _ = tokio::time::timeout(Duration::from_secs(5), collector_node.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), refuser.shutdown()).await;
    Ok(())
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.path().is_dir() {
            out.extend(walk(&entry.path()));
        } else {
            out.push(entry.path());
        }
    }
    out
}
