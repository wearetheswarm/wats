//! Phase 0 spike: two nodes on loopback gossip a signed event and exchange
//! a blob.

use std::time::Duration;

use wats::event::{Draft, Kind};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 8;
    config.min_pow_bits = 8;
    config
}

async fn eventually<F, Fut, T>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = f().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gossip_signed_event_and_blob() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"))).await?;
    let mut b_config = config(&dir.path().join("b"));
    b_config.peers.push(a.endpoint().addr());
    let b = Node::start(b_config).await?;

    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"the swarm remembers")?;
    let event = b
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Offer),
                tags: vec!["cap:summarize".into()],
                text: "Here is my report.".into(),
                ..Draft::default()
            },
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    assert!(event.pow_bits() >= 8);

    let received = eventually("event on a", || async {
        a.store().get(&event.id).ok().flatten()
    })
    .await;
    assert_eq!(received.event.author, b.author_id());
    received.event.verify()?;

    let hash = &event.blobs[0].hash;
    let bytes = eventually("blob on a", || async {
        a.blob_bytes(hash).await.ok().flatten()
    })
    .await;
    assert_eq!(&bytes[..], b"the swarm remembers");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn data_dir_is_locked() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(dir.path())).await?;
    let err = Node::start(config(dir.path())).await.unwrap_err();
    assert!(err.to_string().contains("another wats node"), "{err:#}");
    a.shutdown().await?;
    Ok(())
}
