//! Blob durability (ADR 0029): replica repair, k = 5 for knowledge,
//! last-copy protection. Loopback nodes only (`--infra none` in spirit:
//! nothing leaves the machine).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use iroh::EndpointId;
use wats::event::{Draft, Event, Kind, parse_hex};
use wats::identity::author_id;
use wats::node::durability::{KeptLastCopy, Tuning};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::proto::closest;
use wats::store::DropReason;

/// Repair every few hundred milliseconds, every blob checked again each
/// second.
fn fast() -> Tuning {
    Tuning {
        repair: true,
        first_pass: Duration::from_millis(100),
        every: Duration::from_millis(300),
        recheck: Duration::from_secs(1),
        retry: Duration::from_millis(200),
        blobs_per_pass: 64,
        inventory_every: Duration::from_millis(100),
        ..Tuning::default()
    }
}

/// No repair in the background (passes run by hand, or none).
fn manual() -> Tuning {
    Tuning {
        repair: false,
        ..fast()
    }
}

fn config(dir: &Path, k: usize, tuning: Tuning) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.replication = k;
    config.durability = tuning;
    config
}

async fn start(dir: &Path, k: usize, tuning: Tuning) -> anyhow::Result<Arc<Node>> {
    Node::start(config(dir, k, tuning)).await
}

/// Every node knows every other one (each said hello to each).
async fn mesh(nodes: &[&Arc<Node>]) -> anyhow::Result<()> {
    for a in nodes {
        for b in nodes {
            if a.endpoint_id() != b.endpoint_id() {
                a.add_peer(b.endpoint().addr()).await?;
                a.hello(b.endpoint_id()).await?;
            }
        }
    }
    for n in nodes {
        eventually("every node knows every other one", async || {
            n.known_peers().len() >= nodes.len() - 1
        })
        .await;
    }
    Ok(())
}

async fn eventually_within(what: &str, limit: Duration, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + limit;
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn eventually(what: &str, check: impl AsyncFnMut() -> bool) {
    eventually_within(what, Duration::from_secs(30), check).await;
}

async fn holds(node: &Node, hash: &str) -> bool {
    node.blob_bytes(hash).await.ok().flatten().is_some()
}

fn note(text: &str) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some("a file".into()),
        text: text.into(),
        ..Draft::default()
    }
}

/// 4 KiB that hash differently for every seed.
fn content(seed: u64) -> Vec<u8> {
    let mut bytes = seed.to_le_bytes().to_vec();
    bytes.resize(4096, 7);
    bytes
}

fn hash_of(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// Publish `bytes` as an attachment of a note by `node`.
async fn publish(node: &Arc<Node>, dir: &Path, bytes: &[u8]) -> anyhow::Result<Event> {
    let hex = blake3::hash(bytes).to_hex();
    let file = dir.join(format!("f{}.bin", &hex[..16]));
    std::fs::write(&file, bytes)?;
    node.publish(
        note("a file to keep"),
        &[Attachment {
            path: file,
            name: None,
        }],
    )
    .await
}

/// Content whose 4 closest nodes among `ids` leave `far` out, and those
/// 4, nearest first. Always found: `far` is last for a hash that is its
/// complement, so about one seed in 2^(bits that set it apart) works.
fn far_from(ids: &[EndpointId], far: EndpointId, first_seed: u64) -> (Vec<u8>, Vec<EndpointId>) {
    (first_seed..first_seed + 1_000_000)
        .map(|seed| {
            let bytes = content(seed);
            let order = closest(&hash_of(&bytes), ids, 4);
            (bytes, order)
        })
        .find(|(_, order)| !order.contains(&far))
        .expect("some content leaves it out")
}

fn ids(nodes: &[&Arc<Node>]) -> Vec<EndpointId> {
    nodes.iter().map(|n| n.endpoint_id()).collect()
}

fn by_id<'a>(nodes: &[&'a Arc<Node>], id: EndpointId) -> &'a Arc<Node> {
    nodes
        .iter()
        .find(|n| n.endpoint_id() == id)
        .expect("one of the nodes")
}

/// The publisher and one of the three replicas disappear: the two left
/// push the blob to the next closest node, within a deadline, so it has
/// its 3 replicas again. Repeated in a loop by the release checks
/// (ADR 0029, "Test evidence").
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_replica_is_restored() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut nodes = Vec::new();
    for i in 0..6 {
        nodes.push(start(&dir.path().join(format!("n{i}")), 3, fast()).await?);
    }
    let all: Vec<&Arc<Node>> = nodes.iter().collect();
    mesh(&all).await?;
    let publisher = all[0];
    let others: Vec<&Arc<Node>> = all[1..].to_vec();
    // A file whose 4 closest nodes do not include the publisher: its 3
    // replicas, then the node that must take the lost one's place.
    let (bytes, order) = far_from(&ids(&all), publisher.endpoint_id(), 0);
    let event = publish(publisher, dir.path(), &bytes).await?;
    let hash = event.blobs[0].hash.clone();
    for id in &order[..3] {
        let n = by_id(&others, *id);
        eventually("the 3 closest hold the blob", async || {
            holds(n, &hash).await
        })
        .await;
    }
    let next = by_id(&others, order[3]);
    assert!(!holds(next, &hash).await, "the 4th closest has no copy yet");

    let lost = by_id(&others, order[0]);
    lost.shutdown().await?;
    publisher.shutdown().await?;
    // The two replicas left find the lost one unreachable and push to the
    // next closest.
    eventually_within(
        "the 4th closest takes the lost replica",
        Duration::from_secs(60),
        async || holds(next, &hash).await,
    )
    .await;
    for id in &order[1..4] {
        assert!(holds(by_id(&others, *id), &hash).await);
    }
    let added: u64 = [order[1], order[2]]
        .iter()
        .map(|id| by_id(&others, *id).repair_stats().added)
        .sum();
    assert!(added >= 1, "a replica was added by repair");
    for n in &others {
        if n.endpoint_id() != lost.endpoint_id() {
            n.shutdown().await?;
        }
    }
    Ok(())
}

/// A blob a knowledge entry lists (a digest accepted by the asker, ADR
/// 0019 §5.2) reaches 5 replicas; a blob of an ordinary post stays at 3.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_of_knowledge_reaches_five_replicas() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut nodes = Vec::new();
    for i in 0..7 {
        nodes.push(start(&dir.path().join(format!("n{i}")), 3, fast()).await?);
    }
    let all: Vec<&Arc<Node>> = nodes.iter().collect();
    mesh(&all).await?;
    let publisher = all[0];
    let others: Vec<&Arc<Node>> = all[1..].to_vec();
    let asker = SigningKey::from_bytes(&[21; 32]);
    let curator = SigningKey::from_bytes(&[22; 32]);

    let ask = publisher
        .publish_as(
            &asker,
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Ask),
                title: Some("Rust on musl fails to link".into()),
                text: "How?".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let helper = SigningKey::from_bytes(&[23; 32]);
    let answer = publisher
        .publish_as(
            &helper,
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(ask.id.clone()),
                parent: Some(ask.id.clone()),
                kind: Some(Kind::Answer),
                text: "Install musl-tools.".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let file = dir.path().join("detail.txt");
    std::fs::write(&file, vec![b'd'; 8192])?;
    let digest = publisher
        .publish_as(
            &curator,
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(ask.id.clone()),
                parent: Some(ask.id.clone()),
                refs: vec![answer.id.clone()],
                kind: Some(Kind::Ext(wats::knowledge::DIGEST_KIND.into())),
                title: Some("Rust: linking on musl needs musl-tools".into()),
                text: "Install musl-tools before cargo build.".into(),
                ..Draft::default()
            },
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    // The asker accepts it: the digest is a knowledge entry everywhere.
    publisher
        .publish_as(
            &asker,
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(ask.id.clone()),
                parent: Some(digest.id.clone()),
                kind: Some(Kind::React),
                text: "solved it".into(),
                data: serde_json::json!({"accept": true})
                    .as_object()
                    .unwrap()
                    .clone(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let plain = publish(publisher, dir.path(), &content(99)).await?;
    for n in &others {
        eventually("the posts reach every node", async || {
            n.store().count().unwrap_or(0) >= 5
        })
        .await;
    }

    let knowledge = digest.blobs[0].hash.clone();
    let five = closest(&parse_hex::<32>(&knowledge, "hash")?, &ids(&all), 5);
    for id in &five {
        let n = by_id(&all, *id);
        eventually("the 5 closest hold the blob of knowledge", async || {
            holds(n, &knowledge).await
        })
        .await;
    }
    // The plain blob: 3 replicas besides the publisher, and no more after
    // several passes.
    let ordinary = plain.blobs[0].hash.clone();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut held = 0;
    for n in &others {
        if holds(n, &ordinary).await {
            held += 1;
        }
    }
    assert_eq!(held, 3, "an ordinary blob keeps k = 3");
    let mut held = 0;
    for n in &others {
        if holds(n, &knowledge).await {
            held += 1;
        }
    }
    assert!(held >= 4, "5 replicas, the publisher maybe one: {held}");
    // The helper rotation shares (ADR 0025 PR B) sees the same k.
    let holder = by_id(&all, five[0]);
    assert_eq!(holder.blob_target_k(&knowledge), 5);
    assert_eq!(holder.blob_target_k(&ordinary), 3);
    for n in nodes {
        n.shutdown().await?;
    }
    Ok(())
}

/// Last-copy protection: a node that holds the only copy it knows of
/// refuses to evict it, and the collection keeps it; once another node
/// takes it, the eviction goes through and the collection deletes it.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_copy_is_handed_off_before_it_is_deleted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gc = |name: &str| {
        let mut c = config(&dir.path().join(name), 3, manual());
        c.blob_gc_interval = Duration::from_millis(200);
        c
    };
    let publisher = Node::start(gc("p")).await?;
    let holder = Node::start(gc("b")).await?;
    mesh(&[&publisher, &holder]).await?;
    let event = publish(&publisher, dir.path(), &content(1)).await?;
    let hash = event.blobs[0].hash.clone();
    eventually("the replica arrives", async || holds(&holder, &hash).await).await;
    publisher.shutdown().await?;

    // The only other holder is gone: refused, nothing changes.
    let refused = holder
        .drop_body(&event.id, DropReason::Evicted, None, None)
        .await
        .expect_err("may be the last copy");
    let kept = refused.downcast_ref::<KeptLastCopy>().expect("typed");
    assert_eq!(kept.blobs, std::slice::from_ref(&hash));
    assert!(holder.store().get(&event.id)?.is_some(), "body kept");
    let runs = holder.blob_collections();
    eventually("two collections run", async || {
        holder.blob_collections() >= runs + 2
    })
    .await;
    assert!(holds(&holder, &hash).await, "the collection kept it");
    assert_eq!(holder.repair_stats().kept, 1);

    // Another node appears: it takes the blob, then the eviction proceeds.
    let taker = Node::start(gc("c")).await?;
    mesh(&[&holder, &taker]).await?;
    let outcome = holder
        .drop_body(&event.id, DropReason::Evicted, None, None)
        .await?;
    assert!(outcome.body_dropped);
    assert_eq!(outcome.freed_blobs, std::slice::from_ref(&hash));
    assert!(holds(&taker, &hash).await, "handed off first");
    assert_eq!(holder.repair_stats().handed_off, 1);
    // Untagged, for the next collection. (Not waited for: an `evicted`
    // body is taken back from the next sync with the taker, ADR 0020, and
    // its blob is kept again with it.)
    assert!(!holder.blob_pinned(&hash).await?, "released here");

    // `wats drop` with no node running: refused while it may lose a copy,
    // done with --purge.
    let mine = publish(&taker, dir.path(), &content(2)).await?;
    taker.shutdown().await?;
    holder.shutdown().await?;
    let data = dir.path().join("c");
    let refused = wats::node::drop_body_offline(&data, &mine.id, DropReason::Own, None, false)
        .await
        .expect_err("no node to hand it to");
    assert!(
        refused.downcast_ref::<KeptLastCopy>().is_some(),
        "{refused:#}"
    );
    let outcome =
        wats::node::drop_body_offline(&data, &mine.id, DropReason::Own, None, true).await?;
    assert!(outcome.body_dropped);
    assert_eq!(outcome.freed_blobs, [mine.blobs[0].hash.clone()]);
    Ok(())
}

/// Retractions and erasure labels delete regardless (ADR 0020): the
/// content must not be kept, last copy or not.
#[tokio::test(flavor = "multi_thread")]
async fn erasure_and_retraction_delete_the_last_copy() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let labeler = SigningKey::from_bytes(&[31; 32]);
    let mut c = config(&dir.path().join("alone"), 3, manual());
    c.blob_gc_interval = Duration::from_millis(200);
    c.erasure_labelers = vec![author_id(&labeler)];
    let alone = Node::start(c).await?;

    // An erasure label from a subscribed labeler.
    let target = publish(&alone, dir.path(), &content(3)).await?;
    let erased = target.blobs[0].hash.clone();
    assert!(holds(&alone, &erased).await);
    alone
        .publish_as(
            &labeler,
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(target.thread_root().to_string()),
                parent: Some(target.id.clone()),
                kind: Some(Kind::React),
                text: "Erasure request handled by the operator.".into(),
                data: serde_json::json!({"label": "erasure"})
                    .as_object()
                    .unwrap()
                    .clone(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    // A retraction by the author.
    let retracted = publish(&alone, dir.path(), &content(4)).await?;
    let gone = retracted.blobs[0].hash.clone();
    alone
        .retract_as(
            alone.default_author(),
            std::slice::from_ref(&retracted.id),
            None,
        )
        .await?;
    eventually("both are deleted, the only copies", async || {
        !holds(&alone, &erased).await && !holds(&alone, &gone).await
    })
    .await;
    assert_eq!(alone.repair_stats().kept, 0);
    assert_eq!(alone.repair_stats().asked, 0, "nobody asked to take them");
    // An eviction of the same kind of post is refused.
    let evicted = publish(&alone, dir.path(), &content(5)).await?;
    assert!(
        alone
            .drop_body(&evicted.id, DropReason::Evicted, None, None)
            .await
            .is_err()
    );
    alone.shutdown().await?;
    Ok(())
}

/// Many blobs, and a node that comes back: no storm. Its checks spread
/// over `recheck`, so the first seconds ask almost nothing; and even with
/// every blob due at once, a pass checks at most `blobs_per_pass` blobs
/// and sends at most 16 requests to any one peer.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_rejoins_starts_no_repair_storm() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut nodes = Vec::new();
    for i in 0..4 {
        nodes.push(start(&dir.path().join(format!("n{i}")), 3, manual()).await?);
    }
    let all: Vec<&Arc<Node>> = nodes.iter().collect();
    mesh(&all).await?;
    let publisher = all[0];
    let blobs = 40;
    let mut hashes = Vec::new();
    for seed in 0..blobs {
        hashes.push(
            publish(publisher, dir.path(), &content(100 + seed))
                .await?
                .blobs[0]
                .hash
                .clone(),
        );
    }
    // k = 3 and 3 other nodes: each holds every blob.
    for n in &all[1..] {
        for hash in &hashes {
            eventually("every replica arrives", async || holds(n, hash).await).await;
        }
    }
    let served = |nodes: &[&Arc<Node>]| -> Vec<u64> {
        nodes.iter().map(|n| n.repair_stats().served).collect()
    };

    // It comes back, repairing every 200 ms, each blob checked once an
    // hour: due at random times within that hour.
    let back_dir = dir.path().join("n1");
    nodes[1].shutdown().await?;
    let rejoin = Tuning {
        first_pass: Duration::from_millis(100),
        every: Duration::from_millis(200),
        recheck: Duration::from_secs(3600),
        ..fast()
    };
    let back = start(&back_dir, 3, rejoin).await?;
    let stay: Vec<&Arc<Node>> = vec![&nodes[0], &nodes[2], &nodes[3]];
    mesh(&[&back, stay[0], stay[1], stay[2]]).await?;
    let before = served(&stay);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let stats = back.repair_stats();
    assert!(stats.passes >= 5, "it runs passes: {stats:?}");
    let asked: u64 = served(&stay)
        .iter()
        .zip(&before)
        .map(|(after, before)| after - before)
        .sum();
    assert!(
        asked < blobs,
        "{asked} requests in 3 s for {blobs} blobs: checks are spread out"
    );
    back.shutdown().await?;

    // The worst case: it comes back with every blob due now.
    let due = Tuning {
        recheck: Duration::ZERO,
        retry: Duration::ZERO,
        blobs_per_pass: 30,
        ..manual()
    };
    let back = start(&back_dir, 3, due).await?;
    mesh(&[&back, stay[0], stay[1], stay[2]]).await?;
    let before = served(&stay);
    let checked = back.repair_pass().await;
    assert!(checked <= 30, "at most blobs_per_pass: {checked}");
    let after = served(&stay);
    for (after, before) in after.iter().zip(&before) {
        assert!(
            after - before <= 16,
            "at most 16 requests to one peer: {}",
            after - before
        );
    }
    let stats = back.repair_stats();
    assert!(stats.asked <= 30 * (2 + 3), "{stats:?}");
    back.shutdown().await?;
    for n in stay {
        n.shutdown().await?;
    }
    Ok(())
}

/// A peer of 0.4.2 (no repair, `replicate` answered the old way) is a
/// valid repair target, and never repairs anything itself.
#[tokio::test(flavor = "multi_thread")]
async fn an_older_peer_takes_a_lost_replica() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut nodes = Vec::new();
    for i in 0..5 {
        nodes.push(start(&dir.path().join(format!("n{i}")), 3, fast()).await?);
    }
    let publisher = Arc::clone(&nodes[0]);
    let everyone = ids(&nodes.iter().collect::<Vec<_>>());
    let (bytes, order) = far_from(&everyone, publisher.endpoint_id(), 1000);
    // The 4th closest becomes a node of 0.4.2: restarted on its data
    // directory (same id) with the old behaviour.
    let at = nodes
        .iter()
        .position(|n| n.endpoint_id() == order[3])
        .expect("one of the nodes");
    nodes[at].shutdown().await?;
    let legacy = Tuning {
        repair: false,
        legacy_replicate: true,
        ..fast()
    };
    nodes[at] = start(&dir.path().join(format!("n{at}")), 3, legacy).await?;
    let all: Vec<&Arc<Node>> = nodes.iter().collect();
    mesh(&all).await?;
    let old = all[at];
    assert_eq!(old.endpoint_id(), order[3]);

    let event = publish(&publisher, dir.path(), &bytes).await?;
    let hash = event.blobs[0].hash.clone();
    for id in &order[..3] {
        let n = by_id(&all, *id);
        eventually("the 3 closest hold the blob", async || {
            holds(n, &hash).await
        })
        .await;
    }
    by_id(&all, order[0]).shutdown().await?;
    publisher.shutdown().await?;
    eventually_within(
        "the old node takes the lost replica",
        Duration::from_secs(60),
        async || holds(old, &hash).await,
    )
    .await;
    // Asked again at the next checks, it confirms the old way.
    let watcher = by_id(&all, order[1]);
    let confirmed = watcher.repair_stats().confirmed;
    eventually("the old node confirms again", async || {
        watcher.repair_stats().confirmed > confirmed + 1
    })
    .await;
    assert_eq!(old.repair_stats().passes, 0, "an old node does not repair");
    for id in &order[1..4] {
        by_id(&all, *id).shutdown().await?;
    }
    Ok(())
}
