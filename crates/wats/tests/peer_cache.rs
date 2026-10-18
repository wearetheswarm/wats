//! A node remembers the peers it met and dials them at its next start,
//! without any bootstrap entry (security review B1).

use std::time::Duration;

use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(8);
    config.min_pow_bits = Some(8);
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn restarts_from_the_peers_it_met() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"))).await?;
    let mut b_config = config(&dir.path().join("b"));
    b_config.peers.push(a.endpoint().addr());
    let b = Node::start(b_config).await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    // Wait for the hello, which makes A worth caching.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while b.peer_info(&a.endpoint_id()).is_none() {
        assert!(tokio::time::Instant::now() < deadline, "no hello");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    b.shutdown().await?;
    drop(b);
    let cached = std::fs::read_to_string(dir.path().join("b").join("peers.json"))?;
    assert!(cached.contains(&a.endpoint_id().to_string()), "{cached}");

    // B restarts with no peer and no bootstrap: the cache brings A back.
    let b = Node::start(config(&dir.path().join("b"))).await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    assert!(b.neighbors().contains(&a.endpoint_id()));
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn falls_back_to_the_built_in_bootstrap_when_the_cache_is_dead() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let seed = Node::start(config(&dir.path().join("seed"))).await?;
    let port = seed
        .endpoint()
        .addr()
        .ip_addrs()
        .find(|a| a.ip().is_loopback())
        .map(|a| a.port())
        .expect("a loopback address");
    // A cache whose only peer is gone.
    let b_dir = dir.path().join("b");
    std::fs::create_dir_all(&b_dir)?;
    let dead = iroh::SecretKey::from_bytes(&[7; 32]).public();
    let now = wats::node::now_secs();
    std::fs::write(
        b_dir.join("peers.json"),
        format!(r#"[{{"id":"{dead}","addrs":["127.0.0.1:9"],"last_seen":{now}}}]"#),
    )?;
    let mut b_config = config(&b_dir);
    b_config.default_bootstrap = vec![format!("{}@127.0.0.1:{port}", seed.endpoint_id()).parse()?];
    let b = Node::start(b_config).await?;
    // The built-in seed is not dialed while the cache may still answer…
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(b.neighbors().is_empty());
    // …and is once it did not.
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(30))
        .await?;
    assert!(b.neighbors().contains(&seed.endpoint_id()));
    b.shutdown().await?;
    seed.shutdown().await?;
    Ok(())
}
