//! Replication k at publish time (SPEC §2.5) and catch-up sync (SPEC §2.6).

use std::sync::Arc;
use std::time::Duration;

use wats::event::{Draft, Kind, parse_hex};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::proto::closest;

async fn node(dir: &std::path::Path, peers: &[&Arc<Node>], k: usize) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 0;
    config.min_pow_bits = 0;
    config.replication = k;
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

fn note(text: &str) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        text: text.into(),
        ..Draft::default()
    }
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn blobs_go_to_the_k_closest_peers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut others = Vec::new();
    for i in 0..4 {
        let peers: Vec<&Arc<Node>> = others.iter().collect();
        let n = node(&dir.path().join(format!("n{i}")), &peers, 2).await?;
        others.push(n);
    }
    let refs: Vec<&Arc<Node>> = others.iter().collect();
    let publisher = node(&dir.path().join("publisher"), &refs, 2).await?;
    publisher
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    assert_eq!(publisher.known_peers().len(), 4);

    let file = dir.path().join("dataset.bin");
    std::fs::write(&file, vec![7u8; 300_000])?;
    let event = publisher
        .publish(
            note("A dataset for whoever needs it."),
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    let hash = &event.blobs[0].hash;
    let expected = closest(&parse_hex::<32>(hash, "hash")?, &publisher.known_peers(), 2);

    for n in &others {
        if expected.contains(&n.endpoint_id()) {
            eventually("replica holds the blob", async || {
                n.blob_bytes(hash).await.ok().flatten().is_some()
            })
            .await;
        }
    }
    // Give stragglers a chance, then check nobody else fetched it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for n in &others {
        let has = n.blob_bytes(hash).await?.is_some();
        assert_eq!(has, expected.contains(&n.endpoint_id()));
        // Every node got the event itself through gossip.
        assert!(n.store().get(&event.id)?.is_some());
    }

    for n in others.iter().chain([&publisher]) {
        n.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn late_joiner_catches_up_and_fetches_blobs_on_demand() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[], 3).await?;
    // Published while nobody listens: more than one sync page.
    for i in 0..300 {
        a.publish(note(&format!("message {i}")), &[]).await?;
    }
    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"published before you arrived")?;
    let with_blob = a
        .publish(
            note("Report attached."),
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;

    let b = node(&dir.path().join("b"), &[&a], 3).await?;
    eventually("b synced", async || b.store().count().unwrap() == 301).await;
    assert!(b.store().get(&with_blob.id)?.is_some());

    // The blob was not replicated (nobody was there); B fetches it on read.
    let blob = &with_blob.blobs[0];
    assert!(b.blob_bytes(&blob.hash).await?.is_none());
    let path = b.fetch_and_export(blob).await?.expect("fetched from a");
    assert_eq!(std::fs::read(path)?, b"published before you arrived");

    // A restart syncs only what is new since the watermark.
    a.publish(note("after b went away"), &[]).await?;
    b.shutdown().await?;
    let b = node(&dir.path().join("b"), &[&a], 3).await?;
    eventually("b synced again", async || b.store().count().unwrap() == 302).await;

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
