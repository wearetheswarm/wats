//! Bounded history sync, newest first (ADR 0025): the initial fill and its
//! phases, the budget and the cut, catch-up ranges that resume, older
//! peers, honest serving, and fetches on read. Hermetic: in-process nodes
//! on loopback.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use wats::event::{Draft, Event, Kind};
use wats::mcp::WatsMcp;
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET, now_secs};
use wats::proto::{Request, Response};
use wats::settings::InitialMax;
use wats::store::{RangeState, Store};

const DAY: u64 = 86_400;

fn config(dir: &std::path::Path, peers: &[&Arc<Node>]) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    config.history.page = 16;
    config.history.older_peers_wait = Duration::from_secs(1);
    config.history.retry = Duration::from_secs(2);
    config
}

fn key(n: u32) -> SigningKey {
    let mut bytes = [7u8; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    SigningKey::from_bytes(&bytes)
}

fn sign(draft: Draft, key: &SigningKey, ts: u64) -> Event {
    wats::halt::sign(draft, key, ts, Some(0)).unwrap()
}

fn note(n: u32, ts: u64) -> Event {
    sign(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(Kind::Note),
            title: Some(format!("note {n}")),
            // Big enough for a budget to bite after a few dozen.
            text: format!("note {n}: {}", "the swarm remembers. ".repeat(70)),
            ..Draft::default()
        },
        &key(1000 + n),
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
            text: "a reply".into(),
            data: data.as_object().cloned().unwrap_or_default(),
            ..Draft::default()
        },
        by,
        ts,
    )
}

fn root(kind: Kind, title: &str, by: &SigningKey, ts: u64) -> Event {
    sign(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(kind),
            title: Some(title.into()),
            text: title.into(),
            ..Draft::default()
        },
        by,
        ts,
    )
}

/// Store events on a node as if it had them all along.
fn hold(node: &Node, events: &[Event]) {
    for event in events {
        node.store().insert(event, event.ts).unwrap();
    }
}

fn seq(node: &Node, id: &str) -> Option<i64> {
    node.store().get(id).unwrap().map(|s| s.seq)
}

async fn eventually<T>(what: &str, secs: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn state(node: &Node) -> String {
    node.history_json(PUBLIC_NET)["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn done(node: &Node) {
    eventually("history done", 60, || {
        matches!(state(node).as_str(), "done" | "full").then_some(())
    })
    .await;
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn client_for(node: &Arc<Node>) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = WatsMcp::new(Arc::clone(node), vec![]);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ().serve(client_io).await.unwrap()
}

async fn call(client: &Client, tool: &'static str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

fn structured(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.clone().unwrap()
}

/// `count` notes, one an hour back from `newest`, two per key.
fn notes(count: u32, newest: u64) -> Vec<Event> {
    (0..count)
        .map(|i| note(i / 2, newest - u64::from(i) * 3600))
        .collect()
}

/// A new node with a small `sync.initial_max` against a full node: MCP
/// answers while it fills; the phases come in order (kept-forever kinds,
/// its own threads, digests and their signals, then the rest newest
/// first); it stops at the budget and records the cut.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_node_fills_newest_first_up_to_its_budget() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    let now = now_secs();
    let base = now - 40 * DAY;
    // Kept forever: an announce and a vouch on it, very old.
    let announce = root(Kind::Announce, "I review Rust", &key(1), base - 30 * DAY);
    let vouch = reply(&announce, Kind::Vouch, json!({}), &key(2), base - 29 * DAY);
    // The new node's own profile, a returning identity: its ask and a
    // reply to it.
    let b_dir = dir.path().join("b");
    std::fs::create_dir_all(&b_dir)?;
    let b_key = wats::identity::RootSecret::load_or_create(&b_dir.join("secret.key"))?
        .profile_key(wats::identity::DEFAULT_PROFILE);
    let own = root(Kind::Ask, "my old question", &b_key, base - 20 * DAY);
    let answer = reply(&own, Kind::Answer, json!({}), &key(3), base - 19 * DAY);
    // A digest and the signals on it.
    let digest = sign(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(Kind::Ext(wats::knowledge::DIGEST_KIND.into())),
            title: Some("what we know".into()),
            text: "a digest".into(),
            refs: vec![announce.id.clone()],
            ..Draft::default()
        },
        &key(4),
        base - 10 * DAY,
    );
    let used = reply(
        &digest,
        Kind::React,
        json!({"used": true}),
        &key(5),
        base - 9 * DAY,
    );
    let window = notes(300, now - 3600);
    hold(
        &a,
        &[announce.clone(), vouch.clone(), own.clone(), answer.clone()],
    );
    hold(&a, &[digest.clone(), used.clone()]);
    hold(&a, &window);

    let mut b_config = config(&b_dir, &[&a]);
    let budget = 400 * 1024;
    b_config.initial_max = InitialMax::Bytes(budget);
    b_config.history.page = 8;
    b_config.history.page_pause = Duration::from_millis(150);
    let b = Node::start(b_config).await?;
    let start_bytes = b.store().store_bytes();

    // Usable at once: MCP answers while the history comes.
    eventually("the fill runs", 30, || {
        (state(&b) == "syncing").then_some(())
    })
    .await;
    let client = client_for(&b).await;
    let status = structured(&call(&client, "status", json!({})).await);
    let history = &status["history"][PUBLIC_NET];
    assert_eq!(history["state"], "syncing", "{status}");
    assert_eq!(history["budget"], budget, "{status}");
    assert!(wats::node::history::line(history).starts_with("syncing"));

    done(&b).await;
    // Kept-forever kinds, own threads and knowledge, though below the cut.
    let order: Vec<i64> = [&announce, &vouch, &own, &answer, &digest, &used]
        .iter()
        .map(|e| seq(&b, &e.id).unwrap_or_else(|| panic!("{} not held", e.kind)))
        .collect();
    assert!(order[0].max(order[1]) < order[2].min(order[3]), "{order:?}");
    assert!(order[2].max(order[3]) < order[4], "{order:?}");
    assert!(order[4] < order[5], "{order:?}");
    // Then the window, newest first, down to the budget.
    let held: Vec<(u64, i64)> = window
        .iter()
        .filter_map(|e| seq(&b, &e.id).map(|s| (e.ts, s)))
        .collect();
    assert!(
        !held.is_empty() && held.len() < window.len(),
        "{}",
        held.len()
    );
    assert!(held.iter().all(|(_, s)| *s > order[5]));
    assert!(held.windows(2).all(|w| w[0].1 < w[1].1), "newest first");
    assert!(seq(&b, &window[0].id).is_some(), "the newest note is held");
    assert!(seq(&b, &window[299].id).is_none(), "the oldest is not");
    let added = b.store().store_bytes().saturating_sub(start_bytes);
    assert!(
        added < budget + 128 * 1024,
        "{added} bytes for a budget of {budget}"
    );
    // The cut is recorded, and shown.
    let cut = b
        .store()
        .sync_ranges(PUBLIC_NET)?
        .into_iter()
        .find(|r| r.state == RangeState::Cut)
        .expect("a cut");
    let oldest_held = held.iter().map(|(ts, _)| *ts).min().unwrap();
    assert!(cut.cursor_ts <= oldest_held && cut.cursor_ts >= window[299].ts);
    let history = b.history_json(PUBLIC_NET);
    assert_eq!(history["state"], "done");
    assert_eq!(history["cut_ts"], cut.cursor_ts);
    assert!(b.store().sync_watermark(PUBLIC_NET)? > 0);
    // A node with a cut is no `history_full` peer.
    assert!(!b.history_full());

    client.cancel().await?;
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// `sync.initial_max = 0`: the whole history, no cut, and the node then
/// says it holds it all (`history_full`).
#[tokio::test(flavor = "multi_thread")]
async fn initial_max_zero_pulls_the_whole_history() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    let window = notes(120, now_secs() - 3600);
    hold(&a, &window);
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.initial_max = InitialMax::Bytes(0);
    b_config.storage_max = Some(1 << 30);
    let b = Node::start(b_config).await?;
    done(&b).await;
    assert!(window.iter().all(|e| seq(&b, &e.id).is_some()));
    assert!(b.store().sync_ranges(PUBLIC_NET)?.is_empty());
    assert_eq!(state(&b), "full");
    assert!(b.history_full());
    let info = b.our_info();
    assert!(info.features.iter().any(|f| f == "history_full"));
    assert!(info.features.iter().any(|f| f == "sync_desc"));
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// After an offline period, the catch-up comes newest first. Interrupted
/// half-way, its range stays pending with its cursor and the watermark
/// does not move; at the next start, the range above is walked first,
/// the old one resumes, and only then does the watermark move.
#[tokio::test(flavor = "multi_thread")]
async fn catch_up_is_newest_first_and_resumes_after_a_stop() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    let b_dir = dir.path().join("b");
    let b = Node::start(config(&b_dir, &[&a])).await?;
    done(&b).await;
    b.shutdown().await?;
    drop(b);
    let watermark = Store::open(&b_dir.join("events.sqlite"))?.sync_watermark(PUBLIC_NET)?;
    assert!(watermark > 0);

    // Offline: the swarm moves on.
    let now = now_secs();
    let missed: Vec<Event> = (0..80u32)
        .map(|i| note(i, now - 80 + u64::from(i)))
        .collect();
    hold(&a, &missed);

    let mut slow = config(&b_dir, &[&a]);
    slow.history.page = 5;
    slow.history.page_pause = Duration::from_millis(400);
    let b = Node::start(slow).await?;
    eventually("part of the catch-up", 60, || {
        (missed.iter().filter(|e| seq(&b, &e.id).is_some()).count() >= 15).then_some(())
    })
    .await;
    b.shutdown().await?;
    drop(b);
    // Newest first.
    let store = Store::open(&b_dir.join("events.sqlite"))?;
    let got: Vec<(u64, i64)> = missed
        .iter()
        .filter_map(|e| store.get(&e.id).unwrap().map(|s| (e.ts, s.seq)))
        .collect();
    assert!(got.len() < missed.len(), "interrupted half-way");
    assert!(got.windows(2).all(|w| w[0].1 > w[1].1), "{got:?}");
    assert!(
        store.get(&missed[79].id)?.is_some(),
        "the newest came first"
    );
    // Pending, with its cursor; the watermark has not moved.
    let pending: Vec<_> = store
        .sync_ranges(PUBLIC_NET)?
        .into_iter()
        .filter(|r| r.state == RangeState::Pending)
        .collect();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert!(pending[0].cursor_ts < pending[0].hi_ts);
    assert_eq!(store.sync_watermark(PUBLIC_NET)?, watermark);
    drop(store);

    // More while it is down; then it runs to the end.
    let later: Vec<Event> = (0..10u32)
        .map(|i| note(100 + i, now + 30 + u64::from(i)))
        .collect();
    hold(&a, &later);
    let b = Node::start(config(&b_dir, &[&a])).await?;
    eventually("the catch-up completes", 60, || {
        missed
            .iter()
            .chain(&later)
            .all(|e| seq(&b, &e.id).is_some())
            .then_some(())
    })
    .await;
    // The range above first, then the old one resumed.
    let resumed: Vec<i64> = missed
        .iter()
        .filter(|e| !got.iter().any(|(ts, _)| *ts == e.ts))
        .filter_map(|e| seq(&b, &e.id))
        .collect();
    let newer_max = later.iter().filter_map(|e| seq(&b, &e.id)).max().unwrap();
    assert!(resumed.iter().all(|s| *s > newer_max));
    eventually("the watermark moves", 30, || {
        (b.store().sync_watermark(PUBLIC_NET).unwrap() > watermark
            && !b
                .store()
                .sync_ranges(PUBLIC_NET)
                .unwrap()
                .iter()
                .any(|r| r.state == RangeState::Pending))
        .then_some(())
    })
    .await;
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// A catch-up larger than the budget: the rest of its range is skipped,
/// an intentional hole shown with its dates.
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_beyond_the_budget_is_skipped_and_shown() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    let b_dir = dir.path().join("b");
    let small = |peers: &[&Arc<Node>]| {
        let mut c = config(&b_dir, peers);
        c.initial_max = InitialMax::Bytes(120 * 1024);
        c
    };
    let b = Node::start(small(&[&a])).await?;
    done(&b).await;
    b.shutdown().await?;
    drop(b);
    let now = now_secs();
    let missed: Vec<Event> = (0..200u32)
        .map(|i| note(i, now - 200 + u64::from(i)))
        .collect();
    hold(&a, &missed);
    let b = Node::start(small(&[&a])).await?;
    let skipped = eventually("a skipped range", 60, || {
        b.store()
            .sync_ranges(PUBLIC_NET)
            .unwrap()
            .into_iter()
            .find(|r| r.state == RangeState::Skipped)
    })
    .await;
    assert!(seq(&b, &missed[199].id).is_some(), "the newest are held");
    assert!(seq(&b, &missed[0].id).is_none(), "the oldest are skipped");
    assert!(skipped.lo_ts <= missed[0].ts && skipped.hi_ts < missed[199].ts);
    done(&b).await;
    let history = b.history_json(PUBLIC_NET);
    assert_eq!(history["ranges"][0]["state"], "skipped", "{history}");
    let line = wats::node::history::line(&history);
    assert!(line.contains("(skipped: budget)"), "{line}");
    assert!(b.store().sync_watermark(PUBLIC_NET)? > 0);
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// Honest serving and fetches on demand: a node with a cut refuses an
/// ascending pull from below it (an older full node would take the rest
/// as complete), stops a descending page at its cut with a `gap`, and
/// `read` of a post whose parent is below its cut fetches the parent with
/// `get`.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_is_served_honestly_and_read_fetches_below_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    let now = now_secs();
    let old = root(Kind::Ask, "an old question", &key(1), now - 60 * DAY);
    let recent = reply(&old, Kind::Answer, json!({}), &key(2), now - 60);
    let window = notes(200, now - 3600);
    hold(&a, &[old.clone(), recent.clone()]);
    hold(&a, &window);
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.initial_max = InitialMax::Bytes(200 * 1024);
    let b = Node::start(b_config).await?;
    done(&b).await;
    assert!(seq(&b, &recent.id).is_some());
    assert!(b.store().get_raw(&old.id)?.is_none(), "below the cut");
    let cut = b.history_json(PUBLIC_NET)["cut_ts"].as_u64().unwrap();

    // An ascending pull from below the cut is refused...
    let pull = Request::sync_after(PUBLIC_NET, 0, "", 50);
    match wats::proto::call_solving(a.endpoint(), b.endpoint_id(), &pull).await? {
        Response::Refused { reason } => assert!(reason.contains("not held here"), "{reason}"),
        other => panic!("refused expected: {other:?}"),
    }
    // ...above it, served.
    let above = Request::sync_after(PUBLIC_NET, cut + 1, "", 50);
    assert!(matches!(
        wats::proto::call_solving(a.endpoint(), b.endpoint_id(), &above).await?,
        Response::SyncPage { .. }
    ));
    // A descending page stops at the cut, with a gap.
    let mut gap = None;
    let mut before = (now + 3600, String::new());
    for _ in 0..100 {
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
        let Response::SyncPage { events, gap: g, .. } =
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
        if g.is_some() || events.is_empty() {
            gap = g;
            break;
        }
    }
    assert_eq!(gap.map(|g| g.hi_ts), Some(cut));

    // `read` of the reply fetches its parent from the swarm.
    let client = client_for(&b).await;
    let read = structured(&call(&client, "read", json!({"id": recent.id})).await);
    assert!(read["fetched"].is_string(), "{read}");
    assert!(b.store().get(&old.id)?.is_some(), "fetched with get");
    // And a post nobody holds says so.
    let missing = call(&client, "read", json!({"id": "00".repeat(32)})).await;
    assert_eq!(missing.is_error, Some(true));
    assert!(format!("{missing:?}").contains("nor by the peers asked"));
    client.cancel().await?;
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// Older peers (0.4.0, no `sync_desc`): the fill asks ascending windows of
/// 7 days, then 14, 28…, the newest slice first, until the budget is spent;
/// the cut is recorded. A 0.4.0 node pulls from a full 0.4.1 node as
/// before, and is refused by one with a cut.
#[tokio::test(flavor = "multi_thread")]
async fn older_peers_get_stepped_windows_and_interoperate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut old_config = config(&dir.path().join("old"), &[]);
    old_config.history.legacy = true;
    let old = Node::start(old_config).await?;
    assert!(!old.our_info().features.iter().any(|f| f == "sync_desc"));
    // 120 notes, one every 12 hours: 60 days.
    let now = now_secs();
    let spread: Vec<Event> = (0..120u32)
        .map(|i| note(i, now - 3600 - u64::from(i) * 12 * 3600))
        .collect();
    hold(&old, &spread);

    let mut b_config = config(&dir.path().join("b"), &[&old]);
    b_config.initial_max = InitialMax::Bytes(200 * 1024);
    b_config.history.page = 8;
    let b = Node::start(b_config).await?;
    done(&b).await;
    let first_week: Vec<i64> = spread
        .iter()
        .filter(|e| e.ts >= now - 7 * DAY)
        .map(|e| seq(&b, &e.id).expect("the newest week, whatever the budget"))
        .collect();
    let older: Vec<i64> = spread
        .iter()
        .filter(|e| e.ts < now - 7 * DAY)
        .filter_map(|e| seq(&b, &e.id))
        .collect();
    assert!(!older.is_empty(), "older windows while the budget lasts");
    assert!(
        first_week.iter().max() < older.iter().min(),
        "the newest slice first"
    );
    assert!(seq(&b, &spread[119].id).is_none(), "the budget holds");
    let cut = b
        .store()
        .sync_ranges(PUBLIC_NET)?
        .into_iter()
        .find(|r| r.state == RangeState::Cut)
        .expect("a cut");
    assert!(cut.cursor_ts > spread[119].ts);

    // A 0.4.0 node pulls from a full 0.4.1 node, ascending, as before.
    let mut full_config = config(&dir.path().join("full"), &[&old]);
    full_config.initial_max = InitialMax::Bytes(0);
    let full = Node::start(full_config).await?;
    done(&full).await;
    assert!(
        spread.iter().all(|e| seq(&full, &e.id).is_some()),
        "the full node holds it all: {}",
        full.history_json(PUBLIC_NET)
    );
    let mut legacy = config(&dir.path().join("legacy"), &[&full]);
    legacy.history.legacy = true;
    let legacy = Node::start(legacy).await?;
    eventually("the 0.4.0 node pulls everything", 60, || {
        spread
            .iter()
            .all(|e| seq(&legacy, &e.id).is_some())
            .then_some(())
    })
    .await;
    // From the node with a cut, it is refused: its watermark stays. Alone
    // with it, so that no other peer serves it.
    for node in [legacy, full, old] {
        node.shutdown().await?;
    }
    let mut legacy2 = config(&dir.path().join("legacy2"), &[&b]);
    legacy2.history.legacy = true;
    let legacy2 = Node::start(legacy2).await?;
    legacy2
        .wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    assert!(
        legacy2
            .sync_from(b.endpoint_id(), PUBLIC_NET)
            .await
            .is_err()
    );
    assert_eq!(legacy2.store().sync_watermark(PUBLIC_NET)?, 0);
    for node in [legacy2, b] {
        node.shutdown().await?;
    }
    Ok(())
}

/// History older than a day wakes no runner and stays out of `wait`; a
/// recent post synced at the same time is news.
#[tokio::test(flavor = "multi_thread")]
async fn old_history_stays_out_of_wait() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    let now = now_secs();
    let ancient = root(Kind::Ask, "an ancient question", &key(1), now - 3 * DAY);
    let fresh = root(Kind::Ask, "a fresh question", &key(2), now - 60);
    hold(&a, &[ancient.clone(), fresh.clone()]);
    let b = Node::start(config(&dir.path().join("b"), &[&a])).await?;
    done(&b).await;
    assert!(seq(&b, &ancient.id).is_some() && seq(&b, &fresh.id).is_some());
    let client = client_for(&b).await;
    let waited = structured(&call(&client, "wait", json!({"since": 0, "timeout": 1})).await);
    let ids: Vec<&str> = waited["posts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(ids.contains(&fresh.id.as_str()), "{waited}");
    assert!(!ids.contains(&ancient.id.as_str()), "{waited}");
    client.cancel().await?;
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}
