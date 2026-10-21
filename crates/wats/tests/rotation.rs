//! Rotation at the cap (ADR 0025 §7): a node at its events share keeps
//! taking new posts, under its share, with its own posts, the threads it
//! joined and the kept-forever kinds intact; the cut it raises is served
//! honestly; a restart halfway is safe; blobs follow responsibility.
//! Hermetic: in-process nodes on loopback.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use wats::event::{Draft, Event, Kind};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET, now_secs};
use wats::proto::{Request, Response};
use wats::store::{DropReason, RangeState, Store};

const DAY: u64 = 86_400;
const MIB: u64 = 1 << 20;

fn config(dir: &std::path::Path, peers: &[&Arc<Node>]) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.infra = wats::infra::Infra::NONE;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    config.history.page = 64;
    config.history.retry = Duration::from_secs(2);
    // The tests drive rotation themselves, unless they say otherwise.
    config.history.rotation_check = Duration::from_secs(3600);
    config.history.rotation_pause = Duration::ZERO;
    config
}

/// A node that holds the history of `public` as if in sync all along: no
/// range pending, so rotation may raise its cut.
fn in_sync(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    Store::open(&dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now_secs() + DAY)?;
    Ok(())
}

fn key(n: u32) -> SigningKey {
    let mut bytes = [9u8; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    SigningKey::from_bytes(&bytes)
}

fn own_key(dir: &std::path::Path) -> anyhow::Result<SigningKey> {
    std::fs::create_dir_all(dir)?;
    Ok(
        wats::identity::RootSecret::load_or_create(&dir.join("secret.key"))?
            .profile_key(wats::identity::DEFAULT_PROFILE),
    )
}

fn sign(draft: Draft, key: &SigningKey, ts: u64) -> Event {
    wats::halt::sign(draft, key, ts, Some(0)).unwrap()
}

/// A note whose body is worth dropping (`long`) or not.
fn note(n: u32, by: u32, ts: u64, long: bool) -> Event {
    let text = if long {
        format!("note {n}: {}", "the swarm remembers. ".repeat(70))
    } else {
        format!("note {n}: short")
    };
    sign(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(Kind::Note),
            title: Some(format!("note {n}")),
            text,
            ..Draft::default()
        },
        &key(by),
        ts,
    )
}

fn root(kind: Kind, title: &str, by: &SigningKey, ts: u64) -> Event {
    sign(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(kind),
            title: Some(title.into()),
            text: format!("{title}: {}", "a long body worth dropping. ".repeat(60)),
            ..Draft::default()
        },
        by,
        ts,
    )
}

fn reply(parent: &Event, kind: Kind, data: Value, by: &SigningKey, ts: u64) -> Event {
    sign(
        Draft {
            net: PUBLIC_NET.into(),
            thread: Some(parent.thread_root().to_string()),
            parent: Some(parent.id.clone()),
            kind: Some(kind),
            text: format!("a reply: {}", "with a long body. ".repeat(80)),
            data: data.as_object().cloned().unwrap_or_default(),
            ..Draft::default()
        },
        by,
        ts,
    )
}

fn hold(store: &Store, events: &[Event]) {
    for event in events {
        store.insert(event, event.ts).unwrap();
    }
}

/// What rotation never drops, held by a node whose own key is `own`:
/// returns them, oldest of all.
fn protected(own: &SigningKey, ts: u64) -> Vec<Event> {
    let announce = root(Kind::Announce, "I review Rust", &key(1), ts);
    let vouch = reply(&announce, Kind::Vouch, json!({}), &key(2), ts + 1);
    let mine = root(Kind::Note, "my own note", own, ts + 2);
    let theirs = root(Kind::Ask, "a question I answered", &key(3), ts + 3);
    let my_answer = reply(&theirs, Kind::Answer, json!({}), own, ts + 4);
    let other_answer = reply(&theirs, Kind::Answer, json!({}), &key(4), ts + 5);
    vec![announce, vouch, mine, theirs, my_answer, other_answer]
}

/// Every protected event is held in full.
fn assert_protected(node: &Node, events: &[Event]) {
    for event in events {
        let held = node.store().get(&event.id).unwrap();
        assert!(held.is_some(), "{} {} kept in full", event.kind, event.id);
    }
}

/// No unprotected row of `public` is left strictly below the cut.
fn assert_cut_honest(node: &Node, kept: &[Event]) -> u64 {
    let cut = node.store().cut_ts(PUBLIC_NET).unwrap().expect("a cut");
    let rows = node
        .store()
        .net_rows_after(PUBLIC_NET, (0, ""), 100_000)
        .unwrap();
    let local = node.local_authors();
    for row in rows.iter().filter(|r| r.ts < cut) {
        assert!(
            kept.iter().any(|e| e.id == row.id)
                || local.contains(&row.author)
                || wats::store::Keep::kind_kept(&row.kind),
            "row {} at {} below the cut {cut}",
            row.id,
            row.ts
        );
    }
    cut
}

async fn eventually<T>(what: &str, secs: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Soak: a node with a 2 MiB events share takes 2,000 new posts, a round
/// of 25 at a time, rotating whenever it reaches 95 %. It never goes over
/// its share; old bodies go first (stage 1), then old rows (stage 2), and
/// the cut rises; what is never dropped is all there.
#[tokio::test(flavor = "multi_thread")]
async fn the_store_stays_under_its_share_while_traffic_flows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let b_dir = dir.path().join("b");
    in_sync(&b_dir)?;
    let own = own_key(&b_dir)?;
    let now = now_secs();
    let kept = protected(&own, now - 60 * DAY);
    let old: Vec<Event> = (0..150)
        .map(|i| note(i, 100 + i % 50, now - 30 * DAY + u64::from(i) * 600, true))
        .collect();
    let mut b_config = config(&b_dir, &[]);
    b_config.storage_max = Some(8 * MIB);
    b_config.events_share = 25;
    let b = Node::start(b_config).await?;
    let share = b.events_share().unwrap();
    hold(b.store(), &kept);
    hold(b.store(), &old);

    let mut runs = Vec::new();
    let mut max_seen = 0;
    for round in 0..80u32 {
        let live: Vec<Event> = (0..25u32)
            .map(|i| {
                let n = 1_000 + round * 25 + i;
                // Posted over the last two days, newest last.
                note(
                    n,
                    1_000 + n % 300,
                    now - 2 * DAY + u64::from(n) * 50,
                    n % 3 == 0,
                )
            })
            .collect();
        hold(b.store(), &live);
        if let Some(run) = b.rotate_if_full().await? {
            runs.push(run);
        }
        let bytes = b.store().store_bytes();
        max_seen = max_seen.max(bytes);
        assert!(
            bytes <= share,
            "round {round}: {bytes} store bytes over the {share} share"
        );
    }
    assert!(runs.len() >= 3, "it rotated several times: {runs:?}");
    assert!(runs.iter().map(|r| r.bodies).sum::<u64>() > 0, "{runs:?}");
    assert!(runs.iter().map(|r| r.rows).sum::<u64>() > 0, "{runs:?}");
    assert!(max_seen > share * 9 / 10, "the share was reached");
    // Old bodies went first: stage 1 dropped them, reason evicted.
    assert!(old.iter().any(|e| {
        b.store()
            .drop_record(&e.id)
            .unwrap()
            .is_some_and(|r| r.reason == DropReason::Evicted)
            || b.store().get_raw(&e.id).unwrap().is_none()
    }));
    assert_protected(&b, &kept);
    let cut = assert_cut_honest(&b, &kept);
    assert!(cut > now - 2 * DAY, "the cut rose into the traffic");
    // The newest posts are all there.
    let newest = note(
        2_999,
        1_000 + 2_999 % 300,
        now - 2 * DAY + 2_999 * 50,
        false,
    );
    assert!(b.store().get(&newest.id)?.is_some());
    // It says so.
    let history = b.history_json(PUBLIC_NET);
    assert_eq!(history["cut_ts"].as_u64(), Some(cut), "{history}");
    assert!(history["rotated_ts"].is_u64(), "{history}");
    assert!(wats::node::history::line(&history).contains("(rotation, "));
    let rotation = b.rotation_json();
    assert_eq!(rotation["state"], "idle");
    assert!(rotation["last"]["at"].is_u64(), "{rotation}");
    assert!(wats::node::rotation::summary(&rotation).contains("old rows deleted"));
    assert!(
        !b.history_full(),
        "a node that rotated rows out is not full"
    );
    // Search still works on what is held.
    assert!(
        !b.store()
            .search(Some("remembers"), &Default::default(), 5)?
            .is_empty()
    );
    b.shutdown().await?;
    Ok(())
}

/// A key over its share of the events (a flood) is trimmed first, before
/// any other post loses its body or the cut moves.
#[tokio::test(flavor = "multi_thread")]
async fn a_flooding_key_is_trimmed_before_anyone_else() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let b_dir = dir.path().join("b");
    in_sync(&b_dir)?;
    let own = own_key(&b_dir)?;
    let now = now_secs();
    let kept = protected(&own, now - 60 * DAY);
    // Honest history from 100 keys, old enough for stage 1.
    let honest: Vec<Event> = (0..200)
        .map(|i| note(i, 100 + i % 100, now - 30 * DAY + u64::from(i) * 600, true))
        .collect();
    let mut b_config = config(&b_dir, &[]);
    b_config.storage_max = Some(8 * MIB);
    b_config.events_share = 25;
    let b = Node::start(b_config).await?;
    let (at, _) = b.rotation_bounds().unwrap();
    hold(b.store(), &kept);
    hold(b.store(), &honest);
    // One key floods recent junk until the store is full.
    let mut n = 10_000;
    while b.store().store_bytes() < at {
        hold(
            b.store(),
            &[note(n, 7, now - DAY + u64::from(n - 10_000), true)],
        );
        n += 1;
    }
    let run = b.rotate_if_full().await?.expect("a rotation");
    assert!(run.trimmed > 0, "{run:?}");
    assert_eq!(
        (run.bodies, run.rows),
        (0, 0),
        "nobody else touched: {run:?}"
    );
    assert!(
        honest
            .iter()
            .all(|e| b.store().get(&e.id).unwrap().is_some())
    );
    assert_eq!(b.store().cut_ts(PUBLIC_NET)?, None);
    assert_protected(&b, &kept);
    b.shutdown().await?;
    Ok(())
}

/// Once rotation raised the cut, the node serves it honestly: a descending
/// page (0.4.1 and later) stops at it with a `gap`, an ascending pull
/// from below it (0.4.0) is refused, and one from above is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_raised_cut_is_served_honestly_to_newer_and_older_peers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let b_dir = dir.path().join("b");
    in_sync(&b_dir)?;
    let now = now_secs();
    let mut b_config = config(&b_dir, &[]);
    b_config.storage_max = Some(8 * MIB);
    b_config.events_share = 25;
    let b = Node::start(b_config).await?;
    let (at, _) = b.rotation_bounds().unwrap();
    // Short posts, recent: stage 2 only.
    let mut n = 0;
    let mut all = Vec::new();
    while b.store().store_bytes() < at {
        let event = note(n, 100 + n % 200, now - 3 * DAY + u64::from(n) * 30, false);
        hold(b.store(), std::slice::from_ref(&event));
        all.push(event);
        n += 1;
    }
    let run = b.rotate_if_full().await?.expect("a rotation");
    assert!(run.rows > 0, "{run:?}");
    let cut = assert_cut_honest(&b, &[]);

    let a = Node::start(config(&dir.path().join("a"), &[&b])).await?;
    // A descending page stops at the cut, with a gap.
    let mut before = (now + 3600, String::new());
    let gap = loop {
        let request = Request::Sync {
            net: PUBLIC_NET.into(),
            after_ts: 0,
            after_id: String::new(),
            before_ts: Some(before.0),
            before_id: Some(before.1.clone()),
            kinds: vec![],
            authors: vec![],
            partial: false,
            limit: 256,
        };
        let Response::SyncPage { events, gap, .. } =
            wats::proto::call_solving(a.endpoint(), b.endpoint_id(), &request).await?
        else {
            panic!("page expected");
        };
        assert!(events.iter().all(|e| e["ts"].as_u64().unwrap() > cut));
        if let Some(last) = events.last() {
            before = (
                last["ts"].as_u64().unwrap(),
                last["id"].as_str().unwrap().to_string(),
            );
        }
        if gap.is_some() || events.is_empty() {
            break gap;
        }
    };
    assert_eq!(gap.map(|g| g.hi_ts), Some(cut));
    // An ascending pull from below the cut is refused; above, served.
    match wats::proto::call_solving(
        a.endpoint(),
        b.endpoint_id(),
        &Request::sync_after(PUBLIC_NET, 0, "", 50),
    )
    .await?
    {
        Response::Refused { reason } => assert!(reason.contains("not held here"), "{reason}"),
        other => panic!("refused expected: {other:?}"),
    }
    assert!(matches!(
        wats::proto::call_solving(
            a.endpoint(),
            b.endpoint_id(),
            &Request::sync_after(PUBLIC_NET, cut + 1, "", 50)
        )
        .await?,
        Response::SyncPage { .. }
    ));
    a.shutdown().await?;
    // A 0.4.0 node's catch-up from nothing is refused: its watermark stays.
    let mut legacy = config(&dir.path().join("legacy"), &[&b]);
    legacy.history.legacy = true;
    let legacy = Node::start(legacy).await?;
    legacy
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    assert!(legacy.sync_from(b.endpoint_id(), PUBLIC_NET).await.is_err());
    assert_eq!(legacy.store().sync_watermark(PUBLIC_NET)?, 0);
    let newest = all.last().unwrap();
    assert!(b.store().get(&newest.id)?.is_some());
    for node in [legacy, b] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A node stopped in the middle of a rotation (between two transactions)
/// leaves a consistent store: the cut covers every row deleted, nothing
/// protected is gone. Started again, it finishes the rotation by itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_in_the_middle_of_a_rotation_is_safe() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let b_dir = dir.path().join("b");
    in_sync(&b_dir)?;
    let own = own_key(&b_dir)?;
    let now = now_secs();
    let kept = protected(&own, now - 60 * DAY);
    let bytes = {
        let store = Store::open(&b_dir.join("events.sqlite"))?;
        hold(&store, &kept);
        let rows: Vec<Event> = (0..8_000)
            .map(|i| note(i, 100 + i % 300, now - 10 * DAY + u64::from(i) * 60, false))
            .collect();
        hold(&store, &rows);
        store.store_bytes()
    };
    // The share is what the store holds: rotation starts at once, slowly.
    let mut b_config = config(&b_dir, &[]);
    b_config.storage_max = Some(bytes * 2);
    b_config.events_share = 50;
    b_config.history.rotation_check = Duration::from_millis(200);
    b_config.history.rotation_pause = Duration::from_millis(400);
    let b = Node::start(b_config.clone()).await?;
    eventually("a batch deleted", 30, || {
        b.store().cut_ts(PUBLIC_NET).unwrap().map(|_| ())
    })
    .await;
    assert_eq!(b.rotation_json()["state"], "running");
    b.shutdown().await?;
    // The interrupted run stops at its next pause.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let (_, to) = b.rotation_bounds().unwrap();
    assert!(b.store().store_bytes() > to, "stopped halfway");
    assert_protected(&b, &kept);
    let first_cut = assert_cut_honest(&b, &kept);
    drop(b);

    b_config.history.rotation_pause = Duration::ZERO;
    let b = Node::start(b_config).await?;
    // Below 95 % it waits for more; with more, it rotates by itself.
    fill(&b, now);
    eventually("the rotation finished", 60, || {
        (b.store().store_bytes() <= to && b.rotation_json()["state"] == "idle").then_some(())
    })
    .await;
    assert_protected(&b, &kept);
    assert!(assert_cut_honest(&b, &kept) >= first_cut);
    assert!(b.store().unpin_pending()?.is_empty());
    let ranges = b.store().sync_ranges(PUBLIC_NET)?;
    assert_eq!(
        ranges.iter().filter(|r| r.state == RangeState::Cut).count(),
        1,
        "{ranges:?}"
    );
    b.shutdown().await?;
    Ok(())
}

/// Fill `node`'s store with short recent posts, newer than anything held,
/// up to rotation's threshold.
fn fill(node: &Node, from_ts: u64) {
    let (at, _) = node.rotation_bounds().unwrap();
    let mut n = 50_000;
    while node.store().store_bytes() < at {
        hold(
            node.store(),
            &[note(
                n,
                2_000 + n % 300,
                from_ts + u64::from(n - 50_000) / 10,
                false,
            )],
        );
        n += 1;
    }
}

/// Rotation never loses a file by side effect: a node responsible for a
/// blob (one of its k replicas) keeps it when rotation deletes the post
/// that lists it; a node that only cached it on a read lets it go.
#[tokio::test(flavor = "multi_thread")]
async fn a_responsible_replica_keeps_the_blob_a_cache_lets_it_go() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut a_config = config(&dir.path().join("a"), &[]);
    a_config.replication = 1;
    let a = Node::start(a_config).await?;
    let holder = |name: &str, k: usize, peers: &[&Arc<Node>]| -> anyhow::Result<NodeConfig> {
        let node_dir = dir.path().join(name);
        in_sync(&node_dir)?;
        let mut c = config(&node_dir, peers);
        c.replication = k;
        c.storage_max = Some(64 * MIB);
        c.events_share = 4;
        Ok(c)
    };
    let b = Node::start(holder("b", 3, &[&a])?).await?;
    let c = Node::start(holder("c", 1, &[&a, &b])?).await?;
    let holders = [b, c];
    let (b, c) = (&holders[0], &holders[1]);
    eventually("everyone knows everyone", 30, || {
        (a.known_peers().len() == 2 && !b.known_peers().is_empty() && c.known_peers().len() == 2)
            .then_some(())
    })
    .await;
    // A file whose closest node is b: a asks b to hold it (k = 1), and c,
    // which knows b, is not responsible for it (k = 1).
    let ids = [a.endpoint_id(), b.endpoint_id(), c.endpoint_id()];
    let content = (0u32..200)
        .map(|seed| {
            let mut bytes = vec![7u8; 4_096];
            bytes[..4].copy_from_slice(&seed.to_be_bytes());
            bytes
        })
        .find(|bytes| {
            let hash = *iroh_blobs::Hash::new(bytes).as_bytes();
            wats::proto::closest(&hash, &ids, 1) == [b.endpoint_id()]
        })
        .unwrap_or_else(|| panic!("a file closest to b: {ids:?}"));
    let file = dir.path().join("report.bin");
    std::fs::write(&file, &content)?;
    let event = a
        .publish_as(
            &key(77),
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("a report".into()),
                text: "attached".into(),
                ..Draft::default()
            },
            &[Attachment {
                path: file,
                name: None,
            }],
        )
        .await?;
    let blob = event.blobs[0].clone();
    for node in [b, c] {
        eventually("the post arrives", 30, || {
            node.store().get(&event.id).unwrap().map(|_| ())
        })
        .await;
    }
    let mut held = false;
    for _ in 0..300 {
        if b.blob_bytes(&blob.hash).await?.is_some() {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(held, "b holds the replica");
    assert!(
        c.fetch_and_export(&blob).await?.is_some(),
        "c caches it on a read"
    );
    assert!(b.blob_responsible(&blob.hash));
    assert!(!c.blob_responsible(&blob.hash));

    for node in [b, c] {
        assert!(node.blob_pinned(&blob.hash).await?);
        fill(node, event.ts + 1);
        let run = node.rotate_if_full().await?.expect("a rotation");
        assert!(run.rows > 0, "{run:?}");
        assert!(
            node.store().get_raw(&event.id)?.is_none(),
            "the oldest post went"
        );
        assert!(node.store().unpin_pending()?.is_empty());
    }
    // The replica keeps its tag, and says why.
    assert!(b.blob_pinned(&blob.hash).await?);
    assert_eq!(
        b.store().retained_blobs()?,
        [(blob.hash.clone(), "replica".to_string())]
    );
    // The cache does not: the blob is the collection's to delete.
    assert!(!c.blob_pinned(&blob.hash).await?);
    assert!(c.store().retained_blobs()?.is_empty());
    for node in holders.iter().chain([&a]) {
        node.shutdown().await?;
    }
    Ok(())
}

/// A node holding many tombstones, more than its opaque quota (5 % of
/// `storage.max`), still takes events of newer versions: tombstones are
/// not opaque content (ADR 0025 §7). Tombstones keep coming in too.
#[tokio::test(flavor = "multi_thread")]
async fn many_tombstones_leave_room_for_opaque_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let b_dir = dir.path().join("b");
    let mut b_config = config(&b_dir, &[]);
    b_config.storage_max = Some(8 * MIB);
    let b = Node::start(b_config).await?;
    let quota = b.opaque_quota().unwrap();
    let now = now_secs();
    let mut tombstones = 0u64;
    let mut n = 0;
    while tombstones <= quota {
        let event = note(n, 100 + n % 50, now - DAY + u64::from(n), true);
        hold(b.store(), std::slice::from_ref(&event));
        b.store()
            .drop_body(&event.id, DropReason::Evicted, None, None, now)?;
        let raw = b.store().get_raw(&event.id)?.unwrap().2;
        tombstones += serde_json::to_string(&raw)?.len() as u64;
        n += 1;
    }
    assert_eq!(b.store().opaque_bytes(), 0);
    // A tombstone pushed from elsewhere is taken.
    let elsewhere = note(n + 1, 3, now - 60, true);
    let tombstone = wats::wire::tombstone(&serde_json::to_value(&elsewhere)?)?;
    b.publish_signed(tombstone.to_value()).await?;
    // And an event of a newer version.
    let v3 = wats::wire::sign_value(
        json!({"v": 3, "ts": now, "net": PUBLIC_NET, "thread": null,
               "parent": null, "kind": "note"})
        .as_object()
        .unwrap()
        .clone(),
        json!({"text": "from a newer release"})
            .as_object()
            .unwrap()
            .clone(),
        &key(5),
        12,
    )?;
    let id = v3["id"].as_str().unwrap().to_string();
    b.publish_signed(v3).await?;
    assert!(b.store().get_raw(&id)?.is_some());
    assert!(b.store().opaque_bytes() > 0);
    b.shutdown().await?;
    Ok(())
}
