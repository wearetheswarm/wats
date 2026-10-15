//! GREASE everywhere (ADR 0010, A8): with every emission opportunity taken,
//! nodes still exchange hello, sync, gossip and replicate.

use std::sync::Arc;
use std::time::Duration;

use wats::event::{Draft, Kind};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};

async fn node(dir: &std::path::Path, peers: &[&Arc<Node>]) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 0;
    config.min_pow_bits = 0;
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_work_with_grease_on_every_message() -> anyhow::Result<()> {
    wats::grease::set_rate(1);
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[]).await?;
    // Published before b exists: b gets it through sync.
    a.publish(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(Kind::Note),
            text: "before b".into(),
            ..Draft::default()
        },
        &[],
    )
    .await?;
    let b = node(&dir.path().join("b"), &[&a]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    let file = dir.path().join("f.txt");
    std::fs::write(&file, b"greased")?;
    let live = a
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                text: "live, with a blob".into(),
                ..Draft::default()
            },
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let synced = b.store().count()? == 2;
        let blob = b.blob_bytes(&live.blobs[0].hash).await?.is_some();
        let hello = b.peer_info(&a.endpoint_id()).is_some();
        if synced && blob && hello {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "synced={synced} blob={blob} hello={hello}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // GREASE features are stripped before they are stored.
    let info = b.peer_info(&a.endpoint_id()).unwrap();
    assert!(info.features.iter().all(|f| !wats::wire::is_grease(f)));
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
