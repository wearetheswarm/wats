//! Blob durability (ADR 0029): replica repair, k = 5 for knowledge,
//! last-copy protection with possession proofs. Loopback nodes only
//! (`--infra none` in spirit: nothing leaves the machine).

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
/// second, a peer replaced after two misses a second apart.
fn fast() -> Tuning {
    Tuning {
        repair: true,
        first_pass: Duration::from_millis(100),
        every: Duration::from_millis(300),
        recheck: Duration::from_secs(1),
        retry: Duration::from_millis(200),
        blobs_per_pass: 64,
        inventory_every: Duration::from_millis(100),
        miss_gap: Duration::from_secs(1),
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
    config.blob_gc_interval = Duration::from_millis(200);
    config
}

async fn start(dir: &Path, k: usize, tuning: Tuning) -> anyhow::Result<Arc<Node>> {
    Node::start(config(dir, k, tuning)).await
}

/// Every node knows every other one (each said hello to each, and joined
/// it on gossip).
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

/// Publish `bytes` as an attachment of a note by `node` (as `key`, or its
/// default profile).
async fn publish_as(
    node: &Arc<Node>,
    key: Option<&SigningKey>,
    dir: &Path,
    bytes: &[u8],
) -> anyhow::Result<Event> {
    let hex = blake3::hash(bytes).to_hex();
    let file = dir.join(format!("f{}.bin", &hex[..16]));
    std::fs::write(&file, bytes)?;
    let attachments = [Attachment {
        path: file,
        name: None,
    }];
    match key {
        Some(key) => {
            node.publish_as(key, note("a file to keep"), &attachments)
                .await
        }
        None => node.publish(note("a file to keep"), &attachments).await,
    }
}

async fn publish(node: &Arc<Node>, dir: &Path, bytes: &[u8]) -> anyhow::Result<Event> {
    publish_as(node, None, dir, bytes).await
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

async fn copies(nodes: &[&Arc<Node>], hash: &str) -> usize {
    let mut n = 0;
    for node in nodes {
        if holds(node, hash).await {
            n += 1;
        }
    }
    n
}

/// The publisher and one of the three replicas disappear: the two left
/// push the blob to the next closest node, once the lost one missed twice
/// a second apart, within a deadline. Repeated in a loop by the release
/// checks (ADR 0029, "Test evidence").
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
        assert!(
            n.has_replica_intent(&hash.parse()?).await,
            "recorded as a replica"
        );
    }
    let next = by_id(&others, order[3]);
    assert!(!holds(next, &hash).await, "the 4th closest has no copy yet");

    let lost = by_id(&others, order[0]);
    lost.shutdown().await?;
    publisher.shutdown().await?;
    eventually_within(
        "the 4th closest takes the lost replica",
        Duration::from_secs(60),
        async || holds(next, &hash).await,
    )
    .await;
    assert!(next.has_replica_intent(&hash.parse()?).await);
    for id in &order[1..4] {
        assert!(holds(by_id(&others, *id), &hash).await);
    }
    let survivors: Vec<&Arc<Node>> = others
        .iter()
        .copied()
        .filter(|n| n.endpoint_id() != lost.endpoint_id())
        .collect();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        copies(&survivors, &hash).await,
        3,
        "no more than 3: no overshoot"
    );
    let added: u64 = [order[1], order[2]]
        .iter()
        .map(|id| by_id(&others, *id).repair_stats().added)
        .sum();
    assert!(added >= 1, "a replica was added by repair");
    for n in survivors {
        n.shutdown().await?;
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
    let helper = SigningKey::from_bytes(&[23; 32]);

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
    assert_eq!(
        copies(&others, &ordinary).await,
        3,
        "an ordinary blob keeps k = 3"
    );
    let held = copies(&others, &knowledge).await;
    assert!(
        (4..=5).contains(&held),
        "5 replicas, the publisher maybe one: {held}"
    );
    // The helper rotation shares (ADR 0025 PR B) sees the same k.
    let holder = by_id(&all, five[0]);
    assert_eq!(holder.blob_target_k(&knowledge).await, 5);
    assert_eq!(holder.blob_target_k(&ordinary).await, 3);
    for n in nodes {
        n.shutdown().await?;
    }
    Ok(())
}

/// Last-copy protection. A copy fetched on read, evicted while no other
/// holder is reachable: refused, and the collection keeps it; once a
/// holder is back and proves it holds the blob, the eviction goes through.
/// A replica (it came by `replicate`) stays when its body is evicted.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_copy_is_handed_off_before_it_is_deleted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let publisher = start(&dir.path().join("p"), 1, manual()).await?;
    // k = 3 on the holder: among 3 nodes it is always within the 2k
    // closest, so its replica stays whatever the hashes (§4). The others
    // have k = 1: one proven holder releases a copy.
    let holder = start(&dir.path().join("b"), 3, manual()).await?;
    mesh(&[&publisher, &holder]).await?;
    let event = publish(&publisher, dir.path(), &content(1)).await?;
    let blob = event.blobs[0].clone();
    let hash = blob.hash.clone();
    eventually("the replica arrives", async || holds(&holder, &hash).await).await;
    assert!(holder.has_replica_intent(&hash.parse()?).await, "recorded");
    // A reader fetches it on read: a copy, not a replica.
    let reader = start(&dir.path().join("r"), 1, manual()).await?;
    mesh(&[&publisher, &holder, &reader]).await?;
    eventually("the post reaches the reader", async || {
        reader.store().get(&event.id).ok().flatten().is_some()
    })
    .await;
    reader.fetch_and_export(&blob).await?.expect("fetched");
    assert!(!reader.has_replica_intent(&hash.parse()?).await);
    publisher.shutdown().await?;
    holder.shutdown().await?;

    // No other holder reachable: refused, nothing changes.
    let refused = reader
        .drop_body(&event.id, DropReason::Evicted, None, None)
        .await
        .expect_err("may be the last copy");
    let kept = refused.downcast_ref::<KeptLastCopy>().expect("typed");
    assert_eq!(kept.blobs, std::slice::from_ref(&hash));
    assert!(reader.store().get(&event.id)?.is_some(), "body kept");
    let runs = reader.blob_collections();
    eventually("two collections run", async || {
        reader.blob_collections() >= runs + 2
    })
    .await;
    assert!(holds(&reader, &hash).await, "the collection kept it");
    assert_eq!(reader.repair_stats().kept, 1);

    // The replica's node comes back: it proves it holds the blob, the
    // eviction proceeds.
    let holder = start(&dir.path().join("b"), 3, manual()).await?;
    mesh(&[&reader, &holder]).await?;
    let outcome = reader
        .drop_body(&event.id, DropReason::Evicted, None, None)
        .await?;
    assert!(outcome.body_dropped);
    assert_eq!(outcome.freed_blobs, std::slice::from_ref(&hash));
    let stats = reader.repair_stats();
    assert_eq!((stats.handed_off, stats.confirmed), (1, 1), "{stats:?}");
    // Untagged, for the next collection. (Not waited for: an `evicted`
    // body may come back by sync, ADR 0020, with its blob.)
    assert!(!reader.blob_pinned(&hash).await?, "released here");

    // The replica stays when its body is evicted.
    let outcome = holder
        .drop_body(&event.id, DropReason::Evicted, None, None)
        .await?;
    assert!(outcome.body_dropped && outcome.freed_blobs.is_empty());
    assert!(holder.blob_pinned(&hash).await?, "the replica is kept");
    // Asked again by a repairer, it confirms without taking the body back.
    assert!(
        holder.store().get(&event.id)?.is_none(),
        "still a tombstone"
    );
    reader.shutdown().await?;
    holder.shutdown().await?;
    Ok(())
}

/// A peer that says it holds a blob without holding it does not make a
/// node release its last copy: the possession proof fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_lying_peer_does_not_release_the_last_copy() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let liar = Tuning {
        lie_replicate: true,
        ..manual()
    };
    let reader = start(&dir.path().join("r"), 1, manual()).await?;
    let event = publish(&reader, dir.path(), &content(7)).await?;
    let hash = event.blobs[0].hash.clone();
    // The reader's own copy is not a replica: evicting it needs another
    // holder. The only peer lies.
    let liar = start(&dir.path().join("l"), 1, liar).await?;
    mesh(&[&reader, &liar]).await?;
    let refused = reader
        .drop_body(&event.id, DropReason::Evicted, None, None)
        .await
        .expect_err("the liar proves nothing");
    assert!(
        refused.downcast_ref::<KeptLastCopy>().is_some(),
        "{refused:#}"
    );
    assert!(reader.blob_pinned(&hash).await?);
    assert!(!holds(&liar, &hash).await, "it never fetched");
    let stats = reader.repair_stats();
    assert!(stats.proofs_failed >= 1, "{stats:?}");
    assert_eq!(stats.handed_off, 0);
    reader.shutdown().await?;
    liar.shutdown().await?;
    Ok(())
}

/// A replica kept through an eviction is released by a later erasure
/// label or retraction of its post (B1), and its record goes with it.
#[tokio::test(flavor = "multi_thread")]
async fn an_evicted_replica_is_released_by_erasure_or_retraction() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let labeler = SigningKey::from_bytes(&[41; 32]);
    let author = SigningKey::from_bytes(&[42; 32]);
    let mut c = config(&dir.path().join("b"), 1, manual());
    c.erasure_labelers = vec![author_id(&labeler)];
    let holder = Node::start(c).await?;
    let publisher = start(&dir.path().join("p"), 1, manual()).await?;
    mesh(&[&publisher, &holder]).await?;
    let erased = publish_as(&publisher, Some(&author), dir.path(), &content(11)).await?;
    let retracted = publish_as(&publisher, Some(&author), dir.path(), &content(12)).await?;
    for e in [&erased, &retracted] {
        let hash = e.blobs[0].hash.clone();
        eventually("the replica arrives", async || holds(&holder, &hash).await).await;
        let outcome = holder
            .drop_body(&e.id, DropReason::Evicted, None, None)
            .await?;
        assert!(outcome.freed_blobs.is_empty(), "the replica is kept");
        assert!(holder.blob_pinned(&hash).await?);
    }
    // The erasure label, then the retraction, both by gossip.
    publisher
        .publish_as(
            &labeler,
            Draft {
                net: PUBLIC_NET.into(),
                thread: Some(erased.thread_root().to_string()),
                parent: Some(erased.id.clone()),
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
    publisher
        .retract_as(&author, std::slice::from_ref(&retracted.id), None)
        .await?;
    for (e, reason) in [
        (&erased, DropReason::Erasure),
        (&retracted, DropReason::Retraction),
    ] {
        let hash = e.blobs[0].hash.clone();
        eventually("the replica is released", async || {
            !holder.blob_pinned(&hash).await.unwrap_or(true)
        })
        .await;
        assert!(
            !holder.has_replica_intent(&hash.parse()?).await,
            "record gone"
        );
        assert_eq!(holder.store().drop_record(&e.id)?.unwrap().reason, reason);
        eventually("the collection deletes it", async || {
            !holds(&holder, &hash).await
        })
        .await;
    }
    holder.shutdown().await?;
    publisher.shutdown().await?;
    Ok(())
}

/// Retractions, erasure labels and `wats drop` delete at once, last copy
/// or not, with a peer there to take it: nobody is asked (ADR 0020).
#[tokio::test(flavor = "multi_thread")]
async fn erasure_retraction_and_wats_drop_delete_at_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let labeler = SigningKey::from_bytes(&[31; 32]);
    let mut c = config(&dir.path().join("a"), 3, manual());
    c.erasure_labelers = vec![author_id(&labeler)];
    let alone = Node::start(c).await?;
    // A peer that could take them, and must not be asked. It joins after
    // the posts, so it got no push at publish time.
    let target = publish(&alone, dir.path(), &content(3)).await?;
    let retracted = publish(&alone, dir.path(), &content(4)).await?;
    let dropped = publish(&alone, dir.path(), &content(5)).await?;
    let peer = start(&dir.path().join("peer"), 3, manual()).await?;
    mesh(&[&alone, &peer]).await?;

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
    alone
        .retract_as(
            alone.default_author(),
            std::slice::from_ref(&retracted.id),
            None,
        )
        .await?;
    // `wats drop` through the running node's socket: at once, no hand-off.
    let _sessions = wats::attach::serve(Arc::clone(&alone))?;
    let outcome = wats::attach::control(
        &dir.path().join("a"),
        wats::attach::Control::Drop {
            id: dropped.id.clone(),
            detail: Some("personal data".into()),
        },
    )
    .await?
    .expect("the node answers");
    assert_eq!(outcome["body_dropped"], true, "{outcome}");
    assert_eq!(
        outcome["freed_blobs"],
        serde_json::json!([dropped.blobs[0].hash])
    );
    for e in [&target, &retracted, &dropped] {
        let hash = e.blobs[0].hash.clone();
        eventually("deleted here, the only copy", async || {
            !holds(&alone, &hash).await
        })
        .await;
        assert!(!holds(&peer, &hash).await, "never handed to the peer");
    }
    let stats = alone.repair_stats();
    assert_eq!((stats.asked, stats.kept), (0, 0), "nobody asked: {stats:?}");
    assert_eq!(peer.repair_stats().served, 0);
    alone.shutdown().await?;
    peer.shutdown().await?;
    Ok(())
}

/// Many blobs, and a node that comes back: no storm. Its checks spread
/// over `recheck`, so the first seconds ask almost nothing; and even with
/// every blob due at once, a pass checks at most `blobs_per_pass` blobs,
/// sends at most 16 requests to any one peer, and adds no copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_rejoins_starts_no_repair_storm() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut nodes = Vec::new();
    for i in 0..6 {
        nodes.push(start(&dir.path().join(format!("n{i}")), 3, manual()).await?);
    }
    let all: Vec<&Arc<Node>> = nodes.iter().collect();
    mesh(&all).await?;
    let publisher = all[0];
    let blobs = 60;
    let mut hashes = Vec::new();
    for seed in 0..blobs {
        let event = publish(publisher, dir.path(), &content(100 + seed)).await?;
        hashes.push(event.blobs[0].hash.clone());
    }
    // 3 replicas each among 5 nodes: 36 per node on average, more than
    // the 16 requests one pass sends to one peer.
    let others: Vec<&Arc<Node>> = all[1..].to_vec();
    for hash in &hashes {
        eventually("every replica arrives", async || {
            copies(&others, hash).await == 3
        })
        .await;
    }
    let served = |nodes: &[&Arc<Node>]| -> Vec<u64> {
        nodes.iter().map(|n| n.repair_stats().served).collect()
    };

    // One of them comes back, repairing every 200 ms, each blob checked
    // once an hour: due at random times within that hour.
    let back_at = 1;
    let back_dir = dir.path().join(format!("n{back_at}"));
    nodes[back_at].shutdown().await?;
    let rejoin = Tuning {
        first_pass: Duration::from_millis(100),
        every: Duration::from_millis(200),
        recheck: Duration::from_secs(3600),
        ..fast()
    };
    let back = start(&back_dir, 3, rejoin).await?;
    let stay: Vec<&Arc<Node>> = all
        .iter()
        .copied()
        .filter(|n| !Arc::ptr_eq(n, all[back_at]))
        .collect();
    let mut with_back = stay.clone();
    with_back.push(&back);
    mesh(&with_back).await?;
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
        blobs_per_pass: 50,
        ..manual()
    };
    let back = start(&back_dir, 3, due).await?;
    let mut with_back = stay.clone();
    with_back.push(&back);
    mesh(&with_back).await?;
    for _ in 0..3 {
        let before = served(&stay);
        let checked = back.repair_pass().await;
        assert!(checked <= 50, "at most blobs_per_pass: {checked}");
        for (after, before) in served(&stay).iter().zip(&before) {
            assert!(
                after - before <= 16,
                "at most 16 requests to one peer: {}",
                after - before
            );
        }
    }
    // Over budget, a check ends early: it never moves to a farther peer.
    let mut everyone = stay.clone();
    everyone.push(&back);
    let others: Vec<&Arc<Node>> = everyone
        .iter()
        .copied()
        .filter(|n| n.endpoint_id() != publisher.endpoint_id())
        .collect();
    for hash in &hashes {
        assert_eq!(copies(&others, hash).await, 3, "no copy added for {hash}");
    }
    back.shutdown().await?;
    for n in stay {
        n.shutdown().await?;
    }
    Ok(())
}

/// A peer of 0.4.2 (no repair, `replicate` answered the old way, nothing
/// recorded) is a valid repair target, proves what it holds, and never
/// repairs anything itself.
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
    assert!(
        !old.has_replica_intent(&hash.parse()?).await,
        "0.4.2 records nothing"
    );
    // Asked again at the next checks, it confirms the old way and proves it.
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
