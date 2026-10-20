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
    config.history.max_attempts = 3;
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

    // A full node whose only peer has a cut is not misled: it records a
    // cut of its own, and does not say it holds the whole history.
    a.shutdown().await?;
    let mut c_config = config(&dir.path().join("c"), &[&b]);
    c_config.initial_max = InitialMax::Bytes(0);
    let c = Node::start(c_config).await?;
    done(&c).await;
    assert!(seq(&c, &recent.id).is_some());
    assert!(
        c.store()
            .sync_ranges(PUBLIC_NET)?
            .iter()
            .any(|r| r.state == RangeState::Cut)
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
/// not stay pending: each closes with a cut after `max_attempts` passes.
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
        assert!(
            node.store()
                .sync_ranges("acme")?
                .iter()
                .any(|r| r.state == RangeState::Cut),
            "nobody confirmed older acme history"
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

/// S6: a catch-up whose only peer stops at a hole everywhere is skipped,
/// with its dates, never counted as held: a full node then does not say
/// `history_full`, and refuses an ascending pull from below it.
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_nobody_confirms_is_skipped_not_done() -> anyhow::Result<()> {
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
    let skipped = eventually("a skipped range", 60, || {
        a.store()
            .sync_ranges(PUBLIC_NET)
            .unwrap()
            .into_iter()
            .find(|r| r.state == RangeState::Skipped)
    })
    .await;
    assert!(skipped.lo_ts <= now - 2 * DAY);
    assert!(
        !a.store()
            .sync_ranges(PUBLIC_NET)?
            .iter()
            .any(|r| r.state == RangeState::Done && r.phase.is_empty()),
        "never done"
    );
    assert!(!a.history_full());
    let line = wats::node::history::line(&a.history_json(PUBLIC_NET));
    assert!(line.contains("skipped"), "{line}");
    let pull = Request::sync_after(PUBLIC_NET, now - 2 * DAY, "", 50);
    assert!(matches!(
        wats::proto::call_solving(b.endpoint(), a.endpoint_id(), &pull).await?,
        Response::Refused { .. }
    ));
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// B1: a node with no history yet (watermark 0, no range) serves nothing
/// as complete: a 0.4.0 pull is refused, a descending page gets a gap, and
/// only a `partial` request gets what it holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_no_history_yet_serves_a_hole() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut a_config = config(&dir.path().join("a"), &[]);
    // Its fill stays pending: as before its first pass.
    a_config.history.max_attempts = 1000;
    let a = Node::start(a_config).await?;
    assert_eq!(a.history_holes(PUBLIC_NET).len(), 1, "before any pass");
    let mut b_config = config(&dir.path().join("b"), &[&a]);
    b_config.history.legacy = true;
    let b = Node::start(b_config).await?;
    let held = notes(4, now_secs() - 60);
    hold(&a, &held);
    let call = |request: Request| {
        let (b, a) = (Arc::clone(&b), a.endpoint_id());
        async move { wats::proto::call_solving(b.endpoint(), a, &request).await }
    };
    assert!(matches!(
        call(Request::sync_after(PUBLIC_NET, 0, "", 50)).await?,
        Response::Refused { .. }
    ));
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
