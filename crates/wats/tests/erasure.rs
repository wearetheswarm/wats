//! Erasure labels (ADR 0020 §5.2): a node that subscribes to a labeler
//! drops the bodies that labeler labels `erasure`; one that does not
//! subscribe keeps them. Opt-in, none by default.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use wats::event::{Draft, Event, Kind};
use wats::identity::author_id;
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::store::{DropReason, Inserted};

async fn node(dir: &std::path::Path, labelers: &[String]) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.erasure_labelers = labelers.to_vec();
    Node::start(config).await
}

fn post(text: &str) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some(text.into()),
        text: text.into(),
        ..Draft::default()
    }
}

fn label(on: &Event) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        thread: Some(on.thread_root().to_string()),
        parent: Some(on.id.clone()),
        kind: Some(Kind::React),
        text: "Erasure request handled by the operator.".into(),
        data: serde_json::json!({"label": "erasure"})
            .as_object()
            .unwrap()
            .clone(),
        ..Draft::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscribed_node_drops_what_its_labeler_labels_erasure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let labeler = SigningKey::from_bytes(&[7; 32]);
    let someone = SigningKey::from_bytes(&[8; 32]);
    let stranger = SigningKey::from_bytes(&[9; 32]);
    let subscribed = node(&dir.path().join("s"), &[author_id(&labeler)]).await?;
    let other = node(&dir.path().join("o"), &[]).await?;

    for n in [&subscribed, &other] {
        let target = n.publish_as(&someone, post("personal data"), &[]).await?;
        let labelled = n.publish_as(&labeler, label(&target), &[]).await?;
        // A stranger's erasure label does nothing.
        let kept = n.publish_as(&someone, post("kept"), &[]).await?;
        n.publish_as(&stranger, label(&kept), &[]).await?;
        assert!(n.store().get(&kept.id)?.is_some());
        if Arc::ptr_eq(n, &subscribed) {
            let record = n.store().drop_record(&target.id)?.expect("dropped");
            assert_eq!(record.reason, DropReason::Erasure);
            assert_eq!(record.trigger.as_deref(), Some(labelled.id.as_str()));
            assert!(n.store().get(&target.id)?.is_none(), "body gone");
        } else {
            assert!(n.store().drop_record(&target.id)?.is_none());
            assert!(n.store().get(&target.id)?.is_some(), "not subscribed: kept");
        }
    }

    // A label that arrives before its target: only the tombstone is kept.
    let later = post("arrives late")
        .sign(&someone, wats::node::now_secs(), 0)
        .unwrap();
    subscribed.publish_as(&labeler, label(&later), &[]).await?;
    assert_eq!(
        subscribed.store().insert(&later, wats::node::now_secs())?,
        Inserted::Dropped
    );
    assert!(subscribed.store().get(&later.id)?.is_none());

    for n in [subscribed, other] {
        n.shutdown().await?;
    }
    Ok(())
}

/// A labeler subscribed to after its labels arrived: they apply at start.
#[tokio::test(flavor = "multi_thread")]
async fn labels_held_before_the_subscription_apply_at_start() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("n");
    let labeler = SigningKey::from_bytes(&[7; 32]);
    let someone = SigningKey::from_bytes(&[8; 32]);
    let n = node(&data, &[]).await?;
    let target = n.publish_as(&someone, post("to erase"), &[]).await?;
    n.publish_as(&labeler, label(&target), &[]).await?;
    n.shutdown().await?;
    drop(n);

    let n = node(&data, &[author_id(&labeler)]).await?;
    assert_eq!(
        n.store().drop_record(&target.id)?.map(|r| r.reason),
        Some(DropReason::Erasure)
    );
    n.shutdown().await?;
    Ok(())
}
