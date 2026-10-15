//! Protocol evolution (ADR 0007): a node stores and relays events of a
//! version it does not understand, so mixed-version networks stay whole.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

use wats::mcp::WatsMcp;
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::proto::NodeInfo;
use wats::store::Filter;
use wats::wire::{RawEvent, sign_value};

async fn node(dir: &std::path::Path, peers: &[&Arc<Node>]) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = 0;
    config.min_pow_bits = 0;
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

/// What a v2 node would publish: a new kind, a header extension and a body
/// field v1 does not know.
fn v2_event(text: &str, ts: u64) -> Value {
    sign_value(
        json!({"v": 2, "ts": ts, "net": PUBLIC_NET, "thread": null, "parent": null,
               "kind": "poll", "ext": {"expires": ts + 3600}})
        .as_object()
        .unwrap()
        .clone(),
        json!({"text": text, "tags": ["topic:relays"], "options": ["eu", "us"]})
            .as_object()
            .unwrap()
            .clone(),
        &SigningKey::from_bytes(&[42; 32]),
        0,
    )
    .unwrap()
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_node_relays_v2_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // "a" plays the v2 publisher, "b" is the v1 node in the middle.
    let b = node(&dir.path().join("b"), &[]).await?;
    let a = node(&dir.path().join("a"), &[&b]).await?;
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;

    // Live, over gossip.
    let live = v2_event("Where should the next relay go?", wats::node::now_secs());
    let live_id = live["id"].as_str().unwrap().to_string();
    a.publish_signed(live.clone()).await?;
    eventually("b stores the v2 event", async || {
        b.store().get_raw(&live_id).unwrap().is_some()
    })
    .await;
    let (v, opaque, stored) = b.store().get_raw(&live_id)?.unwrap();
    assert_eq!((v, opaque), (2, true));
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

    // A second v2 event while "a" is the only one holding it, then "a" leaves.
    let offline = v2_event(
        "Second poll, published before c existed.",
        wats::node::now_secs(),
    );
    let offline_id = offline["id"].as_str().unwrap().to_string();
    a.publish_signed(offline).await?;
    eventually("b stores the second v2 event", async || {
        b.store().get_raw(&offline_id).unwrap().is_some()
    })
    .await;
    a.shutdown().await?;

    // "c" (another v2 node) only knows "b": it gets both events from the
    // v1 node, intact and verifiable.
    let c = node(&dir.path().join("c"), &[&b]).await?;
    for id in [&live_id, &offline_id] {
        eventually("c receives the v2 event through b", async || {
            c.store().get_raw(id).unwrap().is_some()
        })
        .await;
        let (_, _, value) = c.store().get_raw(id)?.unwrap();
        let raw = RawEvent::from_value(value)?;
        assert_eq!(raw.v, 2);
        assert_eq!(&raw.id, id);
    }

    b.shutdown().await?;
    c.shutdown().await?;
    Ok(())
}
