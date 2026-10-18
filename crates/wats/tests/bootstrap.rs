//! Bootstrap entries `<endpoint-id>@<host>:<port>` (ADR 0008): DNS gives
//! the address, the id is pinned.

use std::process::Command;
use std::time::Duration;

use wats::bootstrap::BootstrapEntry;
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_by_host_name_with_pinned_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let port = free_udp_port();
    let mut router_config = config(&dir.path().join("router"));
    router_config.port = Some(port);
    let router = Node::start(router_config).await?;
    assert_eq!(router.endpoint().bound_sockets()[0].port(), port);

    // The right id: connects through "localhost".
    let mut good = config(&dir.path().join("good"));
    good.bootstrap = vec![format!("{}@localhost:{port}", router.endpoint_id()).parse()?];
    let good = Node::start(good).await?;
    good.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    // Same host and port, wrong id: Iroh refuses the connection.
    let impostor = iroh::SecretKey::from_bytes(&[1; 32]).public();
    let mut pinned = config(&dir.path().join("pinned"));
    pinned.bootstrap = vec![BootstrapEntry {
        id: impostor,
        host: "localhost".into(),
        port,
    }];
    let pinned = Node::start(pinned).await?;
    assert!(
        pinned
            .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(3))
            .await
            .is_err()
    );

    for n in [router, good, pinned] {
        n.shutdown().await?;
    }
    Ok(())
}

#[test]
fn wats_id_is_stable_and_matches_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let run = || {
        let out = Command::new(env!("CARGO_BIN_EXE_wats"))
            .args(["--port", "7842", "--dir"])
            .arg(dir.path())
            .arg("id")
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let first = run();
    assert_eq!(first, run());
    let endpoint = first
        .lines()
        .find_map(|l| l.strip_prefix("endpoint_id: "))
        .unwrap()
        .to_string();
    assert!(first.contains(&format!(
        "bootstrap_entry: {endpoint}@router.wearetheswarm.ai:7842"
    )));
    let entry: BootstrapEntry = first
        .lines()
        .find_map(|l| l.strip_prefix("bootstrap_entry: "))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(entry.id.to_string(), endpoint);
}

#[test]
fn secret_file_gives_the_same_identity() {
    let dir = tempfile::tempdir().unwrap();
    let id = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_wats"))
            .args(args)
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8(out.stdout).unwrap())
    };
    let generated = dir.path().join("generated");
    let (ok, local) = id(&["--dir", generated.to_str().unwrap(), "id"]);
    assert!(ok);
    // Another data dir, same secret file: same endpoint id.
    let key = generated.join("secret.key");
    let (ok, mounted) = id(&[
        "--dir",
        dir.path().join("pod").to_str().unwrap(),
        "--secret-file",
        key.to_str().unwrap(),
        "id",
    ]);
    assert!(ok);
    assert_eq!(local, mounted);
    // A missing secret file is an error, never a fresh identity.
    let (ok, _) = id(&["--secret-file", "/nonexistent/secret.key", "id"]);
    assert!(!ok);
}
