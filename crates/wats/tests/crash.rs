//! Crash reports: a panicking node writes a redacted report; `wats report
//! show` prints it; `wats report send` delivers it to a collector that
//! accepts reports, and only to one. With an inbox key, reports are sealed
//! to the inbox instead (ADR 0015 §5) and travel by gossip.

#![cfg(unix)]

use std::time::Duration;

use wats::inbox::{InboxKey, read_submissions};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

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
    let id = list.split_whitespace().next().unwrap().to_string();

    // With an inbox key, the report is sealed to the inbox instead and
    // reaches the inbox node by gossip, as v2 `submit` events.
    let inbox_dir = dir.path().join("inbox-node");
    let inbox_node = receiver(&inbox_dir).await?;
    let key_file = dir.path().join("inbox.key");
    let keygen = wats(
        &data,
        &ticket,
        &["inbox", "keygen", "--out", key_file.to_str().unwrap()],
    );
    assert!(keygen.status.success());
    let printed = String::from_utf8(keygen.stdout)?;
    let public = printed
        .lines()
        .find_map(|l| l.strip_prefix("public key: "))
        .unwrap();
    let key = InboxKey::load(&key_file)?;
    assert_eq!(public, key.public().to_b64());
    assert!(printed.contains(key.public().id()), "{printed}");
    // Never over an existing secret.
    let again = wats(
        &data,
        &ticket,
        &["inbox", "keygen", "--out", key_file.to_str().unwrap()],
    );
    assert!(!again.status.success());

    std::fs::write(
        data.join("config.toml"),
        format!("[report]\ninbox = \"{public}\"\n"),
    )?;
    let sent = wats(
        &data,
        &inbox_node.ticket().await.to_string(),
        &[
            "--pow-bits",
            "0",
            "--min-pow-bits",
            "0",
            "report",
            "send",
            &id,
        ],
    );
    let stdout = String::from_utf8_lossy(&sent.stdout);
    assert!(
        sent.status.success() && stdout.contains(key.public().id()),
        "{stdout} {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    eventually("the inbox node stores the submission", async || {
        !inbox_node
            .store()
            .submits_to(key.public().id())
            .unwrap()
            .is_empty()
    })
    .await;
    // `wats inbox read` next to the running node: the exact report bytes,
    // with the secret on standard input (`sops decrypt … | … --key -`).
    let out = dir.path().join("read");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .arg("--dir")
        .arg(&inbox_dir)
        .args(["inbox", "read", "--key", "-"])
        .arg("--out")
        .arg(&out)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), &std::fs::read(&key_file)?)?;
    let read = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(
        read.status.success(),
        "{stdout} {}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(stdout.contains("1 new report"), "{stdout}");
    let files = walk(&out);
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(std::fs::read(&files[0])?, wats::crash::bytes(&data, &id)?);

    let _ = tokio::time::timeout(Duration::from_secs(5), inbox_node.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), collector_node.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), refuser.shutdown()).await;
    Ok(())
}

/// A node that accepts submissions mined at the size and submit bits only.
async fn receiver(dir: &std::path::Path) -> anyhow::Result<std::sync::Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    Node::start(config).await
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A large report crosses the network in several sealed parts, each a
/// valid v2 `submit` within 16 KiB, and the inbox holder reassembles it.
#[tokio::test(flavor = "multi_thread")]
async fn large_report_through_the_inbox_in_parts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let b = receiver(&dir.path().join("b")).await?;
    let mut config = NodeConfig::new(dir.path().join("a"));
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = vec![b.endpoint().addr()];
    let a = Node::start(config).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    let key = InboxKey::generate()?;
    // About 64 KiB of log lines that compress like a real report.
    let mut report = Vec::new();
    for n in 0u32.. {
        if report.len() >= 64 * 1024 {
            break;
        }
        let noise = blake3::hash(&n.to_le_bytes()).to_hex();
        report.extend_from_slice(
            format!("{{\"line\":{n},\"peer\":\"#{}\"}}\n", &noise[..24]).as_bytes(),
        );
    }
    let sealed = a
        .send_to_inbox(key.public(), "crash", report.clone())
        .await?;
    assert!(sealed.events.len() >= 2, "{}", sealed.events.len());
    for event in &sealed.events {
        assert!(serde_json_canonicalizer::to_vec(event)?.len() <= wats::event::MAX_EVENT_BYTES);
        wats::wire::AnyEvent::from_value(event.clone())?;
    }
    let parts = sealed.events.len();
    eventually("b stores every part", async || {
        b.store().submits_to(key.public().id()).unwrap().len() == parts
    })
    .await;
    let out = dir.path().join("read");
    let events = b.store().submits_to(key.public().id())?;
    let summary = read_submissions(&key, &events, &out)?;
    assert_eq!(summary.written.len(), 1, "{summary:?}");
    assert_eq!(std::fs::read(&summary.written[0])?, report);
    // Another key reads nothing.
    let other = InboxKey::generate()?;
    let summary = read_submissions(&other, &events, &out)?;
    assert_eq!((summary.written.len(), summary.undecryptable), (0, parts));

    let _ = tokio::time::timeout(Duration::from_secs(5), a.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), b.shutdown()).await;
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
