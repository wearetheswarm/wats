//! Replication k at publish time (SPEC §2.5) and catch-up sync (SPEC §2.6).

use std::sync::Arc;
use std::time::Duration;

use wats::event::{Draft, Kind, parse_hex};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::proto::closest;

async fn node(dir: &std::path::Path, peers: &[&Arc<Node>], k: usize) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.replication = k;
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

fn note(text: &str) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some("test post".into()),
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

#[tokio::test(flavor = "multi_thread")]
async fn blob_quota_refuses_replicas() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path().join("full"));
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.blob_quota = Some(1);
    let full = Node::start(config).await?;
    let publisher = node(&dir.path().join("publisher"), &[&full], 3).await?;
    publisher
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let file = dir.path().join("big.bin");
    std::fs::write(&file, vec![1u8; 4096])?;
    let event = publisher
        .publish(
            note("Too big for the full node."),
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    // The event still arrives; the blob is refused.
    eventually("event arrives", async || {
        full.store().get(&event.id).unwrap().is_some()
    })
    .await;
    let report = publisher.replicate(&event).await;
    assert!(report.is_empty(), "{report:?}");
    assert!(full.blob_bytes(&event.blobs[0].hash).await?.is_none());
    full.shutdown().await?;
    publisher.shutdown().await?;
    Ok(())
}

/// storage.max: at the limit, a node still takes events but no blobs or
/// replicas; above the hard ceiling (110 %), it refuses events from others
/// too. Its own posts are never refused.
#[tokio::test(flavor = "multi_thread")]
async fn storage_max_refuses_blobs_then_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let start = |name: &str, max: Option<u64>| {
        let mut config = NodeConfig::new(dir.path().join(name));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(0);
        config.min_pow_bits = Some(0);
        config.storage_max = max;
        Node::start(config)
    };
    // Measure what an empty node uses, then cap it exactly there: full,
    // but under the hard ceiling.
    let probe = start("full", None).await?;
    probe.shutdown().await?;
    let used = wats::node::dir_size(&dir.path().join("full"));
    let full = start("full", Some(used)).await?;
    assert_eq!(full.storage_state(), wats::node::StorageState::Full);
    let over = start("over", Some(1)).await?;
    assert_eq!(over.storage_state(), wats::node::StorageState::OverCeiling);

    let publisher = node(&dir.path().join("publisher"), &[&full, &over], 3).await?;
    publisher
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let file = dir.path().join("big.bin");
    std::fs::write(&file, vec![1u8; 4096])?;
    let event = publisher
        .publish(
            note("Storage limits."),
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    eventually("the full node takes the event", async || {
        full.store().get(&event.id).unwrap().is_some()
    })
    .await;
    let report = publisher.replicate(&event).await;
    assert!(report.is_empty(), "no replica anywhere: {report:?}");
    assert!(full.blob_bytes(&event.blobs[0].hash).await?.is_none());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        over.store().get(&event.id)?.is_none(),
        "over the hard ceiling, events from others are refused"
    );
    // Its own posts still go through.
    over.publish(note("mine"), &[]).await?;
    for n in [full, over, publisher] {
        n.shutdown().await?;
    }
    Ok(())
}

/// Per-peer rate limits: past its burst, a peer's sync requests are
/// refused (and logged), while another peer is still served.
#[tokio::test(flavor = "multi_thread")]
async fn sync_requests_are_rate_limited() -> anyhow::Result<()> {
    use wats::proto::{Request, Response, call};
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path().join("server"));
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.limits.sync_per_min = 8; // a burst of 2
    // Refusals only: challenges instead are in tests/challenge.rs.
    config.limits.challenge = false;
    let server = Node::start(config).await?;
    // Clients that pull once at start, ascending, as 0.4.0 did: the
    // newest-first fill of 0.4.1 asks a page per phase (ADR 0025 §4).
    let client = |name: &str| {
        let mut config = NodeConfig::new(dir.path().join(name));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(0);
        config.min_pow_bits = Some(0);
        config.history.legacy = true;
        config.peers = vec![server.endpoint().addr()];
        Node::start(config)
    };
    let a = client("a").await?;
    let b = client("b").await?;
    // What the server holds: it never synced, so it vouches for nothing.
    let sync = Request::sync_after(PUBLIC_NET, 0, "", 10).partial();
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(call(a.endpoint(), server.endpoint_id(), &sync).await?);
    }
    let served = answers
        .iter()
        .filter(|r| matches!(r, Response::SyncPage { .. }))
        .count();
    let refused = answers
        .iter()
        .filter(|r| matches!(r, Response::Refused { reason } if reason.contains("rate limited")))
        .count();
    // a's own catch-up sync at start may have spent a token already.
    assert!(
        served >= 1 && refused >= 2 && served + refused == 4,
        "{answers:?}"
    );
    assert!(matches!(
        call(b.endpoint(), server.endpoint_id(), &sync).await?,
        Response::SyncPage { .. }
    ));
    for n in [server, a, b] {
        n.shutdown().await?;
    }
    Ok(())
}

/// A blob is used only at the size its event declared (security review
/// B10), and is never downloaded beyond it.
#[tokio::test(flavor = "multi_thread")]
async fn blobs_must_match_their_declared_size() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = node(&dir.path().join("a"), &[], 3).await?;
    let file = dir.path().join("data.bin");
    let content: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &content)?;
    let event = a
        .publish(
            note("Data attached."),
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    let b = node(&dir.path().join("b"), &[&a], 3).await?;
    eventually("b synced", async || {
        b.store().get(&event.id).unwrap().is_some()
    })
    .await;
    let blob = event.blobs[0].clone();

    // Declared smaller than it is: only the declared chunks are fetched,
    // and the incomplete blob is not used.
    let smaller = wats::event::BlobRef {
        size: 1000,
        ..blob.clone()
    };
    assert!(b.fetch_and_export(&smaller).await?.is_none());
    assert!(b.blob_bytes(&blob.hash).await?.is_none(), "not complete");
    // Declared larger than it is: complete, but not what was declared.
    let larger = wats::event::BlobRef {
        size: 9000,
        ..blob.clone()
    };
    assert!(b.fetch_and_export(&larger).await?.is_none());
    // The true size works.
    let path = b.fetch_and_export(&blob).await?.expect("fetched from a");
    assert_eq!(std::fs::read(path)?, content);
    assert!(
        b.export_blob(&larger).await?.is_none(),
        "export checks the size too"
    );

    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// Exporting a blob never follows a symlink planted in the inbox
/// (security review D).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn blob_export_never_follows_symlinks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("a");
    let a = node(&data, &[], 3).await?;
    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"exported safely")?;
    let event = a
        .publish(
            note("Report attached."),
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    let blob = &event.blobs[0];
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere)?;
    let hash_dir = data.join("inbox").join(&blob.hash);

    // The per-hash directory is a symlink: refused, nothing written there.
    std::fs::create_dir_all(data.join("inbox"))?;
    std::os::unix::fs::symlink(&elsewhere, &hash_dir)?;
    assert!(a.export_blob(blob).await.is_err());
    assert_eq!(std::fs::read_dir(&elsewhere)?.count(), 0);
    std::fs::remove_file(&hash_dir)?;

    // The target file is a symlink: refused, its target untouched.
    std::fs::create_dir_all(&hash_dir)?;
    let victim = elsewhere.join("victim");
    std::fs::write(&victim, b"keep me")?;
    std::os::unix::fs::symlink(&victim, hash_dir.join(&blob.name))?;
    assert!(a.export_blob(blob).await.is_err());
    assert_eq!(std::fs::read(&victim)?, b"keep me");
    std::fs::remove_file(hash_dir.join(&blob.name))?;

    // A clean inbox works, read-only.
    let path = a.export_blob(blob).await?.expect("exported");
    assert_eq!(std::fs::read(&path)?, b"exported safely");
    assert!(std::fs::metadata(&path)?.permissions().readonly());
    a.shutdown().await?;
    Ok(())
}

/// More blobs pushed at once than the receiver fetches at once for one
/// peer (`limits.blob_fetches_per_peer`, 4).
const BURST: u8 = 12;

/// A blob large enough that fetching it takes a while, so pushes overlap.
fn burst_content(seed: u8) -> Vec<u8> {
    (0..256 * 1024).map(|i| (i % 251) as u8 ^ seed).collect()
}

/// Publish [`BURST`] posts with a blob each, all at once, on `publisher`.
async fn publish_burst(
    publisher: &Arc<Node>,
    dir: &std::path::Path,
) -> anyhow::Result<Vec<wats::event::Event>> {
    let mut files = Vec::new();
    for seed in 0..BURST {
        let path = dir.join(format!("burst-{seed}.bin"));
        std::fs::write(&path, burst_content(seed))?;
        files.push(path);
    }
    let publishes = files.into_iter().enumerate().map(|(i, path)| async move {
        let attachment = Attachment { path, name: None };
        publisher
            .publish(note(&format!("burst {i}")), &[attachment])
            .await
    });
    n0_future::join_all(publishes).await.into_iter().collect()
}

/// `receiver` comes to hold every blob of `events` by itself: never read
/// nor fetched here, so only a push can have brought them.
async fn all_pushed(receiver: &Node, events: &[wats::event::Event]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let mut missing = Vec::new();
        for event in events {
            if !receiver.blob_pinned(&event.blobs[0].hash).await.unwrap() {
                missing.push(event.blobs[0].hash.clone());
            }
        }
        if missing.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} of {} pushed blobs never arrived: {missing:?}",
            missing.len(),
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A burst of posts with blobs: every replica arrives through the push,
/// although the receiver fetches only a few blobs at once for one peer.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_pushes_reaches_its_replicas() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let receiver = node(&dir.path().join("receiver"), &[], 3).await?;
    let publisher = node(&dir.path().join("publisher"), &[&receiver], 3).await?;
    publisher
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let events = publish_burst(&publisher, dir.path()).await?;
    all_pushed(&receiver, &events).await;
    publisher.shutdown().await?;
    receiver.shutdown().await?;
    Ok(())
}

/// The same burst to a receiver that skips, rather than queues, the
/// fetches past its limit, as 0.4.0 and 0.4.1 do: the publisher sends no
/// more pushes at once than that limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_publisher_paces_its_pushes_to_an_older_receiver() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path().join("receiver"));
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.limits.blob_fetch_queue = 0;
    let receiver = Node::start(config).await?;
    let publisher = node(&dir.path().join("publisher"), &[&receiver], 3).await?;
    publisher
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let events = publish_burst(&publisher, dir.path()).await?;
    all_pushed(&receiver, &events).await;
    publisher.shutdown().await?;
    receiver.shutdown().await?;
    Ok(())
}

/// Pushes as a 0.4.1 publisher sends them, all at once: the receiver
/// queues those past its per-peer limit instead of dropping them.
#[tokio::test(flavor = "multi_thread")]
async fn a_receiver_queues_pushes_past_its_limit() -> anyhow::Result<()> {
    use wats::proto::{Request, Response, call_solving};
    let dir = tempfile::tempdir()?;
    let receiver = node(&dir.path().join("receiver"), &[], 3).await?;
    // k = 0: the publisher pushes nothing itself; the test does.
    let publisher = node(&dir.path().join("publisher"), &[&receiver], 0).await?;
    publisher
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let events = publish_burst(&publisher, dir.path()).await?;
    let to = receiver.endpoint_id();
    let calls = events.iter().map(|event| {
        let request = Request::Replicate {
            event: serde_json::to_value(event).unwrap(),
            hashes: vec![event.blobs[0].hash.clone()],
        };
        let endpoint = publisher.endpoint();
        async move { call_solving(endpoint, to, &request).await }
    });
    for (event, response) in events.iter().zip(n0_future::join_all(calls).await) {
        match response? {
            Response::Replicated { hashes } => {
                assert_eq!(hashes, vec![event.blobs[0].hash.clone()], "{}", event.id)
            }
            other => panic!("push of {} refused: {other:?}", event.id),
        }
    }
    all_pushed(&receiver, &events).await;
    publisher.shutdown().await?;
    receiver.shutdown().await?;
    Ok(())
}
