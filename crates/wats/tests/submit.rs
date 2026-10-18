//! v2 `submit` events on the public network (ADR 0015 §5): validated,
//! stored opaque, priced with SUBMIT_EXTRA_POW_BITS, kept for
//! `submit.retention` only.

use std::time::Duration;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET, SUBMIT_EXTRA_POW_BITS, now_secs};
use wats::store::Filter;
use wats::v2::{ENC_BYTES, b64url_encode, sign_v2_value};
use wats::wire::RawEvent;

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
