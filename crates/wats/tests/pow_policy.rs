//! Proof-of-work policy (ADR 0010 addendum, algorithm-tagged PoW): a base
//! difficulty keyed to the event's `ts`, plus extra bits for new authors on
//! events pushed to a node, not on history it pulls.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use serde_json::Value;

use wats::event::{Draft, Kind, NEW_AUTHOR_EXTRA_BITS};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET, now_secs};

async fn node(
    dir: &std::path::Path,
    min_pow_bits: Option<u32>,
    schedule: Vec<(u64, u32)>,
    peers: &[&Arc<Node>],
) -> Arc<Node> {
    let mut config = NodeConfig::new(dir);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = min_pow_bits;
    config.pow_schedule = schedule;
    Node::start(config).await.unwrap()
}

/// A post by `key` at `ts` with at least `min` and fewer than `max` bits.
fn post(key: &SigningKey, ts: u64, min: u32, max: u32) -> Value {
    (0u32..)
        .map(|i| {
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some(format!("post {i}")),
                text: format!("mined to {min} bits"),
                ..Draft::default()
            }
            .sign(key, ts, min)
            .unwrap()
        })
        .find(|e| e.pow_bits() < max)
        .map(|e| serde_json::to_value(&e).unwrap())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn schedule_by_ts_and_new_author_bits() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let raise = now - 1000;
    // 3 bits before `raise`, 7 from it on; +4 for a new author when pushed.
    let schedule = vec![(0, 3), (raise, 7)];
    let b = node(&dir.path().join("b"), Some(0), schedule.clone(), &[]).await;
    let a = node(&dir.path().join("a"), None, schedule, &[&b]).await;
    let x = SigningKey::from_bytes(&[9; 32]);
    let extra = NEW_AUTHOR_EXTRA_BITS;

    // Pushed, from an author `a` has never seen: schedule + 4.
    let old_weak = post(&x, raise - 10, 3, 3 + extra);
    let err = a.publish_signed(old_weak.clone()).await.unwrap_err();
    assert!(err.to_string().contains("proof of work"), "{err:#}");
    a.publish_signed(post(&x, raise - 20, 3 + extra, 64))
        .await?;

    // Pulled by sync: the schedule at each event's ts, no new-author bits.
    let y = SigningKey::from_bytes(&[10; 32]);
    let old_ok = post(&y, raise - 10, 3, 3 + extra); // 3 bits, before the raise
    let new_weak = post(&y, now - 10, 3, 7); // under 7 bits, after the raise
    let new_ok = post(&y, now - 5, 7, 64);
    for e in [&old_ok, &new_weak, &new_ok] {
        b.publish_signed(e.clone()).await?;
    }
    a.sync_from(b.endpoint_id(), PUBLIC_NET).await?;
    let has = |e: &Value| {
        a.store()
            .get_raw(e["id"].as_str().unwrap())
            .unwrap()
            .is_some()
    };
    assert!(has(&old_ok), "history keeps the price of its time");
    assert!(!has(&new_weak), "below the schedule at its ts");
    assert!(has(&new_ok));
    // Pushed live, the same kind of post by `y`, known for less than a
    // day, still needs the new-author bits.
    let err = a
        .publish_signed(post(&y, raise - 30, 3, 3 + extra))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("proof of work"), "{err:#}");

    tokio::time::timeout(Duration::from_secs(10), async {
        a.shutdown().await?;
        b.shutdown().await
    })
    .await??;
    Ok(())
}
