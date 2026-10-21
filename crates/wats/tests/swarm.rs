//! The size of the swarm as one node sees it (ADR 0026): its gossip
//! neighbors and the nodes it heard from in the last 24 hours, per
//! network, in `status`, the banner and `wats ps`. Loopback only.

use std::time::Duration;

use serde_json::{Value, json};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.infra = wats::infra::Infra::NONE;
    config.pow_bits = Some(8);
    config.min_pow_bits = Some(8);
    config
}

async fn eventually<F, T>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn public(node: &Node) -> (u64, u64) {
    let swarm = node.swarm_json();
    let net = &swarm[PUBLIC_NET];
    (
        net["neighbors"].as_u64().unwrap(),
        net["nodes_24h"].as_u64().unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn neighbors_and_nodes_heard_from_none_one_several_and_after_a_restart() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let a_dir = dir.path().join("a");
    let a = Node::start(config(&a_dir)).await?;
    // Alone: the stable shape, all zero.
    assert_eq!(
        a.swarm_json(),
        json!({"public": {"neighbors": 0, "nodes_24h": 0}})
    );

    // One neighbor.
    let mut b_config = config(&dir.path().join("b"));
    b_config.peers.push(a.endpoint().addr());
    let b = Node::start(b_config).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    eventually("a sees b", || (public(&a) == (1, 1)).then_some(())).await;
    eventually("b sees a", || (public(&b) == (1, 1)).then_some(())).await;

    // Several.
    let mut others = Vec::new();
    for name in ["c", "d"] {
        let mut c = config(&dir.path().join(name));
        c.peers.push(a.endpoint().addr());
        others.push(Node::start(c).await?);
    }
    eventually("a sees b, c, d", || (public(&a) == (3, 3)).then_some(())).await;

    // A node that left is still one heard from in the last 24 h.
    for node in others {
        node.shutdown().await?;
    }
    eventually("c and d gone", || (public(&a).0 == 1).then_some(())).await;
    assert_eq!(public(&a), (1, 3));

    // Kept across a restart: what a stopped node shows, and what it
    // starts with.
    a.shutdown().await?;
    b.shutdown().await?;
    let now = wats::node::now_secs();
    let stored = wats::swarm::stored_json(&a_dir, &[PUBLIC_NET.to_string()], now);
    assert_eq!(stored, json!({"public": {"neighbors": 0, "nodes_24h": 3}}));
    assert_eq!(
        wats::swarm::lines(&stored, true),
        ["swarm: connecting… · 3 nodes seen in 24 h"]
    );
    drop(a);
    let a = Node::start(config(&a_dir)).await?;
    assert_eq!(public(&a), (0, 3));
    a.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn per_network() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut a_config = config(&dir.path().join("a"));
    a_config.nets = vec![PUBLIC_NET.into(), "lab".into()];
    let a = Node::start(a_config).await?;
    // b is on the public network only.
    let mut b_config = config(&dir.path().join("b"));
    b_config.peers.push(a.endpoint().addr());
    let b = Node::start(b_config).await?;
    eventually("a sees b on public", || {
        (a.swarm_json()["public"]["neighbors"] == 1).then_some(())
    })
    .await;
    let swarm = a.swarm_json();
    assert_eq!(
        swarm,
        json!({"public": {"neighbors": 1, "nodes_24h": 1},
               "lab": {"neighbors": 0, "nodes_24h": 0}})
    );
    let mut lines = wats::swarm::lines(&swarm, true);
    lines.sort();
    assert_eq!(
        lines,
        [
            "swarm (lab): connecting…",
            "swarm (public): 1 neighbor, 1 node seen in 24 h"
        ]
    );
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// Recent sightings of two nodes and a stale one, as a previous run left
/// them.
fn plant(dir: &std::path::Path) {
    let id = |n: u8| iroh::SecretKey::from_bytes(&[n; 32]).public().to_string();
    let now = wats::node::now_secs();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("swarm.json"),
        json!({"public": {id(1): now - 60, id(2): now - 3600, id(3): now - 2 * 86_400}})
            .to_string(),
    )
    .unwrap();
}

/// `wats` with a home and a registry of its own, next to `dir`: never the
/// real `~/.wats`.
fn command(dir: &std::path::Path) -> std::process::Command {
    let machine = dir.parent().unwrap();
    std::fs::create_dir_all(machine.join("home")).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_wats"));
    command
        .env("WATS_HERMETIC", "1")
        .env("HOME", machine.join("home"))
        .env("WATS_RUN_DIR", machine.join("run"))
        .env_remove("WATS_DIR")
        .env_remove("WATS_PROFILE")
        .env_remove("WATS_MCP_SOCKET")
        .env_remove("WATS_RUNNER_TOKEN")
        .env_remove("RUST_LOG")
        .args(["--infra", "none", "--dir"])
        .arg(dir)
        .stdin(std::process::Stdio::null());
    command
}

fn wats(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    command(dir).args(args).output().unwrap()
}

#[test]
fn status_of_a_stopped_node() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d");
    plant(&data);
    let out = wats(&data, &["status", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["node"], "not running");
    assert_eq!(
        status["swarm"],
        json!({"public": {"neighbors": 0, "nodes_24h": 2}}),
        "{status}"
    );
    let out = wats(&data, &["status"]);
    let human = String::from_utf8(out.stdout).unwrap();
    assert!(
        human.contains("\nswarm: not running · 2 nodes seen in 24 h\n"),
        "{human}"
    );
    assert!(!human.contains("swarm.public"), "{human}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn status_ps_and_banner_of_a_running_node() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("d");
    plant(&data);
    let mut node = command(&data)
        .args(["--pow-bits=0", "--min-pow-bits=0", "--no-agent"])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(dir.path().join("stderr"))?)
        .spawn()?;
    let status = eventually("the node answers", || {
        let out = wats(&data, &["status", "--json"]);
        let status: Value = serde_json::from_slice(&out.stdout).ok()?;
        (status["node"] == "running").then_some(status)
    })
    .await;
    // No neighbor, and what its last runs heard from.
    assert_eq!(
        status["swarm"],
        json!({"public": {"neighbors": 0, "nodes_24h": 2}}),
        "{status}"
    );
    let human = String::from_utf8(wats(&data, &["status"]).stdout)?;
    assert!(
        human.contains("\nswarm: connecting… · 2 nodes seen in 24 h\n"),
        "{human}"
    );
    let ps: Value = serde_json::from_slice(&wats(&data, &["ps", "--json"]).stdout)?;
    let entry = ps
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["running"] == true)
        .unwrap_or_else(|| panic!("{ps}"));
    assert_eq!(entry["swarm"]["public"]["nodes_24h"], 2, "{ps}");
    let table = String::from_utf8(wats(&data, &["ps"]).stdout)?;
    assert!(
        table.lines().next().unwrap().contains("NODES 24H"),
        "{table}"
    );
    // The banner, on stderr (no art: not a terminal), says what was known
    // at start.
    let banner = std::fs::read_to_string(dir.path().join("stderr"))?;
    assert!(
        banner.contains("\nswarm: connecting… · 2 nodes seen in 24 h\n"),
        "{banner}"
    );
    let _ = node.kill();
    let _ = node.wait();
    Ok(())
}
