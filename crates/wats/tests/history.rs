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
    // The test's logs: shown when it fails or times out, as two did on CI
    // (Ubuntu, 300 s) after 0.4.13 without any trace.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("wats=debug,iroh_gossip=info")
        .with_ansi(false)
        .with_test_writer()
        .try_init();
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    config.history.page = 16;
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

/// A node that holds the whole history of `public`, the reference the
/// others sync from: its watermark is ahead, so it never catches up and
/// has no hole, as a node in sync all along (watermark 0 and no range
/// would be a node with no history yet, which serves nothing as held).
async fn reference(dir: &std::path::Path) -> anyhow::Result<Arc<Node>> {
    std::fs::create_dir_all(dir)?;
    Store::open(&dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now_secs() + DAY)?;
    Node::start(config(dir, &[])).await
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
    let a = reference(&dir.path().join("a")).await?;
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
    let a = reference(&dir.path().join("a")).await?;
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
    let a = reference(&dir.path().join("a")).await?;
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
    for _ in 0..600 {
        if missed
            .iter()
            .chain(&later)
            .all(|e| seq(&b, &e.id).is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        missed
            .iter()
            .chain(&later)
            .all(|e| seq(&b, &e.id).is_some()),
        "the catch-up completes: {} {:?} missed {} later {}",
        b.history_json(PUBLIC_NET),
        b.store().sync_ranges(PUBLIC_NET)?,
        missed.iter().filter(|e| seq(&b, &e.id).is_some()).count(),
        later.iter().filter(|e| seq(&b, &e.id).is_some()).count()
    );
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
    let a = reference(&dir.path().join("a")).await?;
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
    let a = reference(&dir.path().join("a")).await?;
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

    // A full node whose only peer has a cut is not misled: what it got is
    // unconfirmed (its budget cut nothing, so it records no cut), and it
    // does not say it holds the whole history.
    a.shutdown().await?;
    let mut c_config = config(&dir.path().join("c"), &[&b]);
    c_config.initial_max = InitialMax::Bytes(0);
    let c = Node::start(c_config).await?;
    done(&c).await;
    assert!(seq(&c, &recent.id).is_some());
    let ranges = c.store().sync_ranges(PUBLIC_NET)?;
    assert!(
        ranges.iter().all(|r| r.state != RangeState::Cut),
        "{ranges:?}"
    );
    assert!(
        ranges.iter().any(|r| r.state == RangeState::Unconfirmed),
        "{ranges:?}"
    );
    assert!(!c.history_full());
    assert_eq!(state(&c), "done");
    c.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// One key takes at most 2 % of the window (ADR 0025 §4): a peer serving
/// a newest slice of junk from one key does not crowd out the rest.
#[tokio::test(flavor = "multi_thread")]
async fn one_key_takes_a_small_share_of_the_window() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = reference(&dir.path().join("a")).await?;
    let now = now_secs();
    let junk_key = key(99);
    let junk: Vec<Event> = (0..120u64)
        .map(|i| {
            sign(
                Draft {
                    net: PUBLIC_NET.into(),
                    kind: Some(Kind::Note),
                    title: Some(format!("junk {i}")),
                    text: "junk ".repeat(300),
                    ..Draft::default()
                },
                &junk_key,
                now - 60 - i,
            )
        })
        .collect();
    let real = notes(80, now - 3600);
    hold(&a, &junk);
    hold(&a, &real);
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    let budget = 400 * 1024;
    b_config.initial_max = InitialMax::Bytes(budget);
    let b = Node::start(b_config).await?;
    done(&b).await;
    let junk_held = junk.iter().filter(|e| seq(&b, &e.id).is_some()).count();
    let real_held = real.iter().filter(|e| seq(&b, &e.id).is_some()).count();
    // 2 % of 400 KiB: a handful of 1.6 KiB events.
    assert!(junk_held <= 6, "{junk_held} junk events held");
    assert!(real_held >= 40, "{real_held} real events held");
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
    let a = reference(&dir.path().join("a")).await?;
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

/// REVIEW (B1): a new 0.4.1 node whose only `sync_desc` peer is another new
/// 0.4.1 node (empty, filling too) and whose other neighbor is a 0.4.0
/// node holding the history. The empty peer's `partial` answer closes
/// nothing, and the 0.4.0 neighbor is always pulled.
#[tokio::test(flavor = "multi_thread")]
async fn review_partial_peer_hides_older_neighbors_history() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut old_config = config(&dir.path().join("old"), &[]);
    old_config.history.legacy = true;
    let old = Node::start(old_config).await?;
    let now = now_secs();
    let spread: Vec<Event> = (0..40u32)
        .map(|i| note(i, now - 3600 - u64::from(i) * 3600))
        .collect();
    hold(&old, &spread);
    let b = Node::start(config(&dir.path().join("b"), &[])).await?;
    let n = Node::start(config(&dir.path().join("n"), &[&b, &old])).await?;
    done(&n).await;
    let held = spread.iter().filter(|e| seq(&n, &e.id).is_some()).count();
    assert_eq!(
        held,
        spread.len(),
        "the 0.4.0 neighbor's history: {} {:?}",
        n.history_json(PUBLIC_NET),
        n.store().sync_ranges(PUBLIC_NET)?
    );
    for node in [n, b, old] {
        node.shutdown().await?;
    }
    Ok(())
}

/// REVIEW (B1): the same with a budget (storage.max set), and the control
/// without the second 0.4.1 node.
#[tokio::test(flavor = "multi_thread")]
async fn review_partial_peer_with_budget_and_control() -> anyhow::Result<()> {
    for with_b in [false, true] {
        let dir = tempfile::tempdir()?;
        let mut old_config = config(&dir.path().join("old"), &[]);
        old_config.history.legacy = true;
        let old = Node::start(old_config).await?;
        let now = now_secs();
        let spread: Vec<Event> = (0..40u32)
            .map(|i| note(i, now - 3600 - u64::from(i) * 3600))
            .collect();
        hold(&old, &spread);
        let b = Node::start(config(&dir.path().join("b"), &[])).await?;
        let peers: Vec<&Arc<Node>> = if with_b { vec![&b, &old] } else { vec![&old] };
        let mut n_config = config(&dir.path().join("n"), &peers);
        n_config.storage_max = Some(1 << 30);
        let n = Node::start(n_config).await?;
        done(&n).await;
        let held = spread.iter().filter(|e| seq(&n, &e.id).is_some()).count();
        assert_eq!(held, spread.len(), "with_b={with_b}, state {}", state(&n));
        for node in [n, b, old] {
            node.shutdown().await?;
        }
    }
    Ok(())
}

/// REVIEW (B1): the upgrade path. A 0.4.1 node back after a day offline,
/// with a 0.4.0 neighbor that holds the day and a fresh 0.4.1 neighbor
/// that holds nothing: the empty peer completes nothing, the 0.4.0
/// neighbor is pulled from the old watermark, and only then does the
/// watermark move.
#[tokio::test(flavor = "multi_thread")]
async fn review_catch_up_lost_with_an_empty_desc_peer() -> anyhow::Result<()> {
    for with_b in [false, true] {
        let dir = tempfile::tempdir()?;
        let mut old_config = config(&dir.path().join("old"), &[]);
        old_config.history.legacy = true;
        let old = Node::start(old_config).await?;
        let now = now_secs();
        let a_dir = dir.path().join("a");
        {
            std::fs::create_dir_all(&a_dir)?;
            let store = Store::open(&a_dir.join("events.sqlite"))?;
            store.set_sync_watermark(PUBLIC_NET, now - 2 * DAY)?;
        }
        let missed: Vec<Event> = (0..30u32)
            .map(|i| note(i, now - 3600 - u64::from(i) * 1800))
            .collect();
        hold(&old, &missed);
        let b = Node::start(config(&dir.path().join("b"), &[])).await?;
        let peers: Vec<&Arc<Node>> = if with_b { vec![&b, &old] } else { vec![&old] };
        let mut a_config = config(&a_dir, &peers);
        a_config.storage_max = Some(1 << 30);
        let a = Node::start(a_config).await?;
        eventually("watermark moves", 60, || {
            (a.store().sync_watermark(PUBLIC_NET).unwrap() > now - DAY).then_some(())
        })
        .await;
        let held = missed.iter().filter(|e| seq(&a, &e.id).is_some()).count();
        assert_eq!(held, missed.len(), "with_b={with_b}, state {}", state(&a));
        for node in [a, b, old] {
            node.shutdown().await?;
        }
    }
    Ok(())
}

fn acme_notes(now: u64) -> Vec<Event> {
    (0..10u32)
        .map(|i| {
            sign(
                Draft {
                    net: "acme".into(),
                    kind: Some(Kind::Note),
                    title: Some(format!("acme {i}")),
                    text: "acme".into(),
                    ..Draft::default()
                },
                &key(500 + i),
                now - 7200 - u64::from(i) * 60,
            )
        })
        .collect()
}

/// REVIEW (S2): two 0.4.1 nodes on a second network, both new, and a
/// public-only 0.4.1 node both know. Only the members of `acme` are asked,
/// one failure vetoes nothing, and two fills that nobody can confirm do
/// not stay pending: each ends unconfirmed, with no cut.
#[tokio::test(flavor = "multi_thread")]
async fn review_a_non_member_known_peer_blocks_the_partial_round() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let z = Node::start(config(&dir.path().join("z"), &[])).await?;
    let nets = vec![PUBLIC_NET.to_string(), "acme".to_string()];
    let mut y_config = config(&dir.path().join("y"), &[&z]);
    y_config.nets = nets.clone();
    let y = Node::start(y_config).await?;
    let acme = acme_notes(now_secs());
    hold(&y, &acme);
    let mut x_config = config(&dir.path().join("x"), &[&z, &y]);
    x_config.nets = nets;
    let x = Node::start(x_config).await?;
    eventually("x holds the acme history", 30, || {
        acme.iter().all(|e| seq(&x, &e.id).is_some()).then_some(())
    })
    .await;
    for node in [&x, &y] {
        eventually("the acme fill closes", 60, || {
            let h = node.history_json("acme");
            matches!(h["state"].as_str(), Some("done")).then_some(())
        })
        .await;
        let ranges = node.store().sync_ranges("acme")?;
        assert!(
            ranges.iter().any(|r| r.state == RangeState::Unconfirmed),
            "nobody confirmed the acme history: {ranges:?}"
        );
        assert!(
            ranges.iter().all(|r| r.state != RangeState::Cut),
            "{ranges:?}"
        );
        assert!(!node.history_full());
    }
    for node in [x, y, z] {
        node.shutdown().await?;
    }
    Ok(())
}

/// REVIEW (S2): the control, without the non-member.
#[tokio::test(flavor = "multi_thread")]
async fn review_control_without_non_member() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let z = Node::start(config(&dir.path().join("z"), &[])).await?;
    let nets = vec![PUBLIC_NET.to_string(), "acme".to_string()];
    let mut y_config = config(&dir.path().join("y"), &[]);
    y_config.nets = nets.clone();
    let y = Node::start(y_config).await?;
    let acme = acme_notes(now_secs());
    hold(&y, &acme);
    let mut x_config = config(&dir.path().join("x"), &[&y]);
    x_config.nets = nets;
    let x = Node::start(x_config).await?;
    eventually("x holds the acme history", 30, || {
        acme.iter().all(|e| seq(&x, &e.id).is_some()).then_some(())
    })
    .await;
    eventually("the acme fill closes", 60, || {
        (x.history_json("acme")["state"] == "done").then_some(())
    })
    .await;
    for node in [x, y, z] {
        node.shutdown().await?;
    }
    Ok(())
}

/// REVIEW (S3): the 2 % per-key share applies to kept-forever kinds too:
/// one key's junk `announce`s do not take the window.
#[tokio::test(flavor = "multi_thread")]
async fn review_keep_kinds_bypass_the_key_share() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = reference(&dir.path().join("a")).await?;
    let now = now_secs();
    let junk_key = key(99);
    let junk: Vec<Event> = (0..120u64)
        .map(|i| {
            sign(
                Draft {
                    net: PUBLIC_NET.into(),
                    kind: Some(Kind::Announce),
                    title: Some(format!("junk {i}")),
                    text: "junk ".repeat(300),
                    ..Draft::default()
                },
                &junk_key,
                now - 60 - i,
            )
        })
        .collect();
    let real = notes(80, now - 3600);
    hold(&a, &junk);
    hold(&a, &real);
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.initial_max = InitialMax::Bytes(400 * 1024);
    let b = Node::start(b_config).await?;
    done(&b).await;
    let junk_held = junk.iter().filter(|e| seq(&b, &e.id).is_some()).count();
    let real_held = real.iter().filter(|e| seq(&b, &e.id).is_some()).count();
    // 2 % of 400 KiB: a handful of 1.6 KiB announces, phase 0 included.
    assert!(junk_held <= 8, "{junk_held} junk announces held");
    assert!(real_held >= 40, "{real_held} real notes held");
    b.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// S6, N1: a catch-up whose only peer stops at a hole everywhere ends
/// unconfirmed, with its dates, never as confirmed: a full node then does
/// not say `history_full`. It keeps serving a 0.4.0 pull, as a 0.4.0 node
/// serves what it holds: only the budget makes it refuse one.
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_nobody_confirms_is_unconfirmed_and_still_served() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let a_dir = dir.path().join("a");
    {
        std::fs::create_dir_all(&a_dir)?;
        let store = Store::open(&a_dir.join("events.sqlite"))?;
        store.set_sync_watermark(PUBLIC_NET, now - 2 * DAY)?;
    }
    // A fresh peer: its own fill is pending, a hole at the top.
    let b = Node::start(config(&dir.path().join("b"), &[])).await?;
    let mut a_config = config(&a_dir, &[&b]);
    a_config.initial_max = InitialMax::Bytes(0);
    let a = Node::start(a_config).await?;
    let unconfirmed = eventually("an unconfirmed range", 60, || {
        a.store()
            .sync_ranges(PUBLIC_NET)
            .unwrap()
            .into_iter()
            .find(|r| r.state == RangeState::Unconfirmed)
    })
    .await;
    assert!(unconfirmed.lo_ts <= now - 2 * DAY);
    let ranges = a.store().sync_ranges(PUBLIC_NET)?;
    assert!(
        ranges
            .iter()
            .all(|r| !matches!(r.state, RangeState::Skipped | RangeState::Cut)),
        "{ranges:?}"
    );
    assert!(!a.history_full());
    let line = wats::node::history::line(&a.history_json(PUBLIC_NET));
    assert!(line.contains("unconfirmed since"), "{line}");
    let pull = Request::sync_after(PUBLIC_NET, now - 2 * DAY, "", 50);
    assert!(matches!(
        wats::proto::call_solving(b.endpoint(), a.endpoint_id(), &pull).await?,
        Response::SyncPage { .. }
    ));
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// B1, N1: a node with no history yet (watermark 0, no range) serves
/// nothing to a 0.4.1 peer as complete: a descending page gets a gap, and
/// only a `partial` request gets what it holds. A 0.4.0 pull gets what it
/// holds, as from a 0.4.0 node.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_no_history_yet_serves_a_hole() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = Node::start(config(&dir.path().join("a"), &[])).await?;
    assert_eq!(a.history_holes(PUBLIC_NET).len(), 1, "before any pass");
    // Not a member of `public`: no neighbor there, so `a` runs no pass.
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.nets = vec!["other".into()];
    let b = Node::start(b_config).await?;
    let held = notes(4, now_secs() - 60);
    hold(&a, &held);
    let call = |request: Request| {
        let (b, a) = (Arc::clone(&b), a.endpoint_id());
        async move { wats::proto::call_solving(b.endpoint(), a, &request).await }
    };
    match call(Request::sync_after(PUBLIC_NET, 0, "", 50)).await? {
        Response::SyncPage { events, .. } => assert_eq!(events.len(), held.len()),
        other => panic!("{other:?}"),
    }
    let desc = |partial| Request::Sync {
        net: PUBLIC_NET.into(),
        after_ts: 0,
        after_id: String::new(),
        before_ts: Some(now_secs() + 60),
        before_id: Some(String::new()),
        kinds: vec![],
        authors: vec![],
        partial,
        limit: 50,
    };
    match call(desc(false)).await? {
        Response::SyncPage { events, gap, .. } => {
            assert!(events.is_empty() && gap.is_some());
        }
        other => panic!("{other:?}"),
    }
    match call(desc(true)).await? {
        Response::SyncPage { events, gap, .. } => {
            assert_eq!(events.len(), held.len());
            assert!(gap.is_none());
        }
        other => panic!("{other:?}"),
    }
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// S5: a `get` answers each id once; smaller fixes: `sync.initial_max`
/// larger than the events share is clamped to it.
#[tokio::test(flavor = "multi_thread")]
async fn get_answers_each_id_once_and_the_budget_is_clamped() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = reference(&dir.path().join("a")).await?;
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.history.legacy = true;
    b_config.storage_max = Some(4 << 20);
    b_config.initial_max = InitialMax::Bytes(1 << 30);
    let b = Node::start(b_config).await?;
    assert_eq!(b.history_budget(), b.events_share());
    assert_eq!(b.history_budget(), Some(2 << 20));
    let posts = notes(2, now_secs() - 60);
    hold(&a, &posts);
    let ids = vec![
        posts[0].id.clone(),
        posts[0].id.clone(),
        posts[1].id.clone(),
    ];
    let get = Request::Get {
        net: PUBLIC_NET.into(),
        ids,
        include: "thread".into(),
    };
    match wats::proto::call_solving(b.endpoint(), a.endpoint_id(), &get).await? {
        Response::SyncPage { events, .. } => assert_eq!(events.len(), 2),
        other => panic!("{other:?}"),
    }
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// REVIEW2: two untrusted peers that hold nothing (stand-ins for two
/// colluding endpoint ids: a watermark ahead and no history) confirm a
/// fill as complete; the honest holder learned the same way is then never
/// asked (no check after a confirmed fill).
#[tokio::test(flavor = "multi_thread")]
async fn review2_two_empty_untrusted_peers_confirm_a_fill() -> anyhow::Result<()> {
    let mut missed_runs = 0;
    for _ in 0..4 {
        let dir = tempfile::tempdir()?;
        let e1 = reference(&dir.path().join("e1")).await?;
        let e2 = reference(&dir.path().join("e2")).await?;
        let h = reference(&dir.path().join("h")).await?;
        let spread = notes(40, now_secs() - 3600);
        hold(&h, &spread);
        let n = Node::start(config(&dir.path().join("n"), &[])).await?;
        for p in [&e1, &e2, &h] {
            n.add_peer(p.endpoint().addr()).await?;
        }
        done(&n).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        let held = spread.iter().filter(|e| seq(&n, &e.id).is_some()).count();
        eprintln!(
            "n holds {held}/40, state {}, ranges {:?}",
            state(&n),
            n.store().sync_ranges(PUBLIC_NET)?
        );
        if held < spread.len() {
            missed_runs += 1;
        }
        // Peers that served nothing confirm nothing.
        assert_ne!(state(&n), "full");
        assert!(!n.history_full());
        for node in [n, e1, e2, h] {
            node.shutdown().await?;
        }
    }
    assert_eq!(missed_runs, 0, "the honest holder is always read");
    Ok(())
}

/// REVIEW2 (N2): a slow but honest trusted peer (rate-limited, no
/// challenge): failed passes never close the fill, and no cut is recorded
/// above what was reached.
#[tokio::test(flavor = "multi_thread")]
async fn review2_failures_close_the_fill_at_its_top() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("a"))?;
    Store::open(&dir.path().join("a").join("events.sqlite"))?
        .set_sync_watermark(PUBLIC_NET, now_secs() + DAY)?;
    let mut a_config = config(&dir.path().join("a"), &[]);
    a_config.limits.challenge = false;
    a_config.limits.sync_per_min = 1;
    let a = Node::start(a_config).await?;
    let spread = notes(40, now_secs() - 3600);
    hold(&a, &spread);
    let n = Node::start(config(&dir.path().join("n"), &[&a])).await?;
    tokio::time::sleep(Duration::from_secs(25)).await;
    let ranges = n.store().sync_ranges(PUBLIC_NET)?;
    assert!(
        ranges.iter().all(|r| r.state != RangeState::Cut),
        "no cut without a budget: {ranges:?}"
    );
    assert!(
        ranges.iter().any(|r| r.state == RangeState::Pending) || state(&n) == "full",
        "still pending, or done: {ranges:?}"
    );
    n.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// REVIEW2 (N1): an upgraded node back after a day, whose only neighbor is
/// an untrusted 0.4.0 node holding the day: the catch-up gets it all, ends
/// unconfirmed (no skipped range), and the node keeps serving 0.4.0 pulls.
#[tokio::test(flavor = "multi_thread")]
async fn review2_catch_up_from_untrusted_older_peers_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut old_config = config(&dir.path().join("old"), &[]);
    old_config.history.legacy = true;
    let old = Node::start(old_config).await?;
    let now = now_secs();
    let a_dir = dir.path().join("a");
    std::fs::create_dir_all(&a_dir)?;
    Store::open(&a_dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now - 2 * DAY)?;
    let missed: Vec<Event> = (0..30u32)
        .map(|i| note(i, now - 3600 - u64::from(i) * 1800))
        .collect();
    hold(&old, &missed);
    let a = Node::start(config(&a_dir, &[])).await?;
    a.add_peer(old.endpoint().addr()).await?;
    tokio::time::sleep(Duration::from_secs(20)).await;
    eventually("the catch-up ends", 30, || {
        (state(&a) == "done").then_some(())
    })
    .await;
    let held = missed.iter().filter(|e| seq(&a, &e.id).is_some()).count();
    assert_eq!(held, missed.len());
    let ranges = a.store().sync_ranges(PUBLIC_NET)?;
    assert!(
        ranges.iter().all(|r| r.state == RangeState::Unconfirmed),
        "{ranges:?}"
    );
    assert!(a.store().sync_watermark(PUBLIC_NET)? > now - DAY);
    // A 0.4.0 node whose watermark is an hour old pulls from it.
    let mut l2 = config(&dir.path().join("l2"), &[&a]);
    l2.history.legacy = true;
    let l2 = Node::start(l2).await?;
    let pull = Request::sync_after(PUBLIC_NET, now - 3600, "", 50);
    let answer = wats::proto::call_solving(old.endpoint(), a.endpoint_id(), &pull).await?;
    assert!(matches!(answer, Response::SyncPage { .. }), "{answer:?}");
    let from_start = Request::sync_after(PUBLIC_NET, now - 2 * DAY, "", 50);
    match wats::proto::call_solving(l2.endpoint(), a.endpoint_id(), &from_start).await? {
        Response::SyncPage { events, .. } => assert_eq!(events.len(), missed.len()),
        other => panic!("{other:?}"),
    }
    for node in [a, old, l2] {
        node.shutdown().await?;
    }
    Ok(())
}

/// N1 interop: a node with no trusted peer (it met its only peer by
/// address, not by configuration) fills its history unconfirmed, then
/// keeps answering a 0.4.0 node's pull with all it holds. A trusted peer
/// met later confirms it.
#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_only_fill_keeps_serving_older_nodes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let h = reference(&dir.path().join("h")).await?;
    let spread = notes(30, now_secs() - 3600);
    hold(&h, &spread);
    let n = Node::start(config(&dir.path().join("n"), &[])).await?;
    n.add_peer(h.endpoint().addr()).await?;
    done(&n).await;
    assert!(spread.iter().all(|e| seq(&n, &e.id).is_some()));
    let ranges = n.store().sync_ranges(PUBLIC_NET)?;
    assert!(
        ranges.iter().any(|r| r.state == RangeState::Unconfirmed),
        "{ranges:?}"
    );
    assert!(
        ranges.iter().all(|r| r.state != RangeState::Cut),
        "{ranges:?}"
    );
    // A 0.4.0 node pulls everything from it.
    let mut legacy = config(&dir.path().join("legacy"), &[&n]);
    legacy.history.legacy = true;
    let legacy = Node::start(legacy).await?;
    eventually("the 0.4.0 node pulls it all", 60, || {
        spread
            .iter()
            .all(|e| seq(&legacy, &e.id).is_some())
            .then_some(())
    })
    .await;
    legacy.shutdown().await?;
    n.shutdown().await?;
    drop(n);
    // Restarted with `h` as a configured peer: `h` confirms it.
    let n = Node::start(config(&dir.path().join("n"), &[&h])).await?;
    eventually("a trusted peer confirms the history", 60, || {
        (!n.store()
            .sync_ranges(PUBLIC_NET)
            .unwrap()
            .iter()
            .any(|r| r.state == RangeState::Unconfirmed))
        .then_some(())
    })
    .await;
    assert_eq!(state(&n), "full");
    n.shutdown().await?;
    h.shutdown().await?;
    Ok(())
}

/// REVIEW3 (S-a): a node with no seed (as the router, whose only default
/// bootstrap is itself) catches up from two honest untrusted peers holding
/// the same history: the walk of one and the union check of the other
/// confirm it, and a newcomer that trusts it is confirmed in turn.
#[tokio::test(flavor = "multi_thread")]
async fn review3_two_honest_untrusted_peers_never_confirm() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let missed: Vec<Event> = (0..20u32)
        .map(|i| note(i, now - 600 - u64::from(i) * 60))
        .collect();
    let p1 = reference(&dir.path().join("p1")).await?;
    let p2 = reference(&dir.path().join("p2")).await?;
    hold(&p1, &missed);
    hold(&p2, &missed);
    let r_dir = dir.path().join("r");
    std::fs::create_dir_all(&r_dir)?;
    Store::open(&r_dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now - 3600)?;
    let mut r_config = config(&r_dir, &[]);
    r_config.initial_max = InitialMax::Bytes(0);
    let r = Node::start(r_config).await?;
    for p in [&p1, &p2] {
        r.add_peer(p.endpoint().addr()).await?;
    }
    eventually("two peers confirm the catch-up", 30, || {
        (missed.iter().all(|e| seq(&r, &e.id).is_some())
            && r.store().sync_ranges(PUBLIC_NET).unwrap().is_empty()
            && state(&r) == "full")
            .then_some(())
    })
    .await;
    assert!(r.history_full());
    // A newcomer that trusts r.
    let n = Node::start(config(&dir.path().join("n"), &[&r])).await?;
    eventually("the newcomer is confirmed", 30, || {
        (state(&n) == "full").then_some(())
    })
    .await;
    assert!(missed.iter().all(|e| seq(&n, &e.id).is_some()));
    for node in [n, r, p1, p2] {
        node.shutdown().await?;
    }
    Ok(())
}

/// REVIEW3 (R1): an honest but incomplete untrusted peer (it missed 5
/// events in the middle, as gossip loses messages) and a complete one.
/// Whichever is walked first, the other gets the union check from the
/// range's top, and the newcomer holds all 40.
#[tokio::test(flavor = "multi_thread")]
async fn review3_interior_events_held_only_by_the_second_peer() -> anyhow::Result<()> {
    let mut missed_runs = 0;
    for _ in 0..6 {
        let dir = tempfile::tempdir()?;
        let all = notes(40, now_secs() - 3600);
        let p = reference(&dir.path().join("p")).await?;
        let h = reference(&dir.path().join("h")).await?;
        let interior: Vec<Event> = all[15..20].to_vec();
        hold(&p, &all[..15]);
        hold(&p, &all[20..]);
        hold(&h, &all);
        let n = Node::start(config(&dir.path().join("n"), &[])).await?;
        for peer in [&p, &h] {
            n.add_peer(peer.endpoint().addr()).await?;
        }
        done(&n).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let got = interior.iter().filter(|e| seq(&n, &e.id).is_some()).count();
        let total = all.iter().filter(|e| seq(&n, &e.id).is_some()).count();
        assert_eq!(total, all.len(), "interior {got}/5, state {}", state(&n));
        if got < interior.len() {
            missed_runs += 1;
        }
        for node in [n, p, h] {
            node.shutdown().await?;
        }
    }
    assert_eq!(missed_runs, 0);
    Ok(())
}

/// REVIEW4 (A): the same with the missed events deeper than the union
/// check's two held pages (page 16 here, 256 in production): 80 notes, P
/// missed notes 50..55. With no trusted walker, H's union read goes to the
/// range's bottom: the 5 come, and the range is never confirmed without
/// them.
#[tokio::test(flavor = "multi_thread")]
async fn review4_deep_interior_events_and_a_confirmed_range() -> anyhow::Result<()> {
    let mut missed_runs = 0;
    for _ in 0..6 {
        let dir = tempfile::tempdir()?;
        let all = notes(80, now_secs() - 3600);
        let p = reference(&dir.path().join("p")).await?;
        let h = reference(&dir.path().join("h")).await?;
        let interior: Vec<Event> = all[50..55].to_vec();
        hold(&p, &all[..50]);
        hold(&p, &all[55..]);
        hold(&h, &all);
        let n = Node::start(config(&dir.path().join("n"), &[])).await?;
        for peer in [&p, &h] {
            n.add_peer(peer.endpoint().addr()).await?;
        }
        done(&n).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let got = interior.iter().filter(|e| seq(&n, &e.id).is_some()).count();
        let ranges = n.store().sync_ranges(PUBLIC_NET)?;
        assert_eq!(
            got,
            interior.len(),
            "state {}, ranges {ranges:?}",
            state(&n)
        );
        if got < interior.len() {
            missed_runs += 1;
        }
        for node in [n, p, h] {
            node.shutdown().await?;
        }
    }
    assert_eq!(missed_runs, 0);
    Ok(())
}

/// REVIEW4 (1): the same as review3_two_honest… with the second peer
/// coming up a few seconds after the first (as neighbors arrive one by one
/// at a start): the later neighbor's check confirms the range.
#[tokio::test(flavor = "multi_thread")]
async fn review4_second_peer_later_never_confirms() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let missed: Vec<Event> = (0..20u32)
        .map(|i| note(i, now - 600 - u64::from(i) * 60))
        .collect();
    let p1 = reference(&dir.path().join("p1")).await?;
    let p2 = reference(&dir.path().join("p2")).await?;
    hold(&p1, &missed);
    hold(&p2, &missed);
    let r_dir = dir.path().join("r");
    std::fs::create_dir_all(&r_dir)?;
    Store::open(&r_dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now - 3600)?;
    let mut r_config = config(&r_dir, &[]);
    r_config.initial_max = InitialMax::Bytes(0);
    let r = Node::start(r_config).await?;
    r.add_peer(p1.endpoint().addr()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    r.add_peer(p2.endpoint().addr()).await?;
    eventually("the second peer confirms", 40, || {
        (missed.iter().all(|e| seq(&r, &e.id).is_some())
            && r.store().sync_ranges(PUBLIC_NET).unwrap().is_empty()
            && state(&r) == "full")
            .then_some(())
    })
    .await;
    assert!(r.history_full());
    for node in [r, p1, p2] {
        node.shutdown().await?;
    }
    Ok(())
}

/// REVIEW5: two 0.4.1 peers that were offline over the span (quiet there,
/// one event each below the floor: what two colluding fresh ids can fake
/// cheaply) and a 0.4.0 neighbor that holds the span but stops answering
/// after one page (rate limit). Its events void the quiet claims, and its
/// failed read keeps the range unconfirmed: never `full` with the span
/// missing.
#[tokio::test(flavor = "multi_thread")]
async fn review5_quiet_claims_and_an_older_holder_that_fails() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let below = note(900, now - 3 * DAY);
    let c1 = reference(&dir.path().join("c1")).await?;
    let c2 = reference(&dir.path().join("c2")).await?;
    hold(&c1, std::slice::from_ref(&below));
    hold(&c2, std::slice::from_ref(&below));
    let mut h_config = config(&dir.path().join("h"), &[]);
    h_config.history.legacy = true;
    h_config.limits.challenge = false;
    h_config.limits.sync_per_min = 1;
    let h = Node::start(h_config).await?;
    let span: Vec<Event> = (0..40u32)
        .map(|i| note(i, now - 3600 - u64::from(i) * 600))
        .collect();
    hold(&h, &span);
    let a_dir = dir.path().join("a");
    std::fs::create_dir_all(&a_dir)?;
    Store::open(&a_dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now - 2 * DAY)?;
    let a = Node::start(config(&a_dir, &[])).await?;
    for p in [&c1, &c2, &h] {
        a.add_peer(p.endpoint().addr()).await?;
    }
    tokio::time::sleep(Duration::from_secs(20)).await;
    let held = span.iter().filter(|e| seq(&a, &e.id).is_some()).count();
    let ranges = a.store().sync_ranges(PUBLIC_NET)?;
    assert!(held > 0, "the 0.4.0 holder served a page");
    if held < span.len() {
        assert_ne!(state(&a), "full", "{held}/40, ranges {ranges:?}");
        assert!(!a.history_full());
        assert!(
            ranges
                .iter()
                .any(|r| matches!(r.state, RangeState::Unconfirmed | RangeState::Pending)),
            "{held}/40, ranges {ranges:?}"
        );
    }
    for node in [a, c1, c2, h] {
        node.shutdown().await?;
    }
    Ok(())
}

/// REVIEW5, the ordering the test above hits now and then: the quiet
/// claims confirm the catch-up range before the 0.4.0 holder is a
/// neighbor; it comes up before the pass ends, so the check of the
/// neighbors that came up reads it from the watermark the pass started
/// with, below the range just confirmed. It serves a page that range
/// lacked, then stops (rate limit): the span is unconfirmed again, never
/// `full` with it missing. Forced here: the holder starts once the range
/// is confirmed, the watermark just ahead (no catch-up range opens) and
/// the span within the skew below it, where the check reads.
#[tokio::test(flavor = "multi_thread")]
async fn review5_an_older_holder_that_comes_up_after_the_quiet_claims() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let below = note(900, now - 3 * DAY);
    let c1 = reference(&dir.path().join("c1")).await?;
    let c2 = reference(&dir.path().join("c2")).await?;
    hold(&c1, std::slice::from_ref(&below));
    hold(&c2, std::slice::from_ref(&below));
    let a_dir = dir.path().join("a");
    std::fs::create_dir_all(&a_dir)?;
    Store::open(&a_dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now - 2 * DAY)?;
    let a = Node::start(config(&a_dir, &[])).await?;
    for p in [&c1, &c2] {
        a.add_peer(p.endpoint().addr()).await?;
    }
    eventually("the quiet claims confirm the range", 30, || {
        (a.store().sync_ranges(PUBLIC_NET).unwrap().is_empty() && state(&a) == "full").then_some(())
    })
    .await;
    a.store().set_sync_watermark(PUBLIC_NET, now_secs() + 60)?;
    let mut h_config = config(&dir.path().join("h"), &[]);
    h_config.history.legacy = true;
    h_config.limits.challenge = false;
    h_config.limits.sync_per_min = 1;
    let h = Node::start(h_config).await?;
    let span: Vec<Event> = (0..40u32)
        .map(|i| note(i, now - 400 + u64::from(i) * 5))
        .collect();
    hold(&h, &span);
    a.add_peer(h.endpoint().addr()).await?;
    eventually("the holder serves a page", 30, || {
        span.iter().any(|e| seq(&a, &e.id).is_some()).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let held = span.iter().filter(|e| seq(&a, &e.id).is_some()).count();
    let ranges = a.store().sync_ranges(PUBLIC_NET)?;
    assert!(held < span.len(), "one page only: {held}/40");
    assert_ne!(state(&a), "full", "{held}/40, ranges {ranges:?}");
    assert!(!a.history_full());
    assert!(
        ranges.iter().any(|r| r.state == RangeState::Unconfirmed),
        "{held}/40, ranges {ranges:?}"
    );
    for node in [a, c1, c2, h] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Key age from the peers (ADR 0025, "Key age"): a new node holds an old
/// key's events since a minute, but three peers, one with full history,
/// saw it 40 days ago. The fill reads one peer; the announces asked of
/// each neighbor afterwards bring the others' `seen`, and the key's age
/// floor counts from their median, not from this node's first sight. The
/// probe stores nothing it does not hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_node_takes_an_old_keys_age_from_its_peers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let now = now_secs();
    let old = now - 40 * DAY;
    let agent = key(77);
    let events = vec![
        root(Kind::Announce, "an old agent", &agent, old),
        root(Kind::Note, "still posting", &agent, now - 5 * DAY),
    ];
    // Newer traffic, more than the union check's two held pages (16 here):
    // only the walked peer serves the key's events in the fill.
    let traffic = notes(60, now - 600);
    // No budget, no hole: p1 advertises full history. p2 and p3 have a
    // budget, so they do not.
    let p1 = reference(&dir.path().join("p1")).await?;
    let partial = |name: &str| {
        let dir = dir.path().join(name);
        async move {
            std::fs::create_dir_all(&dir)?;
            Store::open(&dir.join("events.sqlite"))?.set_sync_watermark(PUBLIC_NET, now + DAY)?;
            let mut config = config(&dir, &[]);
            config.initial_max = InitialMax::Bytes(1 << 30);
            Node::start(config).await
        }
    };
    let p2 = partial("p2").await?;
    let p3 = partial("p3").await?;
    for peer in [&p1, &p2, &p3] {
        hold(peer, &events);
        hold(peer, &traffic);
    }
    // An announce only p2 holds, older than what the check of a neighbor
    // that comes up later reads: the probe reads it but stores nothing new
    // (the fill's per-key share and budget would not apply).
    let only_p2 = root(Kind::Announce, "only p2 has it", &key(78), now - 30 * DAY);
    hold(&p2, std::slice::from_ref(&only_p2));
    assert!(p1.history_full(), "the reference with full history");
    assert!(!p2.history_full());
    // The fill walks p1, its only neighbor: whichever neighbor is up first
    // is walked, and phase 0 rightly stores the announces it serves. p2
    // and p3 come up once the fill is done, so that only the probe and the
    // cheap check of a new neighbor (since the watermark) read them.
    let mut n_config = config(&dir.path().join("n"), &[&p1]);
    n_config.reputation.ttl = Duration::ZERO;
    n_config.reputation.max_age = Duration::ZERO;
    let n = Node::start(n_config).await?;
    done(&n).await;
    for peer in [&p2, &p3] {
        n.add_peer(peer.endpoint().addr()).await?;
    }
    let author = events[0].author.clone();
    let first = n.store().author_first_seen(&author)?.unwrap_or(0);
    assert!(first + 600 > now, "stored here just now");
    let (since, floor) = eventually("the peers' median", 30, || {
        let table = n.reputation(PUBLIC_NET).ok()?;
        let a = table.authors.get(&author)?;
        (a.known_since == Some(old)).then_some((a.known_since, a.floor))
    })
    .await;
    assert_eq!(since, Some(old));
    assert!(floor >= wats::reputation::AGE_FLOOR_CAP, "{floor}");
    let reports = n.store().seen_reports()?;
    let reports = &reports[&author];
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert_eq!(reports.iter().filter(|(_, full)| *full).count(), 1);
    // p2's report came only from the probe, after the page that also held
    // `only_p2` (newer, so before the old key's announce): that was read,
    // and neither stored nor reported.
    assert!(seq(&n, &only_p2.id).is_none(), "the probe stored an event");
    assert!(!n.store().seen_reports()?.contains_key(&only_p2.author));
    for node in [n, p1, p2, p3] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Two neighbors, both caught up; an event reaches one of them outside
/// gossip (as when a broadcast was lost on the way). The other gets it
/// while both stay online, with no restart and no new neighbor: here by
/// the periodic passes of its history task (its start's range still
/// covers the event), else by anti-entropy (the next test).
#[tokio::test(flavor = "multi_thread")]
async fn anti_entropy_brings_what_gossip_missed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = reference(&dir.path().join("a")).await?;
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.history.anti_entropy = Duration::from_secs(1);
    let b = Node::start(b_config).await?;
    done(&b).await;
    let missed = note(4242, now_secs() - 60);
    hold(&a, std::slice::from_ref(&missed));
    eventually("anti-entropy brings the missed event", 30, || {
        seq(&b, &missed.id).map(|_| ())
    })
    .await;
    // `status` counts it as late (0.4.12).
    for node in [a, b] {
        node.shutdown().await?;
    }
    Ok(())
}

/// What anti-entropy pulls goes on gossip at once: a chain A - B - C,
/// where only B reads its neighbors. An event reaches A outside gossip; B
/// pulls it and relays it, and C, which never pulls, gets it live.
#[tokio::test(flavor = "multi_thread")]
async fn anti_entropy_relays_what_it_pulls() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let a = reference(&dir.path().join("a")).await?;
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.history.anti_entropy = Duration::from_secs(1);
    let b = Node::start(b_config).await?;
    done(&b).await;
    let mut c_config = config(&dir.path().join("c"), &[&b]);
    c_config.history.anti_entropy = Duration::ZERO;
    let c = Node::start(c_config).await?;
    done(&c).await;
    eventually("C is B's gossip neighbor", 30, || {
        (c.neighbor_count(PUBLIC_NET) > 0).then_some(())
    })
    .await;
    let missed = note(4343, now_secs() - 30);
    hold(&a, std::slice::from_ref(&missed));
    eventually("C gets the relayed event", 30, || {
        seq(&c, &missed.id).map(|_| ())
    })
    .await;
    for node in [a, b, c] {
        node.shutdown().await?;
    }
    Ok(())
}

/// The gossip ping (0.4.11): B says it pings but never does, so A hears
/// nothing from it for three rounds and gives it a fresh gossip link.
/// Gossip from A still reaches B afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_gossip_neighbor_gets_a_fresh_link() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut b_config = config(&dir.path().join("b"), &[]);
    b_config.gossip_ping = Duration::from_secs(3600);
    let b = Node::start(b_config).await?;
    let mut a_config = config(&dir.path().join("a"), &[&b]);
    a_config.gossip_ping = Duration::from_millis(300);
    let a = Node::start(a_config).await?;
    eventually("A resets the silent link", 30, || {
        (a.link_resets() > 0).then_some(())
    })
    .await;
    assert_eq!(b.link_resets(), 0, "B heard A's pings");
    // `status` names the peer and why (0.4.12).
    let gossip = a.gossip_json();
    assert_eq!(
        gossip["resets"][0]["peer"],
        b.endpoint().id().to_string(),
        "{gossip}"
    );
    assert!(
        gossip["resets"][0]["why"]
            .as_str()
            .unwrap()
            .starts_with("nothing heard from it"),
        "{gossip}"
    );
    let posted = a
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("after the reset".into()),
                text: "after the reset".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    eventually("B gets it by gossip", 30, || {
        seq(&b, &posted.id).map(|_| ())
    })
    .await;
    for node in [a, b] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Pings keep a quiet link alive (0.4.13 check): two nodes that both ping
/// every 300 ms, with nothing else to say, never judge each other silent.
#[tokio::test(flavor = "multi_thread")]
async fn pings_keep_a_quiet_link_alive() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut b_config = config(&dir.path().join("b"), &[]);
    b_config.gossip_ping = Duration::from_millis(300);
    let b = Node::start(b_config).await?;
    let mut a_config = config(&dir.path().join("a"), &[&b]);
    a_config.gossip_ping = Duration::from_millis(300);
    let a = Node::start(a_config).await?;
    eventually("A and B are gossip neighbors", 30, || {
        (a.neighbor_count(PUBLIC_NET) > 0 && b.neighbor_count(PUBLIC_NET) > 0).then_some(())
    })
    .await;
    // Ten rounds; three silent ones reset a link.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(a.link_resets(), 0, "A: {}", a.gossip_json());
    assert_eq!(b.link_resets(), 0, "B: {}", b.gossip_json());
    // `status` counts them (0.4.13).
    for n in [&a, &b] {
        let pings = &n.gossip_json()["pings"];
        assert!(pings["sent"].as_u64() > Some(3), "{pings}");
        assert!(pings["heard"].as_u64() > Some(3), "{pings}");
        assert_eq!(pings["failed"], 0, "{pings}");
    }
    for node in [a, b] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Experiment: a quiet mesh like the seeds' (a hub at active view 8, ten
/// nodes at 8 and two at 5), every node pinging every 300 ms.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "experiment"]
async fn pings_keep_a_quiet_mesh_alive() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut hub_config = config(&dir.path().join("hub"), &[]);
    hub_config.gossip_ping = Duration::from_millis(300);
    hub_config.gossip_active_view = 8;
    let hub = Node::start(hub_config).await?;
    let mut nodes = vec![hub];
    for i in 0..12 {
        let peers: Vec<&Arc<Node>> = nodes.iter().take(3).collect();
        let mut c = config(&dir.path().join(format!("n{i}")), &peers);
        c.gossip_ping = Duration::from_millis(300);
        c.gossip_active_view = if i < 10 { 8 } else { 5 };
        nodes.push(Node::start(c).await?);
    }
    tokio::time::sleep(Duration::from_secs(8)).await;
    let before: Vec<u64> = nodes.iter().map(|n| n.link_resets()).collect();
    tokio::time::sleep(Duration::from_secs(8)).await;
    let after: Vec<u64> = nodes.iter().map(|n| n.link_resets()).collect();
    for (i, n) in nodes.iter().enumerate() {
        eprintln!(
            "node {i}: neighbors {} resets {} -> {}",
            n.neighbor_count(PUBLIC_NET),
            before[i],
            after[i]
        );
    }
    for n in nodes {
        n.shutdown().await?;
    }
    Ok(())
}
