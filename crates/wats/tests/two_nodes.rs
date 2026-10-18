//! Phase 0 spike: two nodes on loopback gossip a signed event and exchange
//! a blob.

use std::time::Duration;

use wats::event::{Draft, Kind};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(8);
    config.min_pow_bits = Some(8);
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
                title: Some("test post".into()),
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

/// Proof of work grows with size (ADR 0010): the publisher mines what
/// receivers require, and an under-mined large event is refused.
#[tokio::test(flavor = "multi_thread")]
async fn proof_of_work_scales_with_size() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut cfg = config(&dir.path().join("n"));
    cfg.pow_bits = Some(4);
    cfg.min_pow_bits = Some(4);
    let node = Node::start(cfg).await?;
    let text = "x".repeat(5_000);
    let event = node
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("test post".into()),
                text: text.clone(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let size = event.canonical_size()?;
    assert!(event.pow_bits() >= wats::event::required_pow_bits(4, size));
    assert!(wats::event::required_pow_bits(4, size) > 4);

    // Signed elsewhere with only the base difficulty: refused.
    let key = ed25519_dalek::SigningKey::from_bytes(&[3; 32]);
    let mut weak = wats::wire::sign_value(
        serde_json::json!({"v": 1, "ts": wats::node::now_secs(), "net": PUBLIC_NET, "thread": null,
                           "parent": null, "refs": [], "kind": "note"})
        .as_object()
        .unwrap()
        .clone(),
        serde_json::json!({"title": "big", "text": text, "tags": [], "data": {}, "blobs": []})
            .as_object()
            .unwrap()
            .clone(),
        &key,
        4,
    )?;
    let bits = wats::event::leading_zero_bits(&wats::event::parse_hex::<32>(
        weak["id"].as_str().unwrap(),
        "id",
    )?);
    if bits < 5 {
        let err = node
            .publish_signed(weak.take())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("insufficient proof of work"), "{err}");
    }
    node.shutdown().await?;
    Ok(())
}

// Network halt (ADR 0016), with a test guardian set (the compiled one is
// empty, which disables halts).

fn guardian() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7; 32])
}

fn halt_config(dir: &std::path::Path, peers: &[&std::sync::Arc<Node>]) -> NodeConfig {
    let mut config = config(dir);
    config.halt.guardians = wats::halt::Guardians {
        keys: vec![wats::identity::author_id(&guardian())],
        threshold: 1,
        window: wats::halt::HALT_APPROVAL_WINDOW,
    };
    for peer in peers {
        config.peers.push(peer.endpoint().addr());
    }
    config
}

fn signed(draft: Draft, ts: u64) -> serde_json::Value {
    let event = wats::halt::sign(draft, &guardian(), ts, Some(8)).unwrap();
    serde_json::to_value(&event).unwrap()
}

async fn outcome(node: &Node, what: &str) -> wats::node::HaltOutcome {
    let mut outcome = node.halt_outcome();
    tokio::time::timeout(
        Duration::from_secs(45),
        outcome.wait_for(|o| *o != wats::node::HaltOutcome::Running),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out: {what}"))
    .unwrap()
    .clone()
}

fn note(net: &str) -> Draft {
    Draft {
        net: net.into(),
        kind: Some(Kind::Note),
        title: Some("still here".into()),
        text: "a post".into(),
        ..Draft::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn guardian_halt_stops_a_node_until_resumed_or_unhalted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // The guardian's node ignores halts (halt.accept = false).
    let mut a_config = halt_config(&dir.path().join("a"), &[]);
    a_config.halt.accept = false;
    let a = Node::start(a_config).await?;
    let b_dir = dir.path().join("b");
    let b = Node::start(halt_config(&b_dir, &[&a])).await?;
    b.enforce_halts();
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    let now = wats::node::now_secs();
    let halt = signed(wats::halt::halt_draft("runaway loop", now - 10)?, now - 10);
    let halt_id = halt["id"].as_str().unwrap().to_string();
    a.publish_signed(halt).await?;

    // b relays the halt to its peers, writes the marker and stops.
    let wats::node::HaltOutcome::Exit(decision) = outcome(&b, "b stops").await else {
        panic!("b must exit");
    };
    assert_eq!(decision.id, halt_id);
    assert_eq!(decision.reason, "runaway loop");
    assert!(b.halt_pushed() >= 1, "pushed to a");
    let marker = wats::halt::read_marker(&b_dir).expect("marker");
    assert_eq!(marker.halt, halt_id);
    let err = b.publish(note(PUBLIC_NET), &[]).await.unwrap_err();
    assert!(err.to_string().contains("halted by guardians"), "{err:#}");
    assert!(b.halt_mcp_message().unwrap().contains("runaway loop"));
    assert_eq!(b.halt_status()["in_effect"]["id"], halt_id.as_str());
    assert_eq!(b.halt_status()["marker"]["halt"], halt_id.as_str());
    b.shutdown().await?;

    // a keeps running, and says it ignores the halt.
    let status = a.halt_status();
    assert_eq!(status["accept"], false, "{status}");
    assert_eq!(status["ignored"], true, "{status}");
    assert_eq!(status["in_effect"]["id"], halt_id.as_str());
    a.publish(note(PUBLIC_NET), &[]).await?;

    // Restarted with the marker, b finds no unhalt and stays halted.
    let b = Node::start(halt_config(&b_dir, &[&a])).await?;
    assert!(b.check_halt_marker(Duration::from_secs(2)).await);
    assert!(wats::halt::read_marker(&b_dir).is_some());
    b.shutdown().await?;

    // `wats resume` clears it and overrides this halt here: no re-halt.
    let cleared = wats::halt::resume(&b_dir, now)?.unwrap();
    assert_eq!(cleared.halt, halt_id);
    let b = Node::start(halt_config(&b_dir, &[&a])).await?;
    b.enforce_halts();
    assert!(!b.check_halt_marker(Duration::from_secs(1)).await);
    assert!(b.halt_view().overridden);
    assert_eq!(b.halt_status()["overridden"], true);
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.publish(note(PUBLIC_NET), &[]).await?;
    assert_eq!(*b.halt_outcome().borrow(), wats::node::HaltOutcome::Running);

    // A new halt still applies.
    let second = signed(wats::halt::halt_draft("again", now - 5)?, now - 5);
    let second_id = second["id"].as_str().unwrap().to_string();
    a.publish_signed(second).await?;
    let wats::node::HaltOutcome::Exit(decision) = outcome(&b, "b stops again").await else {
        panic!("b must exit");
    };
    assert_eq!(decision.id, second_id);
    b.shutdown().await?;

    // A later unhalt, synced at the next start, lifts it.
    a.publish_signed(signed(
        wats::halt::unhalt_draft(&second_id, "fixed", now)?,
        now,
    ))
    .await?;
    let b = Node::start(halt_config(&b_dir, &[&a])).await?;
    assert!(!b.check_halt_marker(Duration::from_secs(20)).await);
    assert!(wats::halt::read_marker(&b_dir).is_none());
    assert!(b.halt_in_force().is_none());
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_network_node_leaves_public_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(halt_config(&dir.path().join("a"), &[])).await?;
    let b_dir = dir.path().join("b");
    let mut b_config = halt_config(&b_dir, &[&a]);
    b_config.nets = vec![PUBLIC_NET.into(), "lab".into()];
    let b = Node::start(b_config).await?;
    b.enforce_halts();
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let now = wats::node::now_secs();
    let halt = signed(wats::halt::halt_draft("runaway", now)?, now);
    a.publish_signed(halt).await?;
    assert_eq!(
        outcome(&b, "b leaves public").await,
        wats::node::HaltOutcome::LeftPublic
    );
    assert_eq!(b.joined_nets(), ["lab"]);
    assert!(wats::halt::read_marker(&b_dir).is_some());
    assert_eq!(b.halt_status()["left"], serde_json::json!(["public"]));
    assert!(b.halt_mcp_message().is_none(), "MCP keeps working for lab");
    b.publish(note("lab"), &[]).await?;
    assert!(b.publish(note(PUBLIC_NET), &[]).await.is_err());
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_serving_node_keeps_only_halt_events_flowing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut a_config = halt_config(&dir.path().join("a"), &[]);
    a_config.halt.accept = false;
    let a = Node::start(a_config).await?;
    let mut b_config = halt_config(&dir.path().join("b"), &[&a]);
    b_config.halt.serve = true;
    let b = Node::start(b_config).await?;
    b.enforce_halts();
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    let before = a.publish(note(PUBLIC_NET), &[]).await?;
    eventually("b stores the note", || async {
        b.store().get(&before.id).ok().flatten()
    })
    .await;
    let now = wats::node::now_secs();
    let halt = signed(wats::halt::halt_draft("runaway", now)?, now);
    let halt_id = halt["id"].as_str().unwrap().to_string();
    a.publish_signed(halt).await?;
    assert_eq!(
        outcome(&b, "b serves halts").await,
        wats::node::HaltOutcome::Serving
    );
    // Still up, no marker; only halt events in, and out by sync.
    assert!(wats::halt::read_marker(&dir.path().join("b")).is_none());
    let after = serde_json::to_value(wats::halt::sign(
        note(PUBLIC_NET),
        &guardian(),
        now,
        Some(8),
    )?)?;
    let request = wats::proto::Request::Replicate {
        event: after,
        hashes: vec![],
    };
    let refused = wats::proto::call_solving(a.endpoint(), b.endpoint_id(), &request).await?;
    assert!(
        matches!(refused, wats::proto::Response::Refused { .. }),
        "{refused:?}"
    );
    let request = wats::proto::Request::Sync {
        net: PUBLIC_NET.into(),
        after_ts: 0,
        after_id: String::new(),
        limit: 100,
    };
    let wats::proto::Response::SyncPage { events, .. } =
        wats::proto::call_solving(a.endpoint(), b.endpoint_id(), &request).await?
    else {
        panic!("sync page expected");
    };
    let ids: Vec<&str> = events.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(ids, [halt_id.as_str()]);
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}
