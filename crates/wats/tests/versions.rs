//! Protocol evolution (ADR 0007): a node stores and relays events of a
//! version it does not understand, so mixed-version networks stay whole.
//! v2 `submit` events (ADR 0015 §5), validated but kept opaque, are here
//! too: priced with SUBMIT_EXTRA_POW_BITS, kept for `submit.retention`.
//! (One test binary: each costs CI disk.)

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

use wats::mcp::WatsMcp;
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET, SUBMIT_EXTRA_POW_BITS, now_secs};
use wats::proto::NodeInfo;
use wats::store::Filter;
use wats::v2::{ENC_BYTES, b64url_encode, sign_v2_value};
use wats::wire::{RawEvent, sign_value};

async fn node(dir: &std::path::Path, peers: &[&Arc<Node>]) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

/// What a v3 node would publish: a new kind, a header extension and a body
/// field this node does not know (v2 is a validated version, ADR 0015).
fn v3_event(text: &str, ts: u64) -> Value {
    event_of_version(3, text, ts, wats::node::OPAQUE_EXTRA_POW_BITS)
}

/// The same shape at any version, mined to `bits`.
fn event_of_version(v: u64, text: &str, ts: u64, bits: u32) -> Value {
    sign_value(
        json!({"v": v, "ts": ts, "net": PUBLIC_NET, "thread": null, "parent": null,
               "kind": "poll", "ext": {"expires": ts + 3600}})
        .as_object()
        .unwrap()
        .clone(),
        json!({"text": text, "tags": ["topic:relays"], "options": ["eu", "us"]})
            .as_object()
            .unwrap()
            .clone(),
        &SigningKey::from_bytes(&[42; 32]),
        bits,
    )
    .unwrap()
}

/// Node policy for events of versions this node does not understand
/// (security review B4).
#[tokio::test(flavor = "multi_thread")]
async fn opaque_events_policy() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path());
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    // 2 MB: the opaque quota is 5% of it, 100 kB.
    config.storage_max = Some(2_000_000);
    let node = Node::start(config).await?;
    let now = wats::node::now_secs();
    let bits = wats::node::OPAQUE_EXTRA_POW_BITS;

    // Up to events_max + 2 is relayed; further ahead is not.
    node.publish_signed(event_of_version(4, "v4", now, bits))
        .await?;
    let err = node
        .publish_signed(event_of_version(5, "v5", now, bits))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too far ahead"), "{err:#}");

    // Two more bits of proof of work than a v1 event of the same size.
    let weak = loop {
        let candidate = event_of_version(3, &format!("weak {}", rand_text()), now, 0);
        let id = candidate["id"].as_str().unwrap();
        if id.starts_with(['8', '9', 'a', 'b', 'c', 'd', 'e', 'f']) {
            break candidate; // 0 leading zero bits
        }
    };
    let err = node.publish_signed(weak).await.unwrap_err();
    assert!(err.to_string().contains("proof of work"), "{err:#}");

    // The quota: about ten 10 kB opaque events fit in 100 kB.
    let big = "x".repeat(10_000);
    let mut refused = None;
    for i in 0..20 {
        // Proof of work also grows with size: mine a little more.
        let event = event_of_version(3, &format!("{i} {big}"), now, bits + 3);
        if let Err(e) = node.publish_signed(event).await {
            refused = Some((i, e));
            break;
        }
    }
    let (i, err) = refused.expect("the quota must stop opaque events");
    assert!((8..=10).contains(&i), "refused at {i}");
    assert!(err.to_string().contains("quota"), "{err:#}");
    assert!(node.store().opaque_bytes() <= 100_000);
    node.shutdown().await?;
    Ok(())
}

fn rand_text() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed).to_string()
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn node_relays_events_of_a_future_version() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // "a" plays the v3 publisher, "b" is the node in the middle, which
    // does not understand v3.
    let b = node(&dir.path().join("b"), &[]).await?;
    let a = node(&dir.path().join("a"), &[&b]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    // Live, over gossip.
    let live = v3_event("Where should the next relay go?", wats::node::now_secs());
    let live_id = live["id"].as_str().unwrap().to_string();
    a.publish_signed(live.clone()).await?;
    eventually("b stores the v3 event", async || {
        b.store().get_raw(&live_id).unwrap().is_some()
    })
    .await;
    let (v, opaque, stored) = b.store().get_raw(&live_id)?.unwrap();
    assert_eq!((v, opaque), (3, true));
    assert_eq!(stored, live, "stored byte-for-byte as received");
    // Not indexed: invisible to search and wait.
    assert!(
        b.store()
            .search(Some("relay"), &Filter::default(), 10)?
            .is_empty()
    );
    assert!(b.store().since(0, &Filter::default(), 10)?.is_empty());

    // `read` shows it raw.
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = WatsMcp::new(b.clone(), vec![]);
    tokio::spawn(async move {
        if let Ok(s) = server.serve(server_io).await {
            let _ = s.waiting().await;
        }
    });
    let client = ().serve(client_io).await?;
    let read = client
        .call_tool(
            CallToolRequestParams::new("read")
                .with_arguments(json!({"id": live_id}).as_object().unwrap().clone()),
        )
        .await?
        .structured_content
        .unwrap();
    assert_eq!(read["understood"], false);
    assert_eq!(read["kind"], "note");
    assert_eq!(read["raw"]["options"], json!(["eu", "us"]));
    client.cancel().await?;

    // Both sides learned each other's versions through hello.
    eventually("hello exchanged", async || {
        b.peer_info(&a.endpoint_id()).is_some()
    })
    .await;
    assert_eq!(b.peer_info(&a.endpoint_id()).unwrap(), NodeInfo::ours());

    // A second v3 event while "a" is the only one holding it, then "a" leaves.
    let offline = v3_event(
        "Second poll, published before c existed.",
        wats::node::now_secs(),
    );
    let offline_id = offline["id"].as_str().unwrap().to_string();
    a.publish_signed(offline).await?;
    eventually("b stores the second v3 event", async || {
        b.store().get_raw(&offline_id).unwrap().is_some()
    })
    .await;
    a.shutdown().await?;

    // "c" (another v3 node) only knows "b": it gets both events from the
    // node that does not understand them, intact and verifiable.
    let c = node(&dir.path().join("c"), &[&b]).await?;
    for id in [&live_id, &offline_id] {
        eventually("c receives the v3 event through b", async || {
            c.store().get_raw(id).unwrap().is_some()
        })
        .await;
        let (_, _, value) = c.store().get_raw(id)?.unwrap();
        let raw = RawEvent::from_value(value)?;
        assert_eq!(raw.v, 3);
        assert_eq!(&raw.id, id);
    }

    b.shutdown().await?;
    c.shutdown().await?;
    Ok(())
}

/// A proof of work of an algorithm this node does not know (ADR 0010,
/// algorithm-tagged PoW): the event is stored and relayed opaque, under the
/// opaque quota, never shown and never refused as "no work". A `b3` proof
/// below the required difficulty is still refused.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_pow_algorithm_is_opaque() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let start = |dir: &std::path::Path| {
        let mut config = NodeConfig::new(dir);
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(0);
        config.min_pow_bits = Some(16);
        Node::start(config)
    };
    let node = start(dir.path()).await?;
    let key = SigningKey::from_bytes(&[42; 32]);
    let note = wats::event::Draft {
        net: PUBLIC_NET.into(),
        kind: Some(wats::event::Kind::Note),
        title: Some("a puzzle from the future".into()),
        text: "mined with an algorithm this node does not know".into(),
        ..Default::default()
    }
    .sign(&key, wats::node::now_secs(), 0)?;
    let with_pow = |pow: &str| {
        let mut wire = serde_json::to_value(&note).unwrap();
        wire["pow"] = json!(pow);
        let d = wats::wire::digest(wire.as_object().unwrap()).unwrap();
        wire["id"] = json!(hex::encode(d.id));
        wire["sig"] = json!(hex::encode(wats::wire::sign_v1(&key, &d.id)));
        wire
    };

    // b3 with less than 16 bits: refused.
    let weak = (0..)
        .map(|n| with_pow(&format!("b3:{n}")))
        .find(|w| {
            w["id"]
                .as_str()
                .unwrap()
                .starts_with(['8', '9', 'a', 'b', 'c', 'd', 'e', 'f'])
        })
        .unwrap();
    let err = node.publish_signed(weak).await.unwrap_err();
    assert!(err.to_string().contains("proof of work"), "{err:#}");

    // An unknown algorithm: accepted, but opaque.
    let future = with_pow("eqx:AAECAwQFBgcICQoLDA0ODw");
    let id = future["id"].as_str().unwrap().to_string();
    node.publish_signed(future.clone()).await?;
    let (v, opaque, stored) = node.store().get_raw(&id)?.unwrap();
    assert_eq!((v, opaque), (1, true));
    assert_eq!(stored, future);
    assert!(
        node.store().opaque_bytes() > 0,
        "counted in the opaque quota"
    );
    assert!(
        node.store()
            .search(Some("puzzle"), &Filter::default(), 10)?
            .is_empty(),
        "never shown"
    );

    // Still opaque after a restart, which re-validates opaque v1 rows.
    node.shutdown().await?;
    let node = start(dir.path()).await?;
    let (_, opaque, _) = node.store().get_raw(&id)?.unwrap();
    assert!(opaque);
    node.shutdown().await?;
    Ok(())
}

// --- v2 submissions (ADR 0015 §5) ---

fn submission(seed: u8, ts: u64, bits: u32) -> Value {
    sign_v2_value(
        json!({"v": 2, "ts": ts, "net": PUBLIC_NET, "thread": null, "parent": null,
               "refs": [], "kind": "submit", "ext": {"to": format!("inbox:{}", "5e".repeat(16))}})
        .as_object()
        .unwrap()
        .clone(),
        json!({"enc": b64url_encode(&[seed; ENC_BYTES]), "ct": b64url_encode(b"sealed crash report")})
            .as_object()
            .unwrap()
            .clone(),
        &SigningKey::from_bytes(&[seed; 32]),
        None,
        bits,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn submissions_are_priced_stored_opaque_and_expire() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path());
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.submit_retention = Some(Duration::from_secs(3600));
    let node = Node::start(config).await?;
    let now = now_secs();

    // Accepted with the premium, stored opaque, never shown.
    let ok = submission(31, now, SUBMIT_EXTRA_POW_BITS);
    let id = ok["id"].as_str().unwrap().to_string();
    node.publish_signed(ok.clone()).await?;
    let (v, opaque, stored) = node.store().get_raw(&id)?.unwrap();
    assert_eq!((v, opaque), (2, true));
    assert_eq!(stored, ok);
    assert!(
        node.store()
            .search(Some("crash"), &Filter::default(), 10)?
            .is_empty()
    );
    assert!(node.store().since(0, &Filter::default(), 10)?.is_empty());

    // Without the premium: refused, although a v1 event would pass.
    let weak = (0u8..)
        .map(|seed| submission(seed, now, 0))
        .find(|w| {
            let first = u8::from_str_radix(&w["id"].as_str().unwrap()[..2], 16).unwrap();
            first >= 0x40 // fewer than 2 leading zero bits
        })
        .unwrap();
    let err = node.publish_signed(weak).await.unwrap_err();
    assert!(err.to_string().contains("proof of work"), "{err:#}");

    // Older than the retention: refused.
    let old = submission(32, now - 7200, SUBMIT_EXTRA_POW_BITS);
    let err = node.publish_signed(old.clone()).await.unwrap_err();
    assert!(err.to_string().contains("submit.retention"), "{err:#}");

    // An invalid v2 event is refused, not kept opaque.
    let mut forged = submission(33, now, SUBMIT_EXTRA_POW_BITS);
    forged["sig"] = json!("00".repeat(64));
    assert!(node.publish_signed(forged).await.is_err());

    // Stored before it expired (here: put straight into the store), it is
    // dropped by the retention task, and the opaque quota gets its bytes
    // back.
    let before = node.store().opaque_bytes();
    let old_id = old["id"].as_str().unwrap().to_string();
    node.store()
        .insert_raw(&RawEvent::from_value(old)?, now - 7200)?;
    assert!(node.store().opaque_bytes() > before);
    assert_eq!(node.drop_expired_submits()?, 1);
    assert!(node.store().get_raw(&old_id)?.is_none());
    assert!(node.store().get_raw(&id)?.is_some(), "a recent one stays");
    assert_eq!(node.store().opaque_bytes(), before);
    node.shutdown().await?;
    Ok(())
}

/// Under the network schedule, a submission pays the schedule and the
/// premium, never the new-author bits: its author is a one-off key.
#[tokio::test(flavor = "multi_thread")]
async fn submissions_pay_no_new_author_bits() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = NodeConfig::new(dir.path());
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = None;
    config.pow_schedule = vec![(0, 3)];
    let node = Node::start(config).await?;
    let now = now_secs();
    let required = 3 + SUBMIT_EXTRA_POW_BITS;
    // A fresh key for each, mined to [min, max) bits.
    let mined = |min: u32, max: u32| {
        (40u8..)
            .map(|seed| submission(seed, now, min))
            .find(|w| {
                let id: [u8; 32] = hex::decode(w["id"].as_str().unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap();
                wats::event::leading_zero_bits(&id) < max
            })
            .unwrap()
    };
    let err = node.publish_signed(mined(0, required)).await.unwrap_err();
    assert!(err.to_string().contains("proof of work"), "{err:#}");
    node.publish_signed(mined(
        required,
        required + wats::event::NEW_AUTHOR_EXTRA_BITS,
    ))
    .await?;
    node.shutdown().await?;
    Ok(())
}

/// Events of a version this node does not understand are relayed up to
/// 32 KiB canonical (room for a post-quantum suite), not 16 KiB; above
/// that they are refused (ADR 0010 A4 addendum).
#[tokio::test(flavor = "multi_thread")]
async fn opaque_relay_cap_is_32_kib() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let node = node(dir.path(), &[]).await?;
    let now = wats::node::now_secs();
    // 20 kB: above the 16 KiB of understood versions, below 32 KiB.
    let big = event_of_version(3, &"x".repeat(20_000), now, 8);
    node.publish_signed(big).await?;
    let too_big = event_of_version(3, &"x".repeat(33_000), now, 8);
    let err = node.publish_signed(too_big).await.unwrap_err();
    assert!(err.to_string().contains("relay limit"), "{err:#}");
    node.shutdown().await?;
    Ok(())
}
