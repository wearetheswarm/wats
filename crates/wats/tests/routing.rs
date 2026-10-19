//! Routing and delivery modes (ADR 0022): runners on separate loopback
//! nodes, with fake agent CLIs. A "fake agent" is the test acting for it:
//! when its CLI is woken, the test reads the prompt and posts what the
//! agent would (a claim, an answer) with the agent's key.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};
use wats::agent::{AgentConfig, DEFAULT_HEARTBEAT};
use wats::event::{Draft, Event, Kind};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

async fn node(
    dir: &Path,
    peers: &[&Arc<Node>],
    tune: impl FnOnce(&mut NodeConfig),
) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    config.peers = peers.iter().map(|p| p.endpoint().addr()).collect();
    config.claim_jitter = Duration::ZERO;
    tune(&mut config);
    Node::start(config).await
}

/// A fake agent CLI: keeps the prompt read on stdin as `run-<n>.prompt`
/// and prints a Claude Code result line.
fn fake_agent(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let out = dir.join("runs");
    std::fs::create_dir_all(&out).unwrap();
    let script = dir.join("fake-claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nn=$(ls {out} | grep -c prompt)\ncat > {out}/run-$n.tmp\nmv {out}/run-$n.tmp {out}/run-$n.prompt\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\"total_cost_usd\":0.01}}'\n",
            out = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// The prompts of the runs so far, with when each started.
fn runs(dir: &Path) -> Vec<(String, SystemTime)> {
    let out = dir.join("runs");
    let mut runs = Vec::new();
    for n in 0.. {
        let path = out.join(format!("run-{n}.prompt"));
        let Ok(prompt) = std::fs::read_to_string(&path) else {
            break;
        };
        let at = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        runs.push((prompt, at));
    }
    runs
}

/// The runs that were woken for `ask`.
fn runs_for(dir: &Path, ask: &Event) -> Vec<(String, SystemTime)> {
    runs(dir)
        .into_iter()
        .filter(|(p, _)| p.contains(&ask.id))
        .collect()
}

/// What a prompt expects of the agent for `ask`: the end of its line in
/// the delivery section.
fn expected(prompt: &str, ask: &Event) -> Option<String> {
    prompt
        .lines()
        .find(|l| l.starts_with(&format!("- {}: mode ", ask.id)))
        .and_then(|l| l.split("Expected: ").nth(1))
        .map(str::to_string)
}

async fn eventually(what: &str, secs: u64, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn agent(root: &Path, data: &Path, caps: &[&str], every: Duration) -> AgentConfig {
    std::fs::create_dir_all(root).unwrap();
    AgentConfig {
        runtime: "claude".into(),
        profile: "agent".into(),
        user: "alice".into(),
        about: String::new(),
        caps: caps.iter().map(|c| c.to_string()).collect(),
        every,
        heartbeat: Duration::from_secs(3600),
        max_runs_per_hour: 100,
        allow: vec![],
        ignore_local: true,
        heartbeat_text: DEFAULT_HEARTBEAT.into(),
        command: fake_agent(root),
        mcp_command: "/usr/bin/true".into(),
        mcp_args: vec![],
        extra_args: vec![],
        run_timeout: Duration::from_secs(10),
        max_turns: 10,
        budget: wats::budget::Budget::default(),
        wakes: wats::wake::WakeLimits::unlimited(),
        codex_disable: vec![],
        access: wats::access::Access::Locked,
        workdir: PathBuf::from("/nonexistent"),
        tools: vec![],
        model: None,
        work_dir: root.join("work"),
        data_dir: data.to_path_buf(),
        keep_runs: 100,
        keep_days: 30,
        openclaw: Default::default(),
        catchup: None,
    }
}

struct Runner {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start(node: &Arc<Node>, config: AgentConfig) -> Runner {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(wats::agent::run(Arc::clone(node), config, async {
        let _ = stopped.await;
    }));
    Runner { stop, task }
}

impl Runner {
    async fn stop(self) -> anyhow::Result<()> {
        let _ = self.stop.send(());
        self.task.await?
    }
}

fn ask_with(text: &str, data: Value, tags: &[String]) -> Draft {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ask),
        title: Some(text.into()),
        text: text.into(),
        tags: tags.to_vec(),
        data: data.as_object().unwrap().clone(),
        ..Draft::default()
    }
}

fn reply(to: &Event, kind: Kind, data: Value) -> Draft {
    Draft {
        net: to.net.clone(),
        thread: Some(to.thread_root().to_string()),
        parent: Some(to.id.clone()),
        text: kind.as_str().to_string(),
        kind: Some(kind),
        data: data.as_object().cloned().unwrap_or_else(Map::new),
        ..Draft::default()
    }
}

/// The agent profile's author id on `node`.
fn agent_id(node: &Node) -> String {
    wats::identity::author_id(&node.author_key("agent").unwrap())
}

/// Posts of `kind` in the thread of `ask`, as `node` stores them.
fn in_thread(node: &Node, ask: &Event, kind: Kind) -> Vec<Arc<Event>> {
    node.store()
        .thread(&ask.id, 1000)
        .unwrap()
        .into_iter()
        .map(|s| s.event)
        .filter(|e| e.kind == kind)
        .collect()
}

/// Seen by every node.
async fn everywhere(nodes: &[&Arc<Node>], id: &str) {
    for n in nodes {
        let n = Arc::clone(n);
        let id = id.to_string();
        eventually("replicated", 10, move || {
            n.store().get(&id).unwrap().is_some()
        })
        .await;
    }
}

/// A race on a `one` ask: two agents on two nodes woke at the same time
/// and both claimed. The earliest claim (then the lowest id) wins on both
/// nodes; the loser's node posts a `release` and the loser is not woken
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn a_race_on_a_one_ask_is_settled_by_the_tie_break() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = node(&dir.path().join("hub"), &[], |_| {}).await?;
    let a = node(&dir.path().join("a"), &[&hub], |_| {}).await?;
    let b = node(&dir.path().join("b"), &[&hub], |_| {}).await?;
    for n in [&a, &b] {
        n.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
    }
    let every = Duration::from_millis(200);
    let (ra, rb) = (dir.path().join("ra"), dir.path().join("rb"));
    let runner_a = start(&a, agent(&ra, &dir.path().join("a"), &[], every));
    let runner_b = start(&b, agent(&rb, &dir.path().join("b"), &[], every));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let ask = hub
        .publish(
            ask_with("Compile this crate", json!({"mode": "one"}), &[]),
            &[],
        )
        .await?;
    eventually("both agents woke", 10, || {
        !runs_for(&ra, &ask).is_empty() && !runs_for(&rb, &ask).is_empty()
    })
    .await;
    for root in [&ra, &rb] {
        let (prompt, _) = runs_for(root, &ask).remove(0);
        let expected = expected(&prompt, &ask).unwrap_or_default();
        assert!(
            expected.contains("reply with kind `claim` before any work"),
            "{prompt}"
        );
        assert!(prompt.contains("mode one (a single worker"), "{prompt}");
    }
    // Both claim before seeing the other's claim.
    let (key_a, key_b) = (a.author_key("agent")?, b.author_key("agent")?);
    let (claim_a, claim_b) = tokio::join!(
        a.publish_as(&key_a, reply(&ask, Kind::Claim, json!({})), &[]),
        b.publish_as(&key_b, reply(&ask, Kind::Claim, json!({})), &[]),
    );
    let (claim_a, claim_b) = (claim_a?, claim_b?);
    let (winner, loser, loser_node) = if (claim_a.ts, &claim_a.id) < (claim_b.ts, &claim_b.id) {
        (&claim_a, &claim_b, &b)
    } else {
        (&claim_b, &claim_a, &a)
    };
    // The loser's node releases its claim, on its own.
    eventually("the loser's release", 10, || {
        in_thread(&hub, &ask, Kind::Release)
            .iter()
            .any(|r| r.parent.as_deref() == Some(loser.id.as_str()))
    })
    .await;
    let releases = in_thread(&hub, &ask, Kind::Release);
    assert_eq!(releases.len(), 1, "only the loser releases: {releases:?}");
    assert_eq!(releases[0].author, agent_id(loser_node));
    // Every node agrees: the winner's claim holds.
    everywhere(&[&hub, &a, &b], &releases[0].id).await;
    for n in [&hub, &a, &b] {
        let state = wats::delivery::state_on(n, &ask, wats::node::now_secs()).unwrap();
        assert_eq!(state.winners, std::slice::from_ref(&winner.id));
        assert_eq!(state.claims.len(), 1);
    }
    // The loser abstains: no other wake for the ask, no answer.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let loser_root = if std::ptr::eq(loser_node, &a) {
        &ra
    } else {
        &rb
    };
    assert_eq!(runs_for(loser_root, &ask).len(), 1);
    assert!(in_thread(&hub, &ask, Kind::Answer).is_empty());

    runner_a.stop().await?;
    runner_b.stop().await?;
    for n in [a, b, hub] {
        n.shutdown().await?;
    }
    Ok(())
}

/// `any` with a cap of 3 and 5 capable agents on 5 nodes: each fake agent
/// answers directly when the prompt says so. At most 3 answer, plus one
/// for a race the jitter did not prevent (ADR 0022 §2.2).
#[tokio::test(flavor = "multi_thread")]
async fn any_with_a_cap_of_three_and_five_agents() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = node(&dir.path().join("hub"), &[], |_| {}).await?;
    let mut nodes = Vec::new();
    let mut runners = Vec::new();
    let mut roots = Vec::new();
    for i in 0..5 {
        let data = dir.path().join(format!("n{i}"));
        let n = node(&data, &[&hub], |c| {
            c.claim_jitter = Duration::from_secs(4);
        })
        .await?;
        n.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
        let root = dir.path().join(format!("r{i}"));
        runners.push(start(
            &n,
            agent(&root, &data, &["rust"], Duration::from_millis(200)),
        ));
        nodes.push(n);
        roots.push(root);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let ask = hub
        .publish(
            ask_with(
                "What does this borrow error mean?",
                json!({"max": 3}),
                &["cap:rust".into()],
            ),
            &[],
        )
        .await?;
    // The fake agents: a light answer, at once, when told to answer.
    let mut answered = [false; 5];
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        for i in 0..5 {
            if answered[i] {
                continue;
            }
            let Some((prompt, _)) = runs_for(&roots[i], &ask).pop() else {
                continue;
            };
            let expected = expected(&prompt, &ask).unwrap_or_default();
            assert!(expected.contains("a light answer"), "{prompt}");
            nodes[i]
                .publish_as(
                    &nodes[i].author_key("agent")?,
                    reply(&ask, Kind::Answer, json!({})),
                    &[],
                )
                .await?;
            answered[i] = true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let worked = answered.iter().filter(|a| **a).count();
    assert!((3..=4).contains(&worked), "{worked} agents answered");
    let woken = roots
        .iter()
        .filter(|r| !runs_for(r, &ask).is_empty())
        .count();
    assert_eq!(woken, worked, "the others were not woken");

    for r in runners {
        r.stop().await?;
    }
    for n in nodes {
        n.shutdown().await?;
    }
    hub.shutdown().await?;
    Ok(())
}

/// A claim with a short lease and no progress: when it runs out, another
/// runner that held back wakes and claims.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_lease_frees_the_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = node(&dir.path().join("hub"), &[], |_| {}).await?;
    let a = node(&dir.path().join("a"), &[&hub], |_| {}).await?;
    let b = node(&dir.path().join("b"), &[&hub], |_| {}).await?;
    for n in [&a, &b] {
        n.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
    }
    let every = Duration::from_millis(200);
    let (ra, rb) = (dir.path().join("ra"), dir.path().join("rb"));
    let runner_a = start(&a, agent(&ra, &dir.path().join("a"), &[], every));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let ask = hub
        .publish(
            ask_with("Render this video", json!({"mode": "one"}), &[]),
            &[],
        )
        .await?;
    // a's agent claims for 3 s, then goes silent.
    eventually("a woke", 10, || !runs_for(&ra, &ask).is_empty()).await;
    let claim = a
        .publish_as(
            &a.author_key("agent")?,
            reply(&ask, Kind::Claim, json!({"lease": 3})),
            &[],
        )
        .await?;
    everywhere(&[&b], &claim.id).await;
    // b's runner starts now and finds the ask claimed: it holds back...
    let mut config = agent(&rb, &dir.path().join("b"), &[], every);
    config.catchup = Some(wats::agent::DEFAULT_CATCHUP);
    let runner_b = start(&b, config);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(runs_for(&rb, &ask).is_empty(), "the claim holds");
    // ...until the lease runs out.
    eventually("b woke after the lease", 10, || {
        !runs_for(&rb, &ask).is_empty()
    })
    .await;
    let (prompt, at) = runs_for(&rb, &ask).remove(0);
    let woke = at.duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
    assert!(
        woke >= claim.ts + 3,
        "woke at {woke}, lease ran out at {}",
        claim.ts + 3
    );
    assert!(prompt.contains("no live claim"), "{prompt}");
    assert!(
        expected(&prompt, &ask)
            .unwrap_or_default()
            .contains("reply with kind `claim`"),
        "{prompt}"
    );
    let second = b
        .publish_as(
            &b.author_key("agent")?,
            reply(&ask, Kind::Claim, json!({})),
            &[],
        )
        .await?;
    everywhere(&[&hub], &second.id).await;
    let state = wats::delivery::state_on(&hub, &ask, wats::node::now_secs()).unwrap();
    assert_eq!(state.winners, std::slice::from_ref(&second.id));
    // a's expired claim lost nothing: no release from a's node.
    assert!(in_thread(&hub, &ask, Kind::Release).is_empty());

    runner_a.stop().await?;
    runner_b.stop().await?;
    for n in [a, b, hub] {
        n.shutdown().await?;
    }
    Ok(())
}

/// The asker accepts an answer: the agent that held a claim gets it
/// released by its node, and a runner that comes later is not woken.
#[tokio::test(flavor = "multi_thread")]
async fn an_accept_stops_everyone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = node(&dir.path().join("hub"), &[], |_| {}).await?;
    let a = node(&dir.path().join("a"), &[&hub], |_| {}).await?;
    let b = node(&dir.path().join("b"), &[&hub], |_| {}).await?;
    for n in [&a, &b] {
        n.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
    }
    let every = Duration::from_millis(200);
    let (ra, rb) = (dir.path().join("ra"), dir.path().join("rb"));
    let runner_a = start(&a, agent(&ra, &dir.path().join("a"), &[], every));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let ask = hub
        .publish(ask_with("Build the release binaries", json!({}), &[]), &[])
        .await?;
    eventually("a woke", 10, || !runs_for(&ra, &ask).is_empty()).await;
    // a's agent claims before heavy work...
    let claim = a
        .publish_as(
            &a.author_key("agent")?,
            reply(&ask, Kind::Claim, json!({})),
            &[],
        )
        .await?;
    everywhere(&[&hub], &claim.id).await;
    // ...but someone else answers, and the asker accepts that answer.
    let answer = hub
        .publish_as(
            &hub.author_key("helper")?,
            reply(&ask, Kind::Answer, json!({})),
            &[],
        )
        .await?;
    let accept = hub
        .publish(reply(&answer, Kind::React, json!({"accept": true})), &[])
        .await?;
    // a's node releases its agent's claim.
    eventually("a's claim released", 10, || {
        in_thread(&hub, &ask, Kind::Release)
            .iter()
            .any(|r| r.parent.as_deref() == Some(claim.id.as_str()))
    })
    .await;
    // A runner that starts now does not wake for it.
    everywhere(&[&b], &accept.id).await;
    let mut config = agent(&rb, &dir.path().join("b"), &[], every);
    config.catchup = Some(wats::agent::DEFAULT_CATCHUP);
    let runner_b = start(&b, config);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(runs_for(&rb, &ask).is_empty(), "accepted: no wake");
    assert_eq!(runs_for(&ra, &ask).len(), 1, "a is not woken again");

    runner_a.stop().await?;
    runner_b.stop().await?;
    for n in [a, b, hub] {
        n.shutdown().await?;
    }
    Ok(())
}

/// `to:` with the full author id in base32 wakes the addressee at once,
/// bypassing its debounce; another agent leaves it alone until its
/// deadline, and wakes then since nobody answered. A prefix of an id
/// addresses nobody: the ask is a plain one.
#[tokio::test(flavor = "multi_thread")]
async fn to_wakes_the_addressee_at_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = node(&dir.path().join("hub"), &[], |_| {}).await?;
    let a = node(&dir.path().join("a"), &[&hub], |_| {}).await?;
    let b = node(&dir.path().join("b"), &[&hub], |_| {}).await?;
    for n in [&a, &b] {
        n.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
    }
    let every = Duration::from_secs(6);
    let (ra, rb) = (dir.path().join("ra"), dir.path().join("rb"));
    let runner_a = start(&a, agent(&ra, &dir.path().join("a"), &[], every));
    let runner_b = start(&b, agent(&rb, &dir.path().join("b"), &[], every));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A plain ask wakes both: their debounce starts.
    let plain = hub
        .publish(ask_with("Anyone?", json!({}), &[]), &[])
        .await?;
    eventually("both woke", 10, || {
        !runs_for(&ra, &plain).is_empty() && !runs_for(&rb, &plain).is_empty()
    })
    .await;
    // An ask to a's agent, with a deadline in 4 s.
    let to = wats::delivery::to_tag(&agent_id(&a)).unwrap();
    assert_eq!(to.len(), 55);
    let deadline = wats::node::now_secs() + 4;
    let addressed = hub
        .publish(
            ask_with(
                "Alice's agent: check this",
                json!({"deadline": deadline}),
                &[to],
            ),
            &[],
        )
        .await?;
    let posted = SystemTime::now();
    eventually("a woke at once", 3, || {
        !runs_for(&ra, &addressed).is_empty()
    })
    .await;
    let (_, at) = runs_for(&ra, &addressed).remove(0);
    assert!(
        at.duration_since(posted).unwrap_or_default() < Duration::from_secs(3),
        "the debounce ({every:?}) was bypassed"
    );
    // b leaves it alone until the deadline, then wakes: nobody answered.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(runs_for(&rb, &addressed).is_empty(), "not addressed to b");
    eventually("b woke after the deadline", 15, || {
        !runs_for(&rb, &addressed).is_empty()
    })
    .await;
    let (_, at) = runs_for(&rb, &addressed).remove(0);
    assert!(at.duration_since(SystemTime::UNIX_EPOCH)?.as_secs() >= deadline);

    // A hex prefix of a's id, with a far deadline:
    // nobody is addressed, so b does not wait for the deadline.
    let prefix = format!("to:{}", &agent_id(&a)[..32]);
    let deadline = wats::node::now_secs() + 600;
    let nobody = hub
        .publish(
            ask_with("A prefix", json!({"deadline": deadline}), &[prefix]),
            &[],
        )
        .await?;
    eventually("both woke for the prefix", 20, || {
        !runs_for(&ra, &nobody).is_empty() && !runs_for(&rb, &nobody).is_empty()
    })
    .await;

    runner_a.stop().await?;
    runner_b.stop().await?;
    for n in [a, b, hub] {
        n.shutdown().await?;
    }
    Ok(())
}

/// `cap:` wakes the agents with that capability first; the others wake
/// after `ask.cap_grace` only if none of them claimed or answered.
#[tokio::test(flavor = "multi_thread")]
async fn cap_wakes_capable_agents_first() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = node(&dir.path().join("hub"), &[], |_| {}).await?;
    let grace = |c: &mut NodeConfig| c.cap_grace = Duration::from_secs(3);
    let a = node(&dir.path().join("a"), &[&hub], grace).await?;
    let b = node(&dir.path().join("b"), &[&hub], grace).await?;
    for n in [&a, &b] {
        n.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
    }
    let every = Duration::from_millis(200);
    let (ra, rb) = (dir.path().join("ra"), dir.path().join("rb"));
    let runner_a = start(&a, agent(&ra, &dir.path().join("a"), &["pdf"], every));
    let runner_b = start(&b, agent(&rb, &dir.path().join("b"), &[], every));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // a can do PDFs and answers the first ask: b never wakes for it.
    let answered = hub
        .publish(
            ask_with("Merge these PDFs", json!({}), &["cap:pdf".into()]),
            &[],
        )
        .await?;
    eventually("a woke", 5, || !runs_for(&ra, &answered).is_empty()).await;
    a.publish_as(
        &a.author_key("agent")?,
        reply(&answered, Kind::Answer, json!({})),
        &[],
    )
    .await?;
    // a leaves the second one alone: b wakes after the grace.
    let ignored = hub
        .publish(
            ask_with("Split this PDF", json!({}), &["cap:pdf".into()]),
            &[],
        )
        .await?;
    eventually("a woke for the second", 5, || {
        !runs_for(&ra, &ignored).is_empty()
    })
    .await;
    eventually("b woke after the grace", 10, || {
        !runs_for(&rb, &ignored).is_empty()
    })
    .await;
    let (_, at) = runs_for(&rb, &ignored).remove(0);
    assert!(at.duration_since(SystemTime::UNIX_EPOCH)?.as_secs() >= ignored.ts + 3);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        runs_for(&rb, &answered).is_empty(),
        "a capable agent answered"
    );

    runner_a.stop().await?;
    runner_b.stop().await?;
    for n in [a, b, hub] {
        n.shutdown().await?;
    }
    Ok(())
}
