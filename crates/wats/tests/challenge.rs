//! Load challenge on `/wats` (ADR 0014 addendum): overloaded or
//! rate-limited nodes challenge peers that can solve, and refuse the others
//! as before. Hermetic: in-process nodes on loopback.

use std::sync::Arc;
use std::time::Duration;

use wats::event::{Draft, Kind};
use wats::limits::Limits;
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::proto::{NodeInfo, Request, Response, call, call_solving, call_value};

async fn node(
    dir: &std::path::Path,
    peers: &[&Arc<Node>],
    limits: Limits,
) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.limits = limits;
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    Node::start(config).await
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn sync_request() -> Request {
    // What the server holds: it never synced, so it vouches for nothing.
    Request::sync_after(PUBLIC_NET, 0, "", 10).partial()
}

/// Whether `node` knows that `peer` can solve challenges.
fn knows_solver(node: &Node, peer: &Node) -> bool {
    node.peer_info(&peer.endpoint_id())
        .is_some_and(|i| i.features.iter().any(|f| f == "challenge"))
}

/// An overloaded node challenges a new peer, which solves and still
/// catches up.
#[tokio::test(flavor = "multi_thread")]
async fn overloaded_node_is_synced_from_by_solving() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = node(
        &dir.path().join("server"),
        &[],
        Limits {
            overload_in_flight: 0, // every request is over the threshold
            challenge_base_bits: 8,
            challenge_max_bits: 12,
            ..Limits::default()
        },
    )
    .await?;
    let event = server
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("before you came".into()),
                text: "Only sync brings this one.".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let late = node(&dir.path().join("late"), &[&server], Limits::default()).await?;
    eventually("the late node catches up through challenges", async || {
        late.store().get(&event.id).unwrap().is_some()
    })
    .await;
    let (issued, solved) = server.challenge_stats();
    assert!(
        issued >= 1 && solved >= 1,
        "{issued} issued, {solved} solved"
    );

    // Unsolved, a request gets a challenge; solved, it is served.
    eventually("the server knows the late node solves", async || {
        knows_solver(&server, &late)
    })
    .await;
    let answer = call(late.endpoint(), server.endpoint_id(), &sync_request()).await?;
    assert!(
        matches!(&answer, Response::Challenge { alg, effort, .. } if alg == "b3" && *effort >= 8),
        "{answer:?}"
    );
    let answer = call_solving(late.endpoint(), server.endpoint_id(), &sync_request()).await?;
    assert!(matches!(answer, Response::SyncPage { .. }), "{answer:?}");
    for n in [server, late] {
        n.shutdown().await?;
    }
    Ok(())
}

/// Over its rate limit, a peer that can solve gets a challenge instead of
/// a refusal, and a solution lets that request through.
#[tokio::test(flavor = "multi_thread")]
async fn rate_limited_solver_is_challenged() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = node(
        &dir.path().join("server"),
        &[],
        Limits {
            sync_per_min: 8, // a burst of 2
            ..Limits::default()
        },
    )
    .await?;
    let a = node(&dir.path().join("a"), &[&server], Limits::default()).await?;
    eventually("the server knows a solves", async || {
        knows_solver(&server, &a)
    })
    .await;
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(call(a.endpoint(), server.endpoint_id(), &sync_request()).await?);
    }
    let challenged = answers
        .iter()
        .filter(|r| matches!(r, Response::Challenge { .. }))
        .count();
    assert!(
        challenged >= 2
            && answers
                .iter()
                .all(|r| matches!(r, Response::Challenge { .. } | Response::SyncPage { .. })),
        "{answers:?}"
    );
    let answer = call_solving(a.endpoint(), server.endpoint_id(), &sync_request()).await?;
    assert!(matches!(answer, Response::SyncPage { .. }), "{answer:?}");
    for n in [server, a] {
        n.shutdown().await?;
    }
    Ok(())
}

/// A peer that does not advertise `challenge` (an older node) is never
/// sent one: over its rate limit it is refused, as before.
#[tokio::test(flavor = "multi_thread")]
async fn old_peer_is_refused_not_challenged() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = node(
        &dir.path().join("server"),
        &[],
        Limits {
            sync_per_min: 8,
            overload_in_flight: 0,
            ..Limits::default()
        },
    )
    .await?;
    // The client knows the server's address but shares no network with it:
    // no gossip neighbor, so no automatic hello or sync. An automatic hello
    // still in flight used to land after the old-style hello below and
    // advertise `challenge` again (a flake on CI); the client's automatic
    // sync also spent the server's rate-limit tokens.
    let old = {
        let mut config = NodeConfig::new(dir.path().join("old"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(0);
        config.min_pow_bits = Some(0);
        config.nets = vec!["elsewhere".into()];
        config.peers = vec![server.endpoint().addr()];
        Node::start(config).await?
    };
    // First as a current node: the server learns it can solve.
    let hello = Request::Hello {
        info: NodeInfo::ours(),
    };
    let answer = call_solving(old.endpoint(), server.endpoint_id(), &hello).await?;
    assert!(matches!(answer, Response::Hello { .. }), "{answer:?}");
    assert!(knows_solver(&server, &old));
    // Then as an older node: its latest hello is what counts.
    let mut info = NodeInfo::ours();
    info.features.retain(|f| f != "challenge");
    let hello = serde_json::json!({ "type": "hello", "info": info });
    let answer = call_value(old.endpoint(), server.endpoint_id(), hello).await?;
    assert!(matches!(answer, Response::Hello { .. }), "{answer:?}");
    assert!(!knows_solver(&server, &old));
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(call(old.endpoint(), server.endpoint_id(), &sync_request()).await?);
    }
    let refused = answers
        .iter()
        .filter(|r| matches!(r, Response::Refused { reason } if reason.contains("rate limited")))
        .count();
    assert!(
        refused >= 2
            && answers
                .iter()
                .all(|r| matches!(r, Response::Refused { .. } | Response::SyncPage { .. })),
        "{answers:?}"
    );
    assert!(!knows_solver(&server, &old));
    // No automatic hello happened either way.
    assert!(old.peer_info(&server.endpoint_id()).is_none());
    for n in [server, old] {
        n.shutdown().await?;
    }
    Ok(())
}
