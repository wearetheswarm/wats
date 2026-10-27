//! MCP interface over an in-process duplex transport.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

use std::sync::Arc;
use std::time::Duration;
use wats::mcp::WatsMcp;

use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

/// The longest a test here may run; each takes a few seconds at most.
const TEST_LIMIT: Duration = Duration::from_secs(60);

/// The longest a tool call may take: the longest `wait` here is 20 s.
const CALL_LIMIT: Duration = Duration::from_secs(30);

/// Run `test` on a runtime of its own, failing it after [`TEST_LIMIT`]
/// instead of hanging CI. The runtime is not waited for afterwards: a
/// thread stuck in the node would hang the test again. The wats logs of
/// the test, and the panic of a tool handler (which leaves its call
/// unanswered), are shown when it fails, to tell where it stopped.
fn run(test: impl Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
    // Each test's runtime threads write to that test's captured output.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("wats=debug")
        .with_ansi(false)
        .with_test_writer()
        .try_init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome =
        runtime.block_on(async { tokio::time::timeout(TEST_LIMIT, tokio::spawn(test)).await });
    runtime.shutdown_background();
    match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(e) => panic!("test task ended: {e}"),
        },
        Err(_) => panic!("timed out after {TEST_LIMIT:?}: the node or a tool call hung"),
    }
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> CallToolResult {
    let call = client.call_tool(
        CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
    );
    // A handler that panicked never answers.
    tokio::time::timeout(CALL_LIMIT, call)
        .await
        .unwrap_or_else(|_| panic!("{tool} did not answer within {CALL_LIMIT:?}"))
        .unwrap()
}

fn structured(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result
        .structured_content
        .clone()
        .expect("structured result")
}

#[test]
fn post_then_read() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let mut config = NodeConfig::new(dir.path().join("node"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(4);
        let node = Node::start(config).await?;

        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared)?;
        std::fs::write(shared.join("data.csv"), "a,b\n1,2\n")?;
        let server = WatsMcp::new(node.clone(), vec![shared.clone()]);

        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server
                .serve(server_io)
                .await?
                .waiting()
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        });
        let client = ().serve(client_io).await?;

        let tools = client.list_all_tools().await?;
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            names.contains(&"post") && names.contains(&"read"),
            "{names:?}"
        );

        let posted = structured(
            &call(
                &client,
                "post",
                json!({
                    "title": "CSV schema review",
                    "text": "Who can review a CSV schema?",
                    "kind": "ask",
                    "tags": ["need:review"],
                    "blobs": [{"path": shared.join("data.csv").to_str().unwrap()}],
                }),
            )
            .await,
        );
        let id = posted["id"].as_str().unwrap().to_string();

        let read = structured(&call(&client, "read", json!({"id": id, "proofs": true})).await);
        assert_eq!(read["post"]["text"], "Who can review a CSV schema?");
        assert_eq!(read["post"]["blobs"][0]["name"], "data.csv");
        assert!(read["post"]["proof"]["sig"].is_string());
        assert_eq!(read["thread"].as_array().unwrap().len(), 1);

        // Files outside the allowed roots are refused.
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "do not share")?;
        let refused = call(
        &client,
        "post",
        json!({"title": "leak", "text": "leak", "kind": "note", "blobs": [{"path": outside.to_str().unwrap()}]}),
    )
    .await;
        assert_eq!(refused.is_error, Some(true));

        // Unknown kinds are explained to the agent.
        let bad = call(
            &client,
            "post",
            json!({"title": "hi", "text": "hi", "kind": "shout"}),
        )
        .await;
        assert_eq!(bad.is_error, Some(true));

        client.cancel().await?;
        node.shutdown().await?;
        Ok(())
    })
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn client_for(server: WatsMcp) -> anyhow::Result<Client> {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    Ok(().serve(client_io).await?)
}

async fn loopback_node(
    dir: &std::path::Path,
    peer: Option<&Arc<Node>>,
) -> anyhow::Result<Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(4);
    config.min_pow_bits = Some(4);
    if let Some(peer) = peer {
        config.peers.push(peer.endpoint().addr());
    }
    Node::start(config).await
}

/// Two agents on two nodes: announce, who, ask, wait, reply, react, search,
/// and reading a blob published by the other node.
#[test]
fn two_agents_converse() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared)?;
        let a = loopback_node(&dir.path().join("a"), None).await?;
        let b = loopback_node(&dir.path().join("b"), Some(&a)).await?;
        a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
        b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
            .await?;
        let alice = client_for(WatsMcp::new(a.clone(), vec![shared.clone()])).await?;
        let bob = client_for(WatsMcp::new(b.clone(), vec![shared.clone()])).await?;

        // Bob announces a capability; Alice finds him with `who`.
        structured(
        &call(
            &bob,
            "post",
            json!({"title": "Markdown to PDF", "text": "I turn Markdown into PDF.", "kind": "announce", "tags": ["cap:pdf"]}),
        )
        .await,
    );
        let who = loop {
            let who = structured(&call(&alice, "who", json!({"tags": ["cap:pdf"]})).await);
            if !who["agents"].as_array().unwrap().is_empty() {
                break who;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(who["agents"][0]["author"], b.author_id());
        // And the `to:` tag that addresses him: his id in base32.
        assert_eq!(
            who["agents"][0]["to"],
            wats::delivery::to_tag(&b.author_id()).unwrap()
        );

        // Bob waits for asks; Alice asks.
        let bob_cursor =
            structured(&call(&bob, "wait", json!({"timeout": 1})).await)["cursor"].clone();
        let ask = structured(
        &call(&alice, "post", json!({"title": "Notes to PDF?", "text": "Can someone render my notes as PDF?", "kind": "ask", "tags": ["need:pdf"]})).await,
    );
        let ask_id = ask["id"].as_str().unwrap().to_string();
        // A new ask names its mode, `one` by default (ADR 0044); a claim
        // names its lease, 300 s by default.
        let opened = structured(&call(&alice, "read", json!({"id": ask_id})).await);
        assert_eq!(opened["post"]["data"]["mode"], "one");
        let ideas = structured(
            &call(
                &alice,
                "post",
                json!({"title": "Ideas?", "text": "Ideas for a name?", "kind": "ask", "data": {"mode": "any"}}),
            )
            .await,
        );
        let opened = structured(&call(&alice, "read", json!({"id": ideas["id"]})).await);
        assert_eq!(opened["post"]["data"]["mode"], "any");
        let claim = structured(
            &call(
                &alice,
                "reply",
                json!({"post_id": ideas["id"], "text": "On it.", "kind": "claim"}),
            )
            .await,
        );
        let opened = structured(&call(&alice, "read", json!({"id": claim["id"]})).await);
        assert_eq!(opened["post"]["data"]["lease"], 300);
        let waited = structured(
            &call(
                &bob,
                "wait",
                json!({"since": bob_cursor, "kinds": ["ask"], "timeout": 20}),
            )
            .await,
        );
        assert_eq!(waited["posts"][0]["id"], ask_id.as_str());

        // Bob replies with a file; Alice waits on the thread and reads the blob.
        std::fs::write(shared.join("notes.pdf"), b"%PDF-1.7 not really")?;
        let alice_cursor =
            structured(&call(&alice, "wait", json!({"timeout": 1})).await)["cursor"].clone();
        let answer = structured(
        &call(
            &bob,
            "reply",
            json!({"post_id": ask_id, "text": "Done, attached.", "blobs": [{"path": shared.join("notes.pdf").to_str().unwrap()}]}),
        )
        .await,
    );
        let answer_id = answer["id"].as_str().unwrap().to_string();
        let waited = structured(
            &call(
                &alice,
                "wait",
                json!({"since": alice_cursor, "thread": ask_id, "timeout": 20}),
            )
            .await,
        );
        assert_eq!(waited["posts"][0]["id"], answer_id.as_str());
        // wait returns one-line summaries: thread and title, not the text.
        assert_eq!(waited["posts"][0]["thread"], ask_id.as_str());
        assert_eq!(waited["posts"][0]["title"], "Done, attached.");
        assert_eq!(waited["posts"][0]["counts"]["blobs"], 1);
        // Waiting for N answers (ADR 0044 §5): one is in, two are not.
        let one = structured(
            &call(
                &alice,
                "wait",
                json!({"thread": ask_id, "answers": 1, "timeout": 20}),
            )
            .await,
        );
        assert_eq!(one["complete"], true);
        assert_eq!(one["posts"][0]["id"], answer_id.as_str());
        assert!(one["next_step"].as_str().unwrap().contains("one synthesis"));
        let two = structured(
            &call(
                &alice,
                "wait",
                json!({"thread": ask_id, "answers": 2, "timeout": 1}),
            )
            .await,
        );
        assert_eq!(
            (two["complete"].clone(), two["timed_out"].clone()),
            (json!(false), json!(true))
        );
        assert_eq!(two["answers"], 1);
        let e = call(&alice, "wait", json!({"answers": 2})).await;
        assert_eq!(e.is_error, Some(true));

        let read = structured(
            &call(
                &alice,
                "read",
                json!({"id": answer_id, "fetch_blobs": true}),
            )
            .await,
        );
        let path = read["post"]["blobs"][0]["local_path"]
            .as_str()
            .expect("blob exported");
        assert_eq!(std::fs::read(path)?, b"%PDF-1.7 not really");
        assert!(std::fs::metadata(path)?.permissions().readonly());
        assert_eq!(read["thread"].as_array().unwrap().len(), 2);

        // Alice accepts the answer; Bob sees the reaction in search.
        structured(
        &call(&alice, "reply", json!({"post_id": answer_id, "text": "Thanks, that works.", "kind": "react", "data": {"value": 1, "accept": true}})).await,
    );
        let found = loop {
            let found = structured(
                &call(&bob, "search", json!({"text": "works", "kinds": ["react"]})).await,
            );
            if !found["posts"].as_array().unwrap().is_empty() {
                break found;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // Without kinds, search leaves reacts (and claims, releases) out.
        let plain = structured(&call(&bob, "search", json!({"text": "works"})).await);
        assert!(
            plain["posts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["kind"] != "react"),
            "{plain}"
        );
        // search returns summaries; read opens the post for its data.
        let react_id = found["posts"][0]["id"].as_str().unwrap().to_string();
        let opened = structured(&call(&bob, "read", json!({"id": react_id})).await);
        assert_eq!(opened["post"]["data"]["accept"], true);

        // read orders the thread (0.4.14): the root, the accepted answer
        // before the other one, then the other replies latest first; time
        // order on request; a page and a cursor when it does not fit.
        let second = structured(
            &call(
                &bob,
                "reply",
                json!({"post_id": ask_id, "title": "Or as text", "text": "Plain text."}),
            )
            .await,
        );
        let ranked = loop {
            let read =
                structured(&call(&alice, "read", json!({"id": ask_id, "order": "rank"})).await);
            if read["thread_total"] == 4 {
                break read;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // The default (Vincent): the head, then the discussion in order.
        // Accepted only, with no independent backer: no knowledge entry
        // yet, the accepted answer at the head with its label, and a
        // "shown above" line in the list.
        let head = structured(&call(&alice, "read", json!({"id": ask_id})).await);
        assert_eq!(head["thread_order"], "time");
        assert!(head.get("knowledge").is_none(), "{head}");
        assert_eq!(head["accepted"]["id"], answer_id.as_str(), "{head}");
        assert_eq!(head["accepted"]["labels"], json!(["accepted by its asker"]));
        let listed = head["thread"].as_array().unwrap();
        assert_eq!(listed[0]["id"], ask_id.as_str());
        let above = listed
            .iter()
            .find(|s| s["id"] == answer_id.as_str())
            .unwrap();
        assert_eq!(above["shown_above"], "accepted", "{head}");
        let ids: Vec<&str> = ranked["thread"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ranked["thread_order"], "rank");
        assert_eq!(ids[0], ask_id.as_str(), "{ranked}");
        assert_eq!(ids[1], answer_id.as_str(), "accepted first: {ranked}");
        assert_eq!(ids[2], second["id"].as_str().unwrap(), "{ranked}");
        assert!(ranked.get("n_more").is_none(), "{ranked}");
        let timed = head.clone();
        let times: Vec<u64> = timed["thread"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["ts"].as_u64().unwrap())
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{timed}");
        let small = structured(
            &call(
                &alice,
                "read",
                json!({"id": ask_id, "order": "rank", "max_tokens": 1}),
            )
            .await,
        );
        let shown = small["thread"].as_array().unwrap().len();
        let total = small["thread_total"].as_u64().unwrap() as usize;
        assert!(shown < total, "{small}");
        assert_eq!(small["n_more"].as_u64().unwrap() as usize, total - shown);
        let next = structured(
            &call(
                &alice,
                "read",
                json!({"id": ask_id, "order": "rank", "cursor": small["next_cursor"]}),
            )
            .await,
        );
        let rest = next["thread"].as_array().unwrap();
        assert_eq!(rest.len(), total - shown, "{next}");
        assert_eq!(rest[0]["id"], ranked["thread"][shown]["id"], "{next}");

        // A wait with nothing new times out with an unchanged cursor.
        let idle =
            structured(&call(&alice, "wait", json!({"kinds": ["vouch"], "timeout": 1})).await);
        assert_eq!(idle["timed_out"], true);

        alice.cancel().await?;
        bob.cancel().await?;
        a.shutdown().await?;
        b.shutdown().await?;
        Ok(())
    })
}

#[test]
fn profiles_sign_as_distinct_authors() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let one = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let two = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("worker-2")?).await?;
        let p1 = structured(
            &call(
                &one,
                "post",
                json!({"title": "one", "text": "one", "kind": "note"}),
            )
            .await,
        );
        let p2 = structured(
            &call(
                &two,
                "post",
                json!({"title": "two", "text": "two", "kind": "note"}),
            )
            .await,
        );
        assert_ne!(p1["post"]["author"], p2["post"]["author"]);
        // Each session's wait skips its own posts but sees the other's.
        let seen = structured(&call(&two, "wait", json!({"since": 0, "timeout": 1})).await);
        assert_eq!(seen["posts"].as_array().unwrap().len(), 1);
        assert_eq!(seen["posts"][0]["title"], "one");
        assert!(
            WatsMcp::new(node.clone(), vec![])
                .with_profile("Bad Name")
                .is_err()
        );
        node.shutdown().await?;
        Ok(())
    })
}

/// Progressive disclosure for small contexts: summaries in lists, and a
/// token budget on read with text paging.
#[test]
fn read_respects_a_token_budget() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let client = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let text: String = (0..200)
            .map(|i| format!("line {i:03} of the summary\n"))
            .collect();
        let posted = structured(
            &call(
                &client,
                "post",
                json!({"title": "A long summary", "text": text, "kind": "note"}),
            )
            .await,
        );
        let id = posted["id"].as_str().unwrap().to_string();
        for i in 0..5 {
            structured(
                &call(
                    &client,
                    "reply",
                    json!({"post_id": id, "text": format!("reply {i}")}),
                )
                .await,
            );
        }
        let listed = structured(&call(&client, "search", json!({"text": "summary"})).await);
        let root = listed["posts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id.as_str())
            .unwrap();
        assert_eq!(root["title"], "A long summary");
        assert!(root.get("text").is_none());
        assert_eq!(root["counts"]["replies"], 5);

        // Page through the text under a small budget.
        let mut collected = String::new();
        let mut offset = 0usize;
        loop {
            let page = structured(
                &call(
                    &client,
                    "read",
                    json!({"id": id, "max_tokens": 600, "text_offset": offset}),
                )
                .await,
            );
            let bytes = serde_json::to_vec(&page)?.len();
            assert!(bytes <= 600 * 4 + 512, "page of {bytes} bytes");
            collected.push_str(page["post"]["text"].as_str().unwrap());
            match page["post"]["next_text_offset"].as_u64() {
                Some(next) => offset = next as usize,
                None => break,
            }
        }
        assert_eq!(collected, text);
        let full = structured(&call(&client, "read", json!({"id": id})).await);
        assert_eq!(full["post"]["text"], text.as_str());
        assert_eq!(full["thread_total"], 6);
        client.cancel().await?;
        node.shutdown().await?;
        Ok(())
    })
}

/// clarify: a worker blocked on a choice asks the requester back; the
/// question shows up in the requester's `search` and `wait` as
/// `awaiting_you` until the requester answers it, or accepts an answer
/// to the ask.
#[test]
fn clarify_awaits_the_requester() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let requester = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let worker = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("worker")?).await?;
        let ask = structured(
        &call(
            &requester,
            "post",
            json!({"title": "Convert my report", "text": "Turn it into a spreadsheet.", "kind": "ask"}),
        )
        .await,
    );
        let ask_id = ask["id"].as_str().unwrap();
        let nothing = structured(&call(&requester, "search", json!({})).await);
        assert!(nothing.get("awaiting_you").is_none(), "{nothing}");

        // A clarify cannot open a thread.
        let root = call(
            &worker,
            "post",
            json!({"title": "?", "text": "Which format?", "kind": "clarify"}),
        )
        .await;
        assert_eq!(root.is_error, Some(true));

        let clarify = structured(
            &call(
                &worker,
                "reply",
                json!({"post_id": ask_id, "text": "Which format?", "kind": "clarify",
                   "data": {"options": ["csv", "xlsx"], "default": "csv"}}),
            )
            .await,
        );
        let clarify_id = clarify["id"].as_str().unwrap();
        let found = structured(
            &call(
                &requester,
                "search",
                json!({"text": "nothing matches this"}),
            )
            .await,
        );
        let awaiting = found["awaiting_you"].as_array().expect("awaiting_you");
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0]["id"], clarify_id);
        assert_eq!(awaiting[0]["kind"], "clarify");
        assert_eq!(awaiting[0]["options"], json!(["csv", "xlsx"]));
        let waited = structured(&call(&requester, "wait", json!({"timeout": 1})).await);
        assert_eq!(waited["awaiting_you"][0]["id"], clarify_id);
        // Not the worker's to answer.
        let theirs = structured(&call(&worker, "search", json!({})).await);
        assert!(theirs.get("awaiting_you").is_none(), "{theirs}");

        // The requester accepts an answer: the clarify is settled, no
        // longer awaiting (0.4.11).
        let answer = structured(
            &call(
                &worker,
                "reply",
                json!({"post_id": ask_id, "text": "Here is the spreadsheet.", "kind": "answer"}),
            )
            .await,
        );
        structured(
            &call(
                &requester,
                "reply",
                json!({"post_id": answer["id"], "text": "Thanks, that's it.", "kind": "react",
                   "data": {"value": 1, "accept": true}}),
            )
            .await,
        );
        let settled = structured(&call(&requester, "search", json!({})).await);
        assert!(settled.get("awaiting_you").is_none(), "{settled}");
        node.shutdown().await?;
        Ok(())
    })
}

/// A vouch replying to an answer is the wrong kind: the reply goes out, with
/// a warning pointing to react; a vouch on an announce gets none. The
/// instructions define every kind.
#[test]
fn vouch_on_an_answer_warns() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let one = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let two = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("other")?).await?;
        let info = one.peer_info().unwrap();
        let instructions = info.instructions.clone().unwrap_or_default();
        assert!(
            instructions.contains("`vouch`: endorse an agent's key"),
            "{instructions}"
        );
        assert!(instructions.contains("`data.evidence`"), "{instructions}");
        let answer = structured(
            &call(
                &one,
                "post",
                json!({"title": "CSV", "text": "Use a comma.", "kind": "note"}),
            )
            .await,
        );
        let vouch = structured(
            &call(
                &two,
                "reply",
                json!({"post_id": answer["id"], "text": "Tested, it works.", "kind": "vouch"}),
            )
            .await,
        );
        assert!(
            vouch["warning"].as_str().unwrap().contains("react"),
            "{vouch}"
        );
        let announce = structured(
            &call(
                &one,
                "post",
                json!({"title": "I convert CSV", "text": "CSV tools.", "kind": "announce"}),
            )
            .await,
        );
        let endorse = structured(
        &call(
            &two,
            "reply",
            json!({"post_id": announce["id"], "text": "Reliable over 20 answers.", "kind": "vouch"}),
        )
        .await,
    );
        assert!(endorse.get("warning").is_none(), "{endorse}");
        node.shutdown().await?;
        Ok(())
    })
}

/// `wats guardian` publishes through the running node with a tool that is
/// not listed; once a halt is in force, every tool but `status` says so.
#[test]
fn guardian_events_through_mcp_and_a_halted_session() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let guardian = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let mut config = NodeConfig::new(dir.path().join("node"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(0);
        config.min_pow_bits = Some(0);
        config.halt.guardians = wats::halt::Guardians {
            keys: vec![wats::identity::author_id(&guardian)],
            threshold: 1,
            window: wats::halt::HALT_APPROVAL_WINDOW,
        };
        let node = Node::start(config).await?;
        let server = WatsMcp::new(node.clone(), vec![]);
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server
                .serve(server_io)
                .await?
                .waiting()
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        });
        let client = ().serve(client_io).await?;
        let tools = client.list_all_tools().await?;
        assert!(
            tools
                .iter()
                .all(|t| t.name != wats::mcp::GUARDIAN_PUBLISH_TOOL)
        );

        let now = wats::node::now_secs();
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let forged = wats::halt::sign(wats::halt::halt_draft("no", now)?, &stranger, now, Some(0))?;
        let refused = call(
            &client,
            wats::mcp::GUARDIAN_PUBLISH_TOOL,
            json!({"event": forged}),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");

        let halt = wats::halt::sign(
            wats::halt::halt_draft("runaway", now)?,
            &guardian,
            now,
            Some(0),
        )?;
        let published = structured(
            &call(
                &client,
                wats::mcp::GUARDIAN_PUBLISH_TOOL,
                json!({"event": halt}),
            )
            .await,
        );
        assert_eq!(published["id"], halt.id.as_str());

        let status = structured(&call(&client, "status", json!({})).await);
        assert_eq!(status["halt"]["accept"], true, "{status}");
        assert_eq!(
            status["halt"]["in_effect"]["id"],
            halt.id.as_str(),
            "{status}"
        );
        assert_eq!(status["halt"]["in_effect"]["reason"], "runaway");
        let search = call(&client, "search", json!({})).await;
        assert_eq!(search.is_error, Some(true));
        assert!(format!("{:?}", search.content).contains("halted by its guardians"));
        client.cancel().await?;
        node.shutdown().await?;
        Ok(())
    })
}

/// An ask with no answer past its deadline shows up for its author, in
/// search, wait and status, with the clarify waiting for them; an answer
/// from someone else ends it (ADR 0018 §4).
#[test]
fn unanswered_asks_surface_for_their_author() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let asker = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let helper = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("helper")?).await?;
        let past = wats::node::now_secs() - 1;
        let ask = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Build this Rust parser", "text": "It must compile.", "kind": "ask",
                   "data": {"deadline": past}}),
            )
            .await,
        );
        let ask_id = ask["id"].as_str().unwrap();
        // A fresh ask without a deadline is not listed (30 minutes by default).
        structured(
            &call(
                &asker,
                "post",
                json!({"title": "Fresh question", "text": "Anyone?", "kind": "ask"}),
            )
            .await,
        );

        let found = structured(&call(&asker, "search", json!({})).await);
        let unanswered = found["unanswered"].as_array().expect("unanswered");
        assert_eq!(unanswered.len(), 1, "{found}");
        assert_eq!(unanswered[0]["id"], ask_id);
        assert_eq!(
            unanswered[0]["notice"],
            "your ask 'Build this Rust parser': 0 answers after 0 min"
        );

        let clarify = structured(
        &call(
            &helper,
            "reply",
            json!({"post_id": ask_id, "text": "I can write it, not compile it here.",
                   "kind": "clarify",
                   "data": {"options": ["untested code", "no thanks"], "default": "untested code"}}),
        )
        .await,
    );
        let waited = structured(&call(&asker, "wait", json!({"timeout": 1})).await);
        assert_eq!(
            waited["unanswered"][0]["notice"],
            "your ask 'Build this Rust parser': 0 answers after 0 min · 1 clarify offering untested code"
        );
        assert_eq!(waited["unanswered"][0]["clarifies"][0]["id"], clarify["id"]);
        let status = structured(&call(&asker, "status", json!({})).await);
        assert_eq!(status["unanswered"][0]["id"], ask_id, "{status}");
        // How events arrived in the last hour (0.4.12).
        assert_eq!(
            status["gossip"]["last_hour"]["by_anti_entropy"], 0,
            "{status}"
        );
        assert!(status["gossip"]["resets"].as_array().is_some(), "{status}");
        // Not the helper's ask.
        let theirs = structured(&call(&helper, "status", json!({})).await);
        assert!(theirs.get("unanswered").is_none(), "{theirs}");

        structured(
            &call(
                &helper,
                "reply",
                json!({"post_id": ask_id, "text": "Here is untested code.", "kind": "answer"}),
            )
            .await,
        );
        let answered = structured(&call(&asker, "search", json!({})).await);
        assert!(answered.get("unanswered").is_none(), "{answered}");
        node.shutdown().await?;
        Ok(())
    })
}

/// `browse` maps the node: tags, threads by activity with open asks, and a
/// thread's outline.
#[test]
fn browse_maps_tags_threads_and_outlines() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let asker = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let helper = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("helper")?).await?;
        let ask = structured(
        &call(
            &asker,
            "post",
            json!({"title": "Rust on musl", "text": "It fails to link.", "kind": "ask", "tags": ["cap:rust"]}),
        )
        .await,
    );
        let ask_id = ask["id"].as_str().unwrap();
        structured(
            &call(
                &helper,
                "reply",
                json!({"post_id": ask_id, "text": "Install musl-tools.", "kind": "answer"}),
            )
            .await,
        );
        let map = structured(&call(&asker, "browse", json!({})).await);
        assert_eq!(map["tags"][0]["tag"], "cap:rust", "{map}");
        assert_eq!(map["threads"][0]["id"], ask_id);
        assert_eq!(map["threads"][0]["answers"], 1);
        assert_eq!(map["threads"][0]["open"], false);
        let open =
            structured(&call(&asker, "browse", json!({"kind": "ask", "tag": "cap:pdf"})).await);
        assert_eq!(open["threads"], json!([]));
        let tree = structured(&call(&asker, "browse", json!({"thread": ask_id})).await);
        assert_eq!(tree["posts"].as_array().unwrap().len(), 2);
        assert_eq!(tree["posts"][1]["depth"], 1);
        // A local profile is named; a unique prefix opens the thread.
        assert_eq!(tree["posts"][1]["name"], "helper", "{tree}");
        let read = structured(&call(&asker, "read", json!({"id": ask_id})).await);
        assert_eq!(read["post"]["author_local"], "default", "{read}");
        assert_eq!(read["thread"][1]["author_local"], "helper", "{read}");
        let found = structured(&call(&asker, "search", json!({"text": "musl"})).await);
        assert!(
            found["posts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["author_local"].is_string()),
            "{found}"
        );
        let short = structured(&call(&asker, "browse", json!({"thread": &ask_id[..12]})).await);
        assert_eq!(short["posts"], tree["posts"]);
        let bad = call(&asker, "browse", json!({"kind": "nonsense"})).await;
        assert_eq!(bad.is_error, Some(true));
        node.shutdown().await?;

        // `wats read <prefix>`: the post in full, its thread, from the store.
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path().join("home"))
            .env("WATS_RUN_DIR", dir.path().join("run"))
            .arg("--dir")
            .arg(dir.path().join("n"))
            .args(["read", &ask_id[..10]])
            .output()?;
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{out:?}");
        assert!(said.starts_with("ask · Rust on musl\n"), "{said}");
        assert!(said.contains(ask_id), "{said}");
        assert!(said.contains("It fails to link."), "{said}");
        assert!(said.contains("thread (1):"), "{said}");
        assert!(said.contains("not verified"), "{said}");
        let bad = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .env("WATS_HERMETIC", "1")
            .env("HOME", dir.path().join("home"))
            .arg("--dir")
            .arg(dir.path().join("n"))
            .args(["read", "zz"])
            .output()?;
        assert!(!bad.status.success());
        Ok(())
    })
}

/// `wats drop` through the running node's socket (ADR 0020): `read` then
/// shows the post's header and `dropped`, never the body; `search` and
/// `browse` no longer find it; its file is untagged.
#[test]
fn a_dropped_post_reads_as_its_header() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("n");
        let node = loopback_node(&data, None).await?;
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared)?;
        std::fs::write(shared.join("key.txt"), "s3cr3t")?;
        let asker = client_for(WatsMcp::new(node.clone(), vec![shared.clone()])).await?;
        let helper = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("helper")?).await?;
        let ask = structured(
            &call(
                &asker,
                "post",
                json!({"title": "My leaked key", "text": "Here it is: s3cr3t", "kind": "ask",
                   "tags": ["cap:secrets"],
                   "blobs": [{"path": shared.join("key.txt").to_str().unwrap()}]}),
            )
            .await,
        );
        let ask_id = ask["id"].as_str().unwrap().to_string();
        let answer = structured(
            &call(
                &helper,
                "reply",
                json!({"post_id": ask_id, "text": "Rotate it now.", "kind": "answer"}),
            )
            .await,
        );
        let answer_id = answer["id"].as_str().unwrap().to_string();
        let hash = node.store().get(&ask_id)?.unwrap().event.blobs[0]
            .hash
            .clone();
        assert!(node.blob_pinned(&hash).await?);

        // No MCP tool drops a body.
        let tools = asker.list_all_tools().await?;
        assert!(tools.iter().all(|t| !t.name.contains("drop")));

        let _sessions = wats::attach::serve(Arc::clone(&node))?;
        let outcome = wats::attach::control(
            &data,
            wats::attach::Control::Drop {
                id: ask_id.clone(),
                detail: Some("a secret".into()),
            },
        )
        .await?
        .expect("the node answers");
        assert_eq!(outcome["body_dropped"], true, "{outcome}");
        assert_eq!(outcome["freed_blobs"], json!([hash]));
        assert!(!node.blob_pinned(&hash).await?);
        let bad = wats::attach::control(
            &data,
            wats::attach::Control::Drop {
                id: "not an id".into(),
                detail: None,
            },
        )
        .await;
        assert!(bad.is_err());

        let read = structured(&call(&asker, "read", json!({"id": ask_id})).await);
        let post = &read["post"];
        assert_eq!(post["id"], ask_id.as_str(), "{read}");
        assert_eq!(post["kind"], "ask");
        assert_eq!(post["author"], node.author_id());
        assert!(post["ts"].as_u64().is_some());
        assert_eq!(post["dropped"]["reason"], "own");
        assert!(post["dropped"]["at"].as_u64().is_some());
        assert!(
            post["dropped"].get("note").is_none(),
            "the detail stays local"
        );
        for gone in ["text", "title", "tags", "data", "blobs"] {
            assert!(post.get(gone).is_none(), "{gone}: {read}");
        }
        assert!(!read.to_string().contains("s3cr3t"), "{read}");
        assert_eq!(read["thread_root"], ask_id.as_str());
        assert_eq!(read["thread"][0]["id"], answer_id.as_str());

        // The reply's thread lists the dropped root by its header.
        let reply = structured(&call(&asker, "read", json!({"id": answer_id})).await);
        assert_eq!(reply["unreadable"][0]["id"], ask_id.as_str(), "{reply}");
        assert_eq!(reply["unreadable"][0]["dropped"]["reason"], "own");
        assert_eq!(reply["post"]["parent"], ask_id.as_str());

        let found = structured(&call(&asker, "search", json!({"text": "leaked key"})).await);
        assert!(!found.to_string().contains(&ask_id), "{found}");
        let tree = structured(&call(&asker, "browse", json!({"thread": ask_id})).await);
        assert_eq!(tree["posts"][0]["title"], "(body dropped: own)", "{tree}");
        let map = structured(&call(&asker, "browse", json!({})).await);
        assert_eq!(map["tags"], json!([]), "{map}");
        node.shutdown().await?;
        Ok(())
    })
}

fn error_text(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "{result:?}");
    serde_json::to_string(&result.content).unwrap()
}

async fn post_note(client: &Client, text: &str) -> String {
    let posted = structured(
        &call(
            client,
            "post",
            json!({"title": "a post", "text": text, "kind": "note"}),
        )
        .await,
    );
    posted["id"].as_str().unwrap().to_string()
}

/// The MCP `retract` tool (ADR 0020 §5.1): own posts only; a human
/// session has no limit and leaves no audit copy; `post` cannot publish a
/// retraction behind its back.
#[test]
fn retract_accepts_only_the_sessions_own_posts() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("n");
        let node = loopback_node(&data, None).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let helper = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("helper")?).await?;
        let tools = me.list_all_tools().await?;
        assert!(tools.iter().any(|t| t.name == "retract"));
        let mine = post_note(&me, "my token is s3cr3t").await;
        let theirs = post_note(&helper, "the helper's post").await;

        let refused = call(&me, "retract", json!({"ids": [theirs]})).await;
        let text = error_text(&refused);
        assert!(text.contains("is not yours"), "{text}");
        assert!(node.store().get(&theirs)?.is_some());
        // Not even mixed with one of mine: nothing is retracted.
        let refused = call(&me, "retract", json!({"ids": [mine, theirs]})).await;
        error_text(&refused);
        assert!(node.store().get(&mine)?.is_some());

        let done = structured(
            &call(
                &me,
                "retract",
                json!({"ids": [mine], "reason": "posted a secret by mistake"}),
            )
            .await,
        );
        assert_eq!(done["retracted"], json!([mine]), "{done}");
        let read = structured(&call(&me, "read", json!({"id": mine})).await);
        assert_eq!(read["post"]["dropped"]["reason"], "retraction", "{read}");
        assert!(!read.to_string().contains("s3cr3t"));
        // A human session: no log, no audit copy, nothing in status.
        assert!(wats::retract::agent_log(&data)?.is_empty());
        assert!(wats::retract::audit_copy(&data, &mine)?.is_none());
        let status = structured(&call(&me, "status", json!({})).await);
        assert!(status.get("agent_retractions").is_none(), "{status}");

        // The retraction kind only goes through `retract`.
        let sneaky = call(
            &helper,
            "post",
            json!({"title": "Retraction", "text": "x", "kind": wats::retract::KIND,
               "refs": [theirs]}),
        )
        .await;
        assert!(error_text(&sneaky).contains("retract"));
        assert!(node.store().get(&theirs)?.is_some());
        node.shutdown().await?;
        Ok(())
    })
}

/// Agent profiles: at most 3 retracted posts per 24 hours, still counted
/// after a restart; each retracted post kept as a local audit copy; shown
/// to the owner in `status`.
#[test]
fn agents_retract_under_a_rate_limit_with_an_audit_copy() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("n");
        let start = || async {
            let mut config = NodeConfig::new(&data);
            config.mode = NetworkMode::Loopback;
            config.pow_bits = Some(4);
            config.min_pow_bits = Some(4);
            config.runners = vec!["claude as alice-agent".into()];
            Node::start(config).await
        };
        let node = start().await?;
        let agent =
            client_for(WatsMcp::new(node.clone(), vec![]).with_profile("alice-agent")?).await?;
        let owner = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let mut ids = Vec::new();
        for n in 0..4 {
            ids.push(post_note(&agent, &format!("body {n}")).await);
        }
        structured(&call(&agent, "retract", json!({"ids": [ids[0], ids[1]]})).await);
        structured(
            &call(
                &agent,
                "retract",
                json!({"ids": [ids[2]], "reason": "leaked a path"}),
            )
            .await,
        );
        for id in &ids[..3] {
            assert!(node.store().get(id)?.is_none(), "dropped");
            let copy = wats::retract::audit_copy(&data, id)?.expect("audit copy");
            assert_eq!(copy["id"], id.as_str());
            assert!(
                copy["text"].as_str().unwrap().starts_with("body "),
                "{copy}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(wats::retract::audit_path(&data, id))?
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
        let refused = call(&agent, "retract", json!({"ids": [ids[3]]})).await;
        assert!(error_text(&refused).contains("rate limit"));
        assert!(node.store().get(&ids[3])?.is_some());
        assert!(wats::retract::audit_copy(&data, &ids[3])?.is_none());

        // The owner sees them; the agent's own status does not list them.
        let status = structured(&call(&owner, "status", json!({})).await);
        let shown = &status["agent_retractions"];
        assert_eq!(shown["count"], 3, "{status}");
        assert_eq!(shown["profiles"], json!(["alice-agent"]));
        assert_eq!(shown["latest"]["reason"], "leaked a path");
        for id in &ids[..3] {
            assert!(shown["ids"].as_array().unwrap().contains(&json!(id)));
        }
        let agent_status = structured(&call(&agent, "status", json!({})).await);
        assert!(agent_status.get("agent_retractions").is_none());
        // The audit copies are never served: `read` shows the header only.
        let read = structured(&call(&owner, "read", json!({"id": ids[0]})).await);
        assert!(!read.to_string().contains("body 0"), "{read}");

        // A restart does not reset the count.
        agent.cancel().await?;
        owner.cancel().await?;
        node.shutdown().await?;
        drop(node);
        let node = start().await?;
        let agent =
            client_for(WatsMcp::new(node.clone(), vec![]).with_profile("alice-agent")?).await?;
        let refused = call(&agent, "retract", json!({"ids": [ids[3]]})).await;
        assert!(error_text(&refused).contains("rate limit"));
        // Not the owner's post either; the owner retracts it as the agent's
        // profile (`wats retract --profile`), with no limit.
        let owner = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let not_mine = call(&owner, "retract", json!({"ids": [ids[3]]})).await;
        assert!(error_text(&not_mine).contains("is not yours"));
        let events = node
            .retract_as(&node.author_key("alice-agent")?, &ids[3..], None)
            .await?;
        assert_eq!(events.len(), 1);
        assert!(node.store().get(&ids[3])?.is_none());
        node.shutdown().await?;
        Ok(())
    })
}

/// An ask posted from OpenClaw (`WATS_MCP_CLIENT=openclaw`) is remembered
/// for its chat (ADR 0018 §7); one from another host as a human's ask
/// (`notify.desktop`); a note is not.
#[test]
fn asks_from_openclaw_are_remembered_for_notifications() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let openclaw =
            client_for(WatsMcp::new(node.clone(), vec![]).with_client(Some("openclaw".into())))
                .await?;
        let other = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let post = |title: &str| json!({"title": title, "text": "Please.", "kind": "ask"});
        let from_openclaw = structured(&call(&openclaw, "post", post("Draw a bee")).await);
        let from_other = structured(&call(&other, "post", post("Sum these")).await);
        let note = structured(
            &call(
                &openclaw,
                "post",
                json!({"title": "A note", "text": "FYI", "kind": "note"}),
            )
            .await,
        );
        let notify = |v: &Value| node.store().ask_notify(v["id"].as_str().unwrap()).unwrap();
        assert_eq!(notify(&from_openclaw).as_deref(), Some("openclaw"));
        // Another host's human ask is remembered too, for the desktop
        // notification (notify.desktop).
        assert_eq!(
            notify(&from_other).as_deref(),
            Some(wats::mcp::HUMAN_CLIENT)
        );
        assert_eq!(notify(&note), None, "only asks");
        node.shutdown().await?;
        Ok(())
    })
}

/// A reply by another node's key, stored as if it had arrived.
fn remote(node: &Node, seed: u8, parent: &str, kind: &str, text: &str, data: Value) -> String {
    let parent = node.store().get(parent).unwrap().unwrap().event;
    let event = wats::event::Draft {
        net: parent.net.clone(),
        thread: Some(parent.thread_root().to_string()),
        parent: Some(parent.id.clone()),
        kind: Some(kind.parse().unwrap()),
        text: text.into(),
        data: data.as_object().unwrap().clone(),
        ..Default::default()
    }
    .sign(
        &ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
        wats::node::now_secs(),
        0,
    )
    .unwrap();
    node.store().insert(&event, wats::node::now_secs()).unwrap();
    event.id
}

/// ADR 0019 v1a: statuses and counts in search, wait and read, search
/// order within comparable relevance, `min_status`, self-signals counting
/// zero, and the next-step hints.
#[test]
fn statuses_order_search_filter_wait_and_hint_the_next_step() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let my_agent =
            client_for(WatsMcp::new(node.clone(), vec![]).with_profile("my-agent")?).await?;

        // An ask that cites nothing gets a hint; one with refs does not.
        let ask = structured(
            &call(
                &me,
                "post",
                json!({"title": "Rust on musl fails to link", "text": "How?", "kind": "ask"}),
            )
            .await,
        );
        assert!(ask["next_step"].as_str().unwrap().contains("refs"), "{ask}");
        let ask = ask["id"].as_str().unwrap().to_string();
        let cited = structured(
            &call(
                &me,
                "post",
                json!({"title": "Again", "text": "Still?", "kind": "ask", "refs": [ask]}),
            )
            .await,
        );
        assert!(cited.get("next_step").is_none(), "{cited}");

        // Three answers alike, by other nodes' keys, and one of mine.
        let text = "Install musl-tools to link on musl.";
        let a1 = remote(&node, 11, &ask, "answer", text, json!({}));
        let a2 = remote(&node, 12, &ask, "answer", text, json!({}));
        let a3 = remote(&node, 13, &ask, "answer", text, json!({}));
        let mine = structured(
            &call(
                &me,
                "reply",
                json!({"post_id": ask, "text": "Use the gnu target instead."}),
            )
            .await,
        )["id"]
            .as_str()
            .unwrap()
            .to_string();
        // a1 is contradicted with evidence; I used a2; my other profile uses
        // my own answer, which counts zero.
        remote(
            &node,
            14,
            &a1,
            "react",
            "fails",
            json!({"value": -1, "evidence": {"type": "test", "detail": "still fails on alpine 3.20"}}),
        );
        structured(
            &call(
                &me,
                "reply",
                json!({"post_id": a2, "kind": "react", "text": "worked", "data": {"used": true}}),
            )
            .await,
        );
        structured(
            &call(
                &my_agent,
                "reply",
                json!({"post_id": mine, "kind": "react", "text": "worked",
                   "data": {"used": true, "value": 1, "evidence": "ran it"}}),
            )
            .await,
        );

        // Confirmed first, contested last but shown, with why; a hint to read.
        let found = structured(
            &call(
                &me,
                "search",
                json!({"text": "musl-tools", "kinds": ["answer"]}),
            )
            .await,
        );
        let posts = found["posts"].as_array().unwrap();
        let ids: Vec<&str> = posts.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, [a2.as_str(), a3.as_str(), a1.as_str()], "{found}");
        let statuses: Vec<&str> = posts
            .iter()
            .map(|p| p["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["confirmed", "unverified", "contested"]);
        assert_eq!(posts[0]["counts"]["uses"], 1);
        assert_eq!(posts[2]["counts"]["contradicts"], 1);
        assert_eq!(
            posts[2]["why"],
            "contradicted by 1 key: still fails on alpine 3.20"
        );
        assert!(posts[1].get("why").is_none());
        assert!(found["next_step"].as_str().unwrap().contains("data.used"));

        // wait stays chronological, and filters on status.
        let wait = |extra: Value| {
            let mut args = json!({"since": 0, "timeout": 1, "kinds": ["answer"]});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            args
        };
        let ids_of = |v: &Value| -> Vec<String> {
            v["posts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["id"].as_str().unwrap().to_string())
                .collect()
        };
        let all = structured(&call(&me, "wait", wait(json!({}))).await);
        assert_eq!(
            ids_of(&all),
            [a1.as_str(), a2.as_str(), a3.as_str()],
            "{all}"
        );
        let confirmed =
            structured(&call(&me, "wait", wait(json!({"min_status": "confirmed"}))).await);
        assert_eq!(ids_of(&confirmed), [a2.as_str()], "{confirmed}");
        let unverified =
            structured(&call(&me, "wait", wait(json!({"min_status": "unverified"}))).await);
        assert_eq!(
            ids_of(&unverified),
            [a2.as_str(), a3.as_str()],
            "{unverified}"
        );
        // Nothing more matches: it times out past what it left out.
        let cursor = unverified["cursor"].as_i64().unwrap();
        let none = structured(
            &call(
                &me,
                "wait",
                wait(json!({"since": cursor, "min_status": "confirmed"})),
            )
            .await,
        );
        assert_eq!(none["timed_out"], true, "{none}");
        let bad = call(&me, "wait", json!({"timeout": 1, "min_status": "great"})).await;
        assert_eq!(bad.is_error, Some(true));

        // read shows the status, the counts and a reminder; not on my posts.
        let read = structured(&call(&me, "read", json!({"id": a2})).await);
        assert_eq!(read["post"]["status"], "confirmed", "{read}");
        assert_eq!(read["post"]["counts"]["uses"], 1);
        assert!(read["next_step"].as_str().unwrap().contains("data.used"));
        let in_thread: Vec<&str> = read["thread"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["status"].as_str())
            .collect();
        assert!(in_thread.contains(&"contested"), "{read}");
        let own = structured(&call(&me, "read", json!({"id": mine})).await);
        assert_eq!(own["post"]["status"], "unverified", "{own}");
        assert!(own["post"].get("counts").is_none());
        assert!(own.get("next_step").is_none());

        node.shutdown().await?;
        Ok(())
    })
}

/// ADR 0019 v1b: an author whose standing contradictions outweigh the rest
/// (contradicted with evidence by this node, which weighs) is left out of
/// `search` and `wait` unless `include_low`; `read` still opens it. Then
/// `wats reputation` shows the numbers behind it, for the human.
#[test]
fn low_authors_are_hidden_unless_asked_and_humans_see_the_numbers() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("n");
        let mut config = NodeConfig::new(&data);
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(4);
        config.min_pow_bits = Some(4);
        // Rebuild the reputation table whenever the store changed.
        config.reputation.ttl = Duration::ZERO;
        let node = Node::start(config).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;

        let ask = structured(
            &call(
                &me,
                "post",
                json!({"title": "Rust on musl fails to link", "text": "How?", "kind": "ask"}),
            )
            .await,
        )["id"]
            .as_str()
            .unwrap()
            .to_string();
        let text = "Install musl-tools to link on musl.";
        let wrong = remote(&node, 11, &ask, "answer", text, json!({}));
        let right = remote(&node, 12, &ask, "answer", text, json!({}));
        let ids_of = |v: &Value| -> Vec<String> {
            v["posts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["id"].as_str().unwrap().to_string())
                .collect()
        };
        // A fresh network hides nobody.
        let search = json!({"text": "musl-tools", "kinds": ["answer"]});
        let found = structured(&call(&me, "search", search.clone()).await);
        assert_eq!(ids_of(&found).len(), 2, "{found}");
        assert!(found.get("hidden_low").is_none());

        // This node contradicts 11 with evidence: 11's score goes negative.
        structured(
            &call(
                &me,
                "reply",
                json!({"post_id": wrong, "kind": "react", "text": "fails",
                   "data": {"value": -1, "evidence": "still fails on alpine 3.20"}}),
            )
            .await,
        );
        let found = structured(&call(&me, "search", search.clone()).await);
        assert_eq!(ids_of(&found), std::slice::from_ref(&right), "{found}");
        assert_eq!(found["hidden_low"], 1);
        let mut low = search.clone();
        low["include_low"] = json!(true);
        let found = structured(&call(&me, "search", low).await);
        assert_eq!(ids_of(&found), [right.clone(), wrong.clone()], "{found}");
        assert_eq!(found["posts"][1]["status"], "contested");
        // No weighted number reaches the agent: only the readable tally
        // `score` (ADR 0055 §2).
        let shown = found.to_string();
        for word in ["trust", "weight", "floor", "author_score", "\"score\":0."] {
            assert!(!shown.contains(word), "{word} in {shown}");
        }

        let wait = |extra: Value| {
            let mut args = json!({"since": 0, "timeout": 1, "kinds": ["answer"]});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            args
        };
        let waited = structured(&call(&me, "wait", wait(json!({}))).await);
        assert_eq!(ids_of(&waited), std::slice::from_ref(&right), "{waited}");
        let waited = structured(&call(&me, "wait", wait(json!({"include_low": true}))).await);
        assert_eq!(ids_of(&waited), [wrong.clone(), right.clone()], "{waited}");
        // Asked by id, it is shown.
        let read = structured(&call(&me, "read", json!({"id": wrong})).await);
        assert_eq!(read["post"]["status"], "contested", "{read}");

        let mine = node.author_id();
        let theirs = wats::identity::author_id(&ed25519_dalek::SigningKey::from_bytes(&[11; 32]));
        me.cancel().await?;
        node.shutdown().await?;

        // `wats reputation`, hermetic, reading the store the node left.
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home)?;
        let reputation = |extra: &[&str]| {
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
                .env("WATS_HERMETIC", "1")
                .env("HOME", &home)
                .env_remove("WATS_PROFILE")
                .args(["--infra", "none", "--dir"])
                .arg(&data)
                .arg("reputation")
                .args(extra)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let lines = reputation(&[]);
        assert!(lines.starts_with("public: "), "{lines}");
        let line_of = |author: &str| {
            lines
                .lines()
                .find(|l| l.trim_start().starts_with(author))
                .unwrap_or_else(|| panic!("{author} in {lines}"))
                .to_string()
        };
        let my_line = line_of(&mine);
        assert!(my_line.contains("1.000"), "trust: {my_line}");
        assert!(my_line.contains("yours"), "{my_line}");
        let their_line = line_of(&theirs);
        assert!(their_line.contains("-2.000"), "score: {their_line}");

        let json: Value =
            serde_json::from_str(&reputation(&["--json", "--author", &theirs[..16]]))?;
        let authors = json[0]["authors"].as_array().unwrap();
        assert_eq!(authors.len(), 1, "{json}");
        assert_eq!(authors[0]["author"], theirs);
        assert!(authors[0]["score"].as_f64().unwrap() < 0.0, "{json}");
        assert_eq!(authors[0]["contradicted"], 1);
        assert_eq!(authors[0]["trust"], 0.0);
        Ok(())
    })
}

/// ADR 0019 step 3: a digest through the MCP `digest` tool, its checks,
/// search by its title, and its promotion to a knowledge entry once two
/// independent keys that weigh back it: `search` shows it first, marked,
/// and `read` of its thread shows it as `knowledge`. A received digest
/// without refs stays valid and is never promoted.
#[test]
fn digests_become_knowledge_entries() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let mut config = NodeConfig::new(dir.path().join("n"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(4);
        config.min_pow_bits = Some(4);
        // Rebuild the reputation table whenever the store changed.
        config.reputation.ttl = Duration::ZERO;
        let node = Node::start(config).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let id_of = |v: Value| v["id"].as_str().unwrap().to_string();

        // This node used answers by 11 and 14: their keys weigh.
        let warmup = id_of(structured(
            &call(
                &me,
                "post",
                json!({"title": "Which linker for static builds?", "text": "?", "kind": "ask"}),
            )
            .await,
        ));
        for seed in [11, 14] {
            let answer = remote(&node, seed, &warmup, "answer", "use lld", json!({}));
            structured(
            &call(
                &me,
                "reply",
                json!({"post_id": answer, "kind": "react", "text": "worked", "data": {"used": true}}),
            )
            .await,
        );
        }

        let ask = id_of(structured(
            &call(
                &me,
                "post",
                json!({"title": "Rust on musl fails to link", "text": "How?", "kind": "ask"}),
            )
            .await,
        ));
        let answer = remote(
            &node,
            12,
            &ask,
            "answer",
            "Install musl-tools to link on musl.",
            json!({}),
        );

        // The tool's checks: sources required and known; digests only
        // through `digest`.
        let digest_args = |refs: Value| {
            json!({
                "title": "Rust: linking on musl needs musl-tools",
                "text": "On musl targets, install musl-tools before cargo build.",
                "refs": refs,
                "reply_to": ask,
                "tags": ["cap:rust"],
            })
        };
        for refs in [json!([]), json!([ask]), json!(["ab".repeat(32)])] {
            let refused = call(&me, "digest", digest_args(refs)).await;
            let text = error_text(&refused);
            assert!(
                text.contains("refs") || text.contains("not known"),
                "{text}"
            );
        }
        let refused = call(
            &me,
            "post",
            json!({"title": "t", "text": "x", "kind": "x.wearetheswarm.digest", "refs": [answer]}),
        )
        .await;
        assert!(error_text(&refused).contains("`digest` tool"));

        // Round trip: posted, read, found by a word of its title only.
        let digest = id_of(structured(
            &call(&me, "digest", digest_args(json!([answer]))).await,
        ));
        let read = structured(&call(&me, "read", json!({"id": digest})).await);
        let post = &read["post"];
        assert_eq!(post["kind"], "note", "{read}");
        assert_eq!(post["ext_kind"], "x.wearetheswarm.digest");
        assert_eq!(post["refs"], json!([answer]));
        assert_eq!(post["parent"], json!(ask));
        assert_eq!(post["status"], "unverified");
        assert!(post.get("knowledge").is_none() && read.get("knowledge").is_none());
        let found = structured(&call(&me, "search", json!({"text": "linking"})).await);
        assert_eq!(found["posts"][0]["id"], json!(digest), "{found}");
        // "digest" among the kinds: the digests.
        let digests = structured(&call(&me, "search", json!({"kinds": ["digest"]})).await);
        assert_eq!(digests["posts"][0]["id"], json!(digest), "{digests}");
        // "knowledge" among the kinds, as in browse: entries only, no error.
        let entries = structured(&call(&me, "search", json!({"kinds": ["knowledge"]})).await);
        assert!(
            entries["posts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["knowledge"] == true),
            "{entries}"
        );
        assert!(found["posts"][0].get("knowledge").is_none());

        // 11 confirms it with evidence: one key is not enough.
        remote(
            &node,
            11,
            &digest,
            "react",
            "checked",
            json!({"value": 1, "evidence": {"type": "test", "detail": "cargo build --target x86_64-unknown-linux-musl: ok"}}),
        );
        let read = structured(&call(&me, "read", json!({"id": ask})).await);
        assert!(read.get("knowledge").is_none(), "{read}");
        // 14 used it: a knowledge entry.
        remote(&node, 14, &digest, "react", "worked", json!({"used": true}));

        // `search` brings it first among comparable matches, marked, even
        // when only its thread matched; never against the filter.
        let found = structured(&call(&me, "search", json!({"text": "fails to link"})).await);
        let posts = found["posts"].as_array().unwrap();
        assert_eq!(posts[0]["id"], json!(digest), "{found}");
        assert_eq!(posts[0]["knowledge"], true);
        assert_eq!(posts[0]["sources"], json!([answer]));
        assert_eq!(posts[0]["status"], "confirmed");
        assert_eq!(posts[0]["title"], "Rust: linking on musl needs musl-tools");
        assert_eq!(posts[1]["id"], json!(ask));
        assert!(posts[1].get("knowledge").is_none());
        let asks = structured(
            &call(
                &me,
                "search",
                json!({"text": "fails to link", "kinds": ["ask"]}),
            )
            .await,
        );
        assert_eq!(asks["posts"].as_array().unwrap().len(), 1, "{asks}");
        let found = structured(&call(&me, "search", json!({"text": "musl-tools"})).await);
        assert_eq!(found["posts"][0]["id"], json!(digest), "{found}");
        assert_eq!(found["posts"][0]["knowledge"], true);

        // `read` of its thread shows it first; the digest itself is marked.
        for id in [&ask, &answer] {
            let read = structured(&call(&me, "read", json!({"id": id})).await);
            assert_eq!(read["knowledge"]["id"], json!(digest), "{read}");
            assert_eq!(read["knowledge"]["knowledge"], true);
            assert_eq!(read["knowledge"]["sources"], json!([answer]));
            assert_eq!(read["knowledge"]["status"], "confirmed");
            assert_eq!(read["post"]["id"], json!(id), "the post is unchanged");
            assert!(read["post"].get("knowledge").is_none());
        }
        let read = structured(&call(&me, "read", json!({"id": digest})).await);
        assert_eq!(read["post"]["knowledge"], true, "{read}");
        assert_eq!(read["post"]["sources"], json!([answer]));
        assert!(read.get("knowledge").is_none());

        // A received digest without refs: valid, confirmed, never an entry.
        let bare = remote(
            &node,
            13,
            &ask,
            "x.wearetheswarm.digest",
            "musl needs nothing",
            json!({}),
        );
        for seed in [11, 14] {
            remote(&node, seed, &bare, "react", "ok", json!({"used": true}));
        }
        let read = structured(&call(&me, "read", json!({"id": bare})).await);
        assert_eq!(read["post"]["status"], "confirmed", "{read}");
        assert!(read["post"].get("knowledge").is_none());
        assert_eq!(read["knowledge"]["id"], json!(digest));

        // `supersedes`: only one's own digests.
        let mut replacing = digest_args(json!([answer]));
        replacing["supersedes"] = json!(bare);
        let refused = call(&me, "digest", replacing.clone()).await;
        assert!(error_text(&refused).contains("another key's digest"));
        replacing["supersedes"] = json!(digest);
        let newer = id_of(structured(&call(&me, "digest", replacing).await));
        let read = structured(&call(&me, "read", json!({"id": newer})).await);
        assert_eq!(read["post"]["data"]["supersedes"], json!(digest), "{read}");
        // Not promoted yet: the older one stays the entry.
        assert_eq!(read["knowledge"]["id"], json!(digest));

        // No weighted number reaches the agent: only the readable tally
        // `score` (ADR 0055 §2).
        let shown = format!(
            "{} {}",
            structured(&call(&me, "search", json!({"text": "musl"})).await),
            structured(&call(&me, "read", json!({"id": ask})).await)
        );
        for word in ["trust", "weight", "floor", "author_score", "\"score\":0."] {
            assert!(!shown.contains(word), "{word} in {shown}");
        }

        node.shutdown().await?;
        Ok(())
    })
}

/// ADR 0022 for sessions: `post` writes a full author id in a `to:` tag
/// in base32 and says what nodes read differently; a `claim` that loses the tie-break
/// says so; `read` of an ask shows its delivery state.
#[test]
fn sessions_see_how_an_ask_wants_to_be_served() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let asker = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let worker = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("worker")?).await?;
        let instructions = asker
            .peer_info()
            .unwrap()
            .instructions
            .clone()
            .unwrap_or_default();
        assert!(
            instructions.contains("post a `claim` first"),
            "{instructions}"
        );

        let to = wats::identity::author_id(&ed25519_dalek::SigningKey::from_bytes(&[7; 32]));
        let posted = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Compile this", "text": "One worker, please.", "kind": "ask",
                   "tags": [format!("to:{to}")], "data": {"mode": "one"}}),
            )
            .await,
        );
        // Hex in, base32 out: the full id, in a tag's 64 bytes.
        let tag = wats::delivery::to_tag(&to).unwrap();
        assert_eq!(posted["post"]["tags"][0], tag);
        assert!(posted.get("note").is_none(), "{posted}");
        // `wait` finds it by either form.
        for filter in [format!("to:{to}"), tag.clone()] {
            let waited = structured(
                &call(
                    &worker,
                    "wait",
                    json!({"since": 0, "tags": [filter], "timeout": 1}),
                )
                .await,
            );
            assert_eq!(waited["posts"][0]["id"], posted["id"], "{waited}");
        }
        // A prefix of an id addresses nobody: kept as is, with a note, and
        // the other notes stay.
        let prefix = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Prefix", "text": "?", "kind": "ask",
                   "tags": [format!("to:{}", &to[..32]), "cap:pdf"],
                   "data": {"mode": "split"}}),
            )
            .await,
        );
        assert_eq!(prefix["post"]["tags"][0], format!("to:{}", &to[..32]));
        let note = prefix["note"].as_str().unwrap();
        assert!(note.contains("addresses nobody"), "{prefix}");
        assert!(note.contains("read it as any"), "{prefix}");
        let odd = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Odd", "text": "?", "kind": "ask", "data": {"mode": "split"}}),
            )
            .await,
        );
        assert!(
            odd["note"].as_str().unwrap().contains("read it as any"),
            "{odd}"
        );

        // Another node's agent claimed it first.
        let ask = node
            .store()
            .get(posted["id"].as_str().unwrap())?
            .unwrap()
            .event;
        let older = wats::event::Draft {
            net: ask.net.clone(),
            thread: Some(ask.id.clone()),
            parent: Some(ask.id.clone()),
            kind: Some(wats::event::Kind::Claim),
            text: "mine".into(),
            ..Default::default()
        }
        .sign(
            &ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
            wats::node::now_secs() - 5,
            0,
        )
        .unwrap();
        node.store().insert(&older, wats::node::now_secs())?;
        let claimed = structured(
        &call(
            &worker,
            "reply",
            json!({"post_id": ask.id, "text": "On it.", "kind": "claim", "data": {"lease": 600}}),
        )
        .await,
    );
        assert_eq!(claimed["delivery"]["mode"], "one", "{claimed}");
        assert_eq!(claimed["delivery"]["expected"], "abstain", "{claimed}");
        assert_eq!(claimed["delivery"]["your_claim_lost"], claimed["id"]);
        assert!(
            claimed["warning"]
                .as_str()
                .unwrap()
                .contains("reply to your claim with kind release"),
            "{claimed}"
        );
        let read = structured(&call(&asker, "read", json!({"id": ask.id})).await);
        assert_eq!(read["delivery"]["mode"], "one", "{read}");
        assert_eq!(read["delivery"]["cap"], 1, "{read}");
        assert_eq!(
            read["delivery"]["claims"][0]["id"],
            older.id.as_str(),
            "{read}"
        );
        node.shutdown().await?;
        Ok(())
    })
}

/// A post signed by the key of `seed`, as if from another node: a root
/// when `parent` is `None`.
fn remote_post(
    node: &Node,
    seed: u8,
    parent: Option<&str>,
    kind: &str,
    title: &str,
    tags: &[&str],
    refs: &[&str],
) -> String {
    let parent = parent.map(|p| node.store().get(p).unwrap().unwrap().event);
    let mut refs: Vec<String> = refs.iter().map(|r| r.to_string()).collect();
    refs.sort();
    let event = wats::event::Draft {
        net: PUBLIC_NET.into(),
        thread: parent.as_ref().map(|p| p.thread_root().to_string()),
        parent: parent.as_ref().map(|p| p.id.clone()),
        kind: Some(kind.parse().unwrap()),
        title: Some(title.into()),
        text: title.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        refs,
        ..Default::default()
    }
    .sign(
        &ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
        wats::node::now_secs(),
        0,
    )
    .unwrap();
    node.store().insert(&event, wats::node::now_secs()).unwrap();
    event.id
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

fn ids_in(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect()
}

/// ADR 0024: tags as directories, each level with its children and their
/// counts, then its threads; facets (author in any form, period, kind)
/// combine with it; the outline shows citations; `wats browse` prints the
/// same map for a human.
#[test]
fn browse_walks_tag_directories_with_facets() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("n");
        let node = loopback_node(&data, None).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let id_of = |v: Value| v["id"].as_str().unwrap().to_string();
        let mine = id_of(structured(
            &call(
                &me,
                "post",
                json!({"title": "Async Rust on musl", "text": "?", "kind": "ask",
                   "tags": ["lang:rust:async", "cap:build"]}),
            )
            .await,
        ));
        let theirs = remote_post(&node, 11, None, "note", "Rust tips", &["lang:rust"], &[]);
        let go = remote_post(&node, 12, None, "ask", "Go modules", &["lang:go"], &[]);
        // A note elsewhere citing my ask.
        let citing = remote_post(&node, 13, None, "note", "Builds on it", &[], &[&mine]);
        let answer = remote_post(
            &node,
            12,
            Some(&mine),
            "answer",
            "Use musl-gcc",
            &[],
            &[&theirs],
        );
        let browse = |args: Value| {
            let me = &me;
            async move { structured(&call(me, "browse", args).await) }
        };

        // The roots, then one level down.
        let top = browse(json!({})).await;
        assert_eq!(top["dirs"][0]["tag"], "lang", "{top}");
        assert_eq!(top["dirs"][0]["threads"], 3);
        assert_eq!(top["dirs"][0]["children"], 2);
        assert_eq!(top["dirs"][1]["tag"], "cap");
        assert_eq!(top["dirs_more"], false);
        assert_eq!(top["more"], false);
        assert_eq!(top["tags"][0]["threads"], 1, "the busiest tags stay");
        let lang = browse(json!({"dir": "lang"})).await;
        assert_eq!(lang["dir"], "lang");
        let children: Vec<(String, u64)> = lang["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["tag"].as_str().unwrap().into(),
                    d["threads"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            children,
            [("lang:rust".into(), 2), ("lang:go".into(), 1)],
            "{lang}"
        );
        assert_eq!(lang["dirs"][0]["children"], 1);
        assert!(lang.get("tags").is_none());
        assert_eq!(ids_in(&lang["threads"]).len(), 3);
        let rust = browse(json!({"prefix": "lang:rust:"})).await;
        assert_eq!(rust["dirs"][0]["tag"], "lang:rust:async", "{rust}");
        assert_eq!(
            sorted(ids_in(&rust["threads"])),
            sorted(vec![mine.clone(), theirs.clone()])
        );

        // Facets: an author, in hex, base32 or its to: tag; a period; a kind.
        let twelve = wats::identity::author_id(&ed25519_dalek::SigningKey::from_bytes(&[12; 32]));
        let to = wats::delivery::to_tag(&twelve).unwrap();
        for author in [twelve.clone(), to.clone(), to[3..].to_uppercase()] {
            let by = browse(json!({"dir": "lang", "author": author})).await;
            assert_eq!(
                sorted(ids_in(&by["threads"])),
                sorted(vec![mine.clone(), go.clone()]),
                "{by}"
            );
            // Ties (same activity, same second here) by name.
            assert_eq!(by["dirs"][0]["tag"], "lang:go", "{by}");
            assert_eq!(by["dirs"][1]["tag"], "lang:rust");
            assert_eq!(by["dirs"][1]["threads"], 1);
        }
        let bad = call(&me, "browse", json!({"author": &twelve[..16]})).await;
        assert!(error_text(&bad).contains("not a full author id"));
        let recent = browse(json!({"since": "1h", "kind": "ask"})).await;
        assert_eq!(
            sorted(ids_in(&recent["threads"])),
            sorted(vec![mine.clone(), go.clone()])
        );
        let old = browse(json!({"until": "2020-01-01"})).await;
        assert_eq!(old["threads"], json!([]), "{old}");
        assert_eq!(old["dirs"], json!([]));
        let window = browse(json!({"since": 1_600_000_000, "until": "2100-01-01T00:00:00Z"})).await;
        assert_eq!(ids_in(&window["threads"]).len(), 4);
        let bad = call(&me, "browse", json!({"since": "yesterday"})).await;
        assert!(error_text(&bad).contains("invalid time"));

        // The outline: what each post cites, and who cites the root.
        let tree = browse(json!({"thread": mine})).await;
        assert_eq!(tree["cited_by"], 1, "{tree}");
        assert_eq!(tree["citing"][0]["id"], json!(citing));
        assert_eq!(tree["posts"][0]["cited_by"], 1);
        assert_eq!(tree["posts"][1]["id"], json!(answer));
        assert_eq!(tree["posts"][1]["refs"], json!([theirs]));
        let cited = browse(json!({"thread": theirs})).await;
        assert_eq!(cited["cited_by"], 1, "{cited}");
        assert_eq!(cited["citing"][0]["id"], json!(answer));

        me.cancel().await?;
        node.shutdown().await?;

        // `wats browse`, hermetic, reading the store the node left.
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home)?;
        let wats = |args: &[&str]| {
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
                .env("WATS_HERMETIC", "1")
                .env("HOME", &home)
                .env_remove("WATS_PROFILE")
                .args(["--infra", "none", "--dir"])
                .arg(&data)
                .arg("browse")
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let lines = wats(&["lang"]);
        assert!(lines.contains("dir: lang"), "{lines}");
        assert!(
            lines.contains("lang:rust  2 threads · 2 posts · 1 sub-tags"),
            "{lines}"
        );
        assert!(lines.contains("Async Rust on musl"), "{lines}");
        let outline = wats(&["--thread", &mine]);
        assert!(outline.contains("cited by 1 posts elsewhere"), "{outline}");
        assert!(outline.contains("Use musl-gcc · "), "{outline}");
        assert!(outline.contains("cites 1"), "{outline}");
        let json: Value =
            serde_json::from_str(&wats(&["--json", "--author", &to, "--kind", "ask"]))?;
        assert_eq!(
            sorted(ids_in(&json["threads"])),
            sorted(vec![mine.clone(), go.clone()]),
            "{json}"
        );
        Ok(())
    })
}

/// ADR 0024: cursors page through a snapshot, so a walk is exhaustive and
/// repeats nothing even when posts arrive between pages; `offset` still
/// works.
#[test]
fn browse_cursors_are_exhaustive_under_inserts() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let mut threads = Vec::new();
        for i in 0..8 {
            threads.push(remote_post(
                &node,
                11 + i,
                None,
                "note",
                &format!("Note {i}"),
                &["topic:x"],
                &[],
            ));
        }
        let page = |cursor: Option<&str>| {
            let me = &me;
            let mut args = json!({"dir": "topic", "limit": 3});
            if let Some(cursor) = cursor {
                args["cursor"] = json!(cursor);
            }
            async move { structured(&call(me, "browse", args).await) }
        };
        let first = page(None).await;
        assert_eq!(first["more"], true, "{first}");
        assert_eq!(first["next_offset"], 3);
        let mut seen = ids_in(&first["threads"]);
        let mut cursor = first["cursor"].as_str().map(str::to_string);
        let mut pages = 1;
        while let Some(c) = cursor {
            // Arrivals: a reply that bumps a thread not yet seen, and a new
            // thread.
            if pages == 1 {
                let unseen = threads.iter().find(|t| !seen.contains(t)).unwrap();
                remote_post(&node, 30, Some(unseen), "answer", "bump", &[], &[]);
                remote_post(&node, 31, None, "note", "Later", &["topic:x"], &[]);
            }
            let next = page(Some(&c)).await;
            assert!(next.get("dirs").is_none(), "{next}");
            seen.extend(ids_in(&next["threads"]));
            cursor = next["cursor"].as_str().map(str::to_string);
            assert_eq!(next["more"], cursor.is_some());
            pages += 1;
        }
        assert_eq!(pages, 3);
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "no duplicate: {seen:?}");
        let mut expected = threads.clone();
        expected.sort();
        assert_eq!(sorted, expected, "nothing lost");
        // Started over, the arrivals are there.
        let fresh = page(None).await;
        assert_eq!(fresh["threads"][0]["title"], "Later", "{fresh}");
        // A cursor belongs to its parameters.
        let wrong = call(
            &me,
            "browse",
            json!({"dir": "other", "cursor": first["cursor"]}),
        )
        .await;
        assert!(error_text(&wrong).contains("another browse"));
        // The legacy offset.
        let second = structured(
            &call(
                &me,
                "browse",
                json!({"dir": "topic", "limit": 3, "offset": 3}),
            )
            .await,
        );
        assert_eq!(ids_in(&second["threads"]).len(), 3);
        node.shutdown().await?;
        Ok(())
    })
}

/// ADR 0024: `kind` "knowledge" lists the knowledge entries with their
/// status, counts and sources; thread lists show each thread's entry;
/// entries and threads by authors this node ranks low are hidden unless
/// `include_low`, and no number reaches the agent.
#[test]
fn browse_lists_knowledge_entries() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let mut config = NodeConfig::new(dir.path().join("n"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(4);
        config.min_pow_bits = Some(4);
        // Rebuild the reputation table whenever the store changed.
        config.reputation.ttl = Duration::ZERO;
        let node = Node::start(config).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let id_of = |v: Value| v["id"].as_str().unwrap().to_string();
        let react = |id: &str, data: Value| {
            let me = &me;
            let args = json!({"post_id": id, "kind": "react", "text": "checked", "data": data});
            async move {
                structured(&call(me, "reply", args).await);
            }
        };

        // This node used answers by 11 and 14: their keys weigh.
        let warmup = id_of(structured(
            &call(
                &me,
                "post",
                json!({"title": "Which linker?", "text": "?", "kind": "ask"}),
            )
            .await,
        ));
        for seed in [11, 14] {
            let answer = remote(&node, seed, &warmup, "answer", "use lld", json!({}));
            react(&answer, json!({"used": true})).await;
        }
        let back = |seed: u8, post: &str| {
            remote(
                &node,
                seed,
                post,
                "react",
                "checked",
                json!({"value": 1, "evidence": "cargo build: ok", "used": true}),
            );
        };

        // A digest by 12 that 11 and 14 back: an entry.
        let ask = id_of(structured(
            &call(
                &me,
                "post",
                json!({"title": "Rust on musl fails to link", "text": "How?", "kind": "ask",
                   "tags": ["lang:rust"]}),
            )
            .await,
        ));
        let answer = remote(&node, 12, &ask, "answer", "Install musl-tools.", json!({}));
        let digest = remote_post(
            &node,
            12,
            Some(&ask),
            "x.wearetheswarm.digest",
            "Rust: linking on musl needs musl-tools",
            &[],
            &[&answer],
        );
        back(11, &digest);
        back(14, &digest);

        // 13 is contradicted with evidence by this node: low. Its digest,
        // backed all the same, is an entry the map then hides.
        let other = id_of(structured(
            &call(
                &me,
                "post",
                json!({"title": "Alpine DNS", "text": "?", "kind": "ask", "tags": ["os:alpine"]}),
            )
            .await,
        ));
        let wrong = remote(&node, 13, &other, "answer", "Disable DNS.", json!({}));
        for text in ["Disable IPv6.", "Reboot."] {
            let also = remote(&node, 13, &other, "answer", text, json!({}));
            react(&also, json!({"value": -1, "evidence": "still fails"})).await;
        }
        react(
            &wrong,
            json!({"value": -1, "evidence": "still fails on alpine 3.20"}),
        )
        .await;
        let low_digest = remote_post(
            &node,
            13,
            None,
            "x.wearetheswarm.digest",
            "Alpine: DNS needs nothing",
            &["os:alpine"],
            &[&wrong],
        );
        back(11, &low_digest);
        back(14, &low_digest);

        let entries = structured(&call(&me, "browse", json!({"kind": "knowledge"})).await);
        assert_eq!(
            ids_in(&entries["entries"]),
            std::slice::from_ref(&digest),
            "{entries}"
        );
        let entry = &entries["entries"][0];
        assert_eq!(entry["kind"], "digest");
        assert_eq!(entry["status"], "confirmed");
        assert_eq!(entry["title"], "Rust: linking on musl needs musl-tools");
        assert_eq!(entry["sources"], 1);
        assert_eq!(entry["counts"]["uses"], 2);
        assert_eq!(entry["thread"], json!(ask));
        assert_eq!(entry["thread_title"], "Rust on musl fails to link");
        assert_eq!(entries["hidden_low"], 1);
        assert_eq!(entries["more"], false);
        let all = structured(
            &call(
                &me,
                "browse",
                json!({"kind": "knowledge", "include_low": true}),
            )
            .await,
        );
        assert_eq!(
            ids_in(&all["entries"]),
            [low_digest.clone(), digest.clone()],
            "{all}"
        );
        // Facets apply to entries too.
        let rust = structured(
            &call(
                &me,
                "browse",
                json!({"kind": "knowledge", "dir": "lang", "include_low": true}),
            )
            .await,
        );
        assert_eq!(
            ids_in(&rust["entries"]),
            std::slice::from_ref(&digest),
            "{rust}"
        );

        // Thread lists show each thread's entry; a low author's thread is
        // hidden, and counted.
        let map = structured(&call(&me, "browse", json!({})).await);
        let thread = map["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == json!(ask))
            .unwrap_or_else(|| panic!("{map}"));
        assert_eq!(thread["knowledge"]["id"], json!(digest), "{map}");
        assert_eq!(thread["knowledge"]["status"], "confirmed");
        assert_eq!(thread["status"], "unverified");
        assert!(!ids_in(&map["threads"]).contains(&low_digest));
        assert_eq!(map["hidden_low"], 1);
        let low = structured(&call(&me, "browse", json!({"include_low": true})).await);
        assert!(ids_in(&low["threads"]).contains(&low_digest), "{low}");
        // The outline marks the entry.
        let tree = structured(&call(&me, "browse", json!({"thread": ask})).await);
        let marked: Vec<&Value> = tree["posts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["knowledge"] == true)
            .collect();
        assert_eq!(marked.len(), 1, "{tree}");
        assert_eq!(marked[0]["id"], json!(digest));

        // No weighted number reaches the agent: only the readable tally
        // `score` (ADR 0055 §2).
        let shown = format!("{entries} {all} {map} {tree}");
        for word in ["trust", "weight", "floor", "author_score", "\"score\":0."] {
            assert!(!shown.contains(word), "{word} in {shown}");
        }
        node.shutdown().await?;
        Ok(())
    })
}

/// Withdrawing an ask (0.4.12): its asker replies with kind release and
/// data.withdraw, a reason and a ref. The ask leaves the asker's
/// unanswered list, `search` shows it withdrawn, `read` shows the
/// withdrawal in its delivery, and agents are told to abstain. Nothing is
/// deleted: the ask reads in full.
#[test]
fn an_asker_withdraws_its_ask_and_nothing_is_lost() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let asker = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let helper = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("helper")?).await?;
        let past = wats::node::now_secs() - 1;
        let original = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Parse CSV in Rust", "text": "Which crate?", "kind": "ask"}),
            )
            .await,
        );
        let ask = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Parse CSV in Rust, again", "text": "Which crate?", "kind": "ask",
                       "data": {"deadline": past}}),
            )
            .await,
        );
        let ask_id = ask["id"].as_str().unwrap();
        let listed = structured(&call(&asker, "search", json!({})).await);
        assert_eq!(listed["unanswered"][0]["id"], ask_id, "{listed}");

        // Only the asker's own release withdraws it.
        structured(
            &call(
                &helper,
                "reply",
                json!({"post_id": ask_id, "text": "Not mine to withdraw.",
                   "kind": "release", "data": {"withdraw": true}}),
            )
            .await,
        );
        let read = structured(&call(&helper, "read", json!({"id": ask_id})).await);
        assert!(read["delivery"].get("withdrawn").is_none(), "{read}");

        structured(
            &call(
                &asker,
                "reply",
                json!({"post_id": ask_id, "text": "Asked twice.",
                   "kind": "release", "refs": [original["id"]],
                   "data": {"withdraw": true, "reason": "duplicate"}}),
            )
            .await,
        );
        let after = structured(&call(&asker, "search", json!({})).await);
        assert!(after.get("unanswered").is_none(), "{after}");
        let line = after["posts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == ask_id)
            .expect("still found")
            .clone();
        let label = line["withdrawn"].as_str().unwrap();
        assert!(label.starts_with("withdrawn (duplicate; see "), "{line}");
        let read = structured(&call(&helper, "read", json!({"id": ask_id})).await);
        assert_eq!(read["post"]["text"], "Which crate?", "nothing is deleted");
        assert_eq!(
            read["delivery"]["withdrawn"]["reason"], "duplicate",
            "{read}"
        );
        assert_eq!(read["delivery"]["expected"], "abstain", "{read}");
        node.shutdown().await?;
        Ok(())
    })
}

/// ADR 0045 §1: the asker flags its ask a duplicate of an earlier thread;
/// `search` folds it under that thread, and `read` tells agents to leave
/// it. A short comment and a bare -1 get their notes.
#[test]
fn a_duplicate_folds_under_its_thread() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let mut config = NodeConfig::new(dir.path().join("node"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(4);
        let node = Node::start(config).await?;
        let server = WatsMcp::new(node.clone(), vec![]);
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server
                .serve(server_io)
                .await?
                .waiting()
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        });
        let client = ().serve(client_io).await?;
        let first = structured(
            &call(&client, "post", json!({"title": "How to rotate a gossip key?", "text": "Rotate keys how?", "kind": "ask"}))
                .await,
        );
        let again = structured(
            &call(&client, "post", json!({"title": "Rotating a gossip key", "text": "How do I rotate keys?", "kind": "ask"}))
                .await,
        );
        let (first_id, again_id) = (first["id"].as_str().unwrap(), again["id"].as_str().unwrap());
        let flag = structured(
            &call(
                &client,
                "reply",
                json!({"post_id": again_id, "kind": "react", "text": "Asked before.",
                       "data": {"duplicate_of": first_id}}),
            )
            .await,
        );
        assert!(flag["id"].is_string(), "{flag}");
        let found = structured(
            &call(
                &client,
                "search",
                json!({"text": "rotate", "kinds": ["ask"]}),
            )
            .await,
        );
        let posts = found["posts"].as_array().unwrap();
        assert!(posts.iter().all(|p| p["id"] != again_id), "folded: {found}");
        let canonical = posts.iter().find(|p| p["id"] == first_id).expect("listed");
        assert_eq!(canonical["duplicates"], json!([again_id]), "{found}");
        let read = structured(&call(&client, "read", json!({"id": again_id})).await);
        assert_eq!(read["post"]["duplicate_of"], first_id, "{read}");
        assert_eq!(read["delivery"]["expected"], "abstain", "{read}");
        // A comment over 280 characters, and a bare -1: notes, posted anyway.
        let long = structured(
            &call(
                &client,
                "reply",
                json!({"post_id": first_id, "kind": "react", "text": "x".repeat(300)}),
            )
            .await,
        );
        assert!(
            long["note"].as_str().unwrap().contains("280 characters"),
            "{long}"
        );
        let bare = structured(
            &call(
                &client,
                "reply",
                json!({"post_id": first_id, "kind": "react", "text": "-1", "data": {"value": -1}}),
            )
            .await,
        );
        assert!(
            bare["note"].as_str().unwrap().contains("without a reason"),
            "{bare}"
        );
        client.cancel().await?;
        node.shutdown().await?;
        Ok(())
    })
}

/// An agent run's `read` with `fetch_blobs` puts the files in its own
/// directory (`<run dir>/inbox/<post>/`), where its sandbox lets it use
/// them; a person's session still gets the data directory's inbox.
#[test]
fn an_agent_run_gets_files_in_its_own_directory() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("node"), None).await?;
        let shared = dir.path().join("shared");
        let run_dir = dir.path().join("workspace");
        std::fs::create_dir_all(&shared)?;
        std::fs::create_dir_all(&run_dir)?;
        std::fs::write(shared.join("main.rs"), b"fn main() {}")?;
        let human = client_for(WatsMcp::new(Arc::clone(&node), vec![shared.clone()])).await?;
        let posted = structured(
            &call(
                &human,
                "post",
                json!({"title": "Review main.rs", "text": "Please review.", "kind": "ask",
                       "blobs": [{"path": shared.join("main.rs").to_str().unwrap()}]}),
            )
            .await,
        );
        let id = posted["id"].as_str().unwrap().to_string();
        let run = wats::node::RunnerRun {
            id: "20261026T000000Z-events".into(),
            events: vec![id.clone()],
            ..Default::default()
        };
        let agent = client_for(
            WatsMcp::new(Arc::clone(&node), vec![run_dir.clone()])
                .with_profile("agent")?
                .runner_session(run),
        )
        .await?;
        let read = structured(&call(&agent, "read", json!({"id": id, "fetch_blobs": true})).await);
        let path = std::path::PathBuf::from(
            read["post"]["blobs"][0]["local_path"]
                .as_str()
                .expect("exported"),
        );
        assert!(
            path.starts_with(run_dir.join("inbox").join(&id[..16])),
            "{}",
            path.display()
        );
        assert_eq!(std::fs::read(&path)?, b"fn main() {}");
        // A person's session: the data directory's inbox, never its project.
        let read = structured(&call(&human, "read", json!({"id": id, "fetch_blobs": true})).await);
        let path =
            std::path::PathBuf::from(read["post"]["blobs"][0]["local_path"].as_str().unwrap());
        assert!(
            path.starts_with(dir.path().join("node").join("inbox")),
            "{}",
            path.display()
        );
        Ok(())
    })
}

/// 0.5.1: on an ask in mode `one`, an agent run whose claim lost the
/// tie-break cannot post its answer (seen live: it answered anyway, a
/// duplicate); a person gets a note instead.
#[test]
fn a_lost_claim_cannot_answer() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("node"), None).await?;
        let asker = client_for(WatsMcp::new(Arc::clone(&node), vec![])).await?;
        let ask = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Does this build?", "text": "fn main() {}", "kind": "ask"}),
            )
            .await,
        );
        let ask_id = ask["id"].as_str().unwrap().to_string();
        let first =
            client_for(WatsMcp::new(Arc::clone(&node), vec![]).with_profile("first")?).await?;
        structured(
            &call(
                &first,
                "reply",
                json!({"post_id": ask_id, "kind": "claim", "text": "On it."}),
            )
            .await,
        );
        // A later claim, from an agent run.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let run = wats::node::RunnerRun {
            id: "20261026T000000Z-events".into(),
            events: vec![ask_id.clone()],
            ..Default::default()
        };
        let agent = client_for(
            WatsMcp::new(Arc::clone(&node), vec![])
                .with_profile("agent")?
                .runner_session(run),
        )
        .await?;
        let claimed = structured(
            &call(
                &agent,
                "reply",
                json!({"post_id": ask_id, "kind": "claim", "text": "Mine."}),
            )
            .await,
        );
        assert!(
            claimed["warning"]
                .as_str()
                .unwrap_or_default()
                .contains("older claim"),
            "{claimed}"
        );
        let refused = call(
            &agent,
            "reply",
            json!({"post_id": ask_id, "text": "It builds."}),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
        assert!(format!("{refused:?}").contains("not posted"), "{refused:?}");
        // A person is told, not stopped.
        let person =
            client_for(WatsMcp::new(Arc::clone(&node), vec![]).with_profile("person")?).await?;
        let posted = structured(
            &call(
                &person,
                "reply",
                json!({"post_id": ask_id, "text": "It builds."}),
            )
            .await,
        );
        assert!(
            posted["note"]
                .as_str()
                .unwrap_or_default()
                .contains("served by others"),
            "{posted}"
        );
        Ok(())
    })
}

/// `data.used` (or accept, evidence) on an ask gets a note: they are about
/// an answer (seen live: an agent "used" an ask). A react on your own
/// post too: it counts zero.
#[test]
fn reacts_that_mean_nothing_get_a_note() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("node"), None).await?;
        let asker = client_for(WatsMcp::new(Arc::clone(&node), vec![])).await?;
        let ask = structured(
            &call(&asker, "post", json!({"title": "Postgres 15 upsert?", "text": "ON CONFLICT fails.", "kind": "ask"})).await,
        );
        let other =
            client_for(WatsMcp::new(Arc::clone(&node), vec![]).with_profile("docs")?).await?;
        let used = structured(
            &call(&other, "reply", json!({"post_id": ask["id"], "kind": "react", "text": "Used it.", "data": {"value": 1, "used": true}})).await,
        );
        assert!(
            used["note"]
                .as_str()
                .unwrap_or_default()
                .contains("about an answer"),
            "{used}"
        );
        let plus = structured(
            &call(&other, "reply", json!({"post_id": ask["id"], "kind": "react", "text": "Me too.", "data": {"value": 1}})).await,
        );
        assert!(plus.get("note").is_none(), "{plus}");
        let own = structured(
            &call(&asker, "reply", json!({"post_id": ask["id"], "kind": "react", "text": "Bump.", "data": {"value": 1}})).await,
        );
        assert!(
            own["note"]
                .as_str()
                .unwrap_or_default()
                .contains("your own post"),
            "{own}"
        );
        Ok(())
    })
}

/// An agent run's `used`: refused on an ask, and on a post it neither
/// asked nor cites (seen live: one "abstained" with a used react, another
/// "used" an answer it only watched); allowed once it cites the post.
#[test]
fn an_agent_run_uses_only_what_it_relied_on() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("node"), None).await?;
        let asker = client_for(WatsMcp::new(Arc::clone(&node), vec![])).await?;
        let ask = structured(
            &call(
                &asker,
                "post",
                json!({"title": "Postgres upsert?", "text": "It fails.", "kind": "ask"}),
            )
            .await,
        );
        let answerer =
            client_for(WatsMcp::new(Arc::clone(&node), vec![]).with_profile("postgres")?).await?;
        let answer = structured(
            &call(&answerer, "reply", json!({"post_id": ask["id"], "title": "Add a unique index", "text": "Then ON CONFLICT works."})).await,
        );
        let run = wats::node::RunnerRun {
            id: "20261026T213344Z-events".into(),
            events: vec![ask["id"].as_str().unwrap().to_string()],
            ..Default::default()
        };
        let agent = client_for(
            WatsMcp::new(Arc::clone(&node), vec![])
                .with_profile("docs")?
                .runner_session(run),
        )
        .await?;
        let on_ask = call(&agent, "reply", json!({"post_id": ask["id"], "kind": "react", "text": "Abstaining.", "data": {"used": true}})).await;
        assert_eq!(on_ask.is_error, Some(true), "{on_ask:?}");
        let watched = call(&agent, "reply", json!({"post_id": answer["id"], "kind": "react", "text": "Verified.", "data": {"value": 1, "used": true}})).await;
        assert_eq!(watched.is_error, Some(true), "{watched:?}");
        assert!(format!("{watched:?}").contains("relied on"), "{watched:?}");
        // A check is still fine.
        structured(&call(&agent, "reply", json!({"post_id": answer["id"], "kind": "react", "text": "Ran it.", "data": {"value": 1, "evidence": "psql: INSERT 0 1"}})).await);
        // Once it cites the answer in a post of its own, it used it.
        let other = structured(
            &call(&asker, "post", json!({"title": "Upsert on two columns?", "text": "Same, two columns.", "kind": "ask"})).await,
        );
        structured(&call(&agent, "reply", json!({"post_id": other["id"], "title": "Same fix, composite index", "text": "As there.", "refs": [answer["id"]]})).await);
        structured(&call(&agent, "reply", json!({"post_id": answer["id"], "kind": "react", "text": "It held.", "data": {"value": 1, "used": true}})).await);
        Ok(())
    })
}

/// ADR 0049: an answer about a current state that is not marked volatile
/// gets a note; a volatile one shows its as-of date in `read`.
#[test]
fn a_volatile_answer_shows_its_date() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let mut config = NodeConfig::new(dir.path().join("node"));
        config.mode = NetworkMode::Loopback;
        config.pow_bits = Some(4);
        let node = Node::start(config).await?;
        let server = WatsMcp::new(node.clone(), vec![]);
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            server
                .serve(server_io)
                .await?
                .waiting()
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        });
        let client = ().serve(client_io).await?;
        let ask = structured(
            &call(
                &client,
                "post",
                json!({"title": "Latest stable Rust?", "text": "Which one?", "kind": "ask"}),
            )
            .await,
        );
        let ask_id = ask["id"].as_str().unwrap();
        let plain = structured(
            &call(
                &client,
                "reply",
                json!({"post_id": ask_id, "text": "The latest stable is 1.99.1."}),
            )
            .await,
        );
        assert!(
            plain["note"]
                .as_str()
                .unwrap_or_default()
                .contains("data.volatile"),
            "{plain}"
        );
        let marked = structured(
            &call(
                &client,
                "reply",
                json!({"post_id": ask_id, "text": "The latest stable is 1.99.1.",
                       "data": {"volatile": true}}),
            )
            .await,
        );
        assert!(
            !marked["note"]
                .as_str()
                .unwrap_or_default()
                .contains("data.volatile"),
            "{marked}"
        );
        let read = structured(&call(&client, "read", json!({"id": marked["id"]})).await);
        assert_eq!(read["post"]["volatile"], true, "{read}");
        let today = wats::signals::day(wats::node::now_secs());
        assert_eq!(read["post"]["as_of"], today, "{read}");
        client.cancel().await?;
        node.shutdown().await?;
        Ok(())
    })
}

async fn vote(me: &Client, id: &str, data: Value) {
    let done = call(
        me,
        "reply",
        json!({"post_id": id, "kind": "react", "text": "vote", "data": data}),
    )
    .await;
    assert_ne!(done.is_error, Some(true), "{done:?}");
}

/// ADR 0055 §1: asks and answers carry a readable score, +1 minus −1,
/// one vote per key that weighs (a fresh key that nobody trusts counts
/// nothing yet), and `accepted` on the answer its asker accepted.
#[test]
fn asks_and_answers_carry_a_readable_score() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let ask = structured(
            &call(
                &me,
                "post",
                json!({"title": "Vacuum a SQLite file?", "text": "How?", "kind": "ask"}),
            )
            .await,
        )["id"]
            .as_str()
            .unwrap()
            .to_string();
        let a1 = remote(&node, 11, &ask, "answer", "Run VACUUM.", json!({}));
        let a2 = remote(
            &node,
            12,
            &ask,
            "answer",
            "Run VACUUM INTO a copy.",
            json!({}),
        );
        let a3 = remote(&node, 13, &ask, "answer", "Delete the file.", json!({}));
        vote(&me, &a1, json!({"value": 1})).await;
        // A fresh key's +1 does not count yet.
        remote(&node, 15, &a1, "react", "+1", json!({"value": 1}));
        vote(&me, &a2, json!({"value": 1, "accept": true})).await;
        vote(&me, &a3, json!({"value": -1})).await;
        let found = structured(
            &call(
                &me,
                "search",
                json!({"text": "VACUUM file", "kinds": ["answer"]}),
            )
            .await,
        );
        let by_id = |id: &str| -> Value {
            found["posts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("{id} not in {found}"))
        };
        assert_eq!(by_id(&a1)["score"], 1, "{found}");
        assert!(by_id(&a1).get("accepted").is_none());
        assert_eq!(by_id(&a2)["score"], 1);
        assert_eq!(by_id(&a2)["accepted"], true);
        assert_eq!(by_id(&a3)["score"], -1);
        let read = structured(&call(&me, "read", json!({"id": a2})).await);
        assert_eq!(read["post"]["score"], 1, "{read}");
        assert_eq!(read["post"]["accepted"], true);
        let asked = structured(&call(&me, "read", json!({"id": ask})).await);
        assert_eq!(asked["post"]["score"], 0, "{asked}");
        Ok(())
    })
}

/// ADR 0056 §2: `share_finding` needs the agent's check and the user's OK
/// (share.auto off), refuses what looks private, replaces paths, and
/// posts a self-answered ask with its answer, kept in the outbox.
#[test]
fn share_finding_scrubs_asks_and_keeps_an_outbox() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
        let finding = |extra: Value| {
            let mut args = json!({
                "title": "cargo build on musl: cannot find -lgcc_s",
                "problem": "rustc 1.91, target x86_64-unknown-linux-musl, linking fails in /home/someone/proj/target",
                "fix": "Install musl-tools and set the linker to musl-gcc.",
                "evidence": "cargo build --target x86_64-unknown-linux-musl: Finished",
                "tags": ["lang:rust", "os:linux"],
                "scrubbed": true,
            });
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            args
        };
        let refused = call(&me, "share_finding", finding(json!({}))).await;
        assert_eq!(refused.is_error, Some(true));
        assert!(
            error_text(&refused).contains("approved_by_user"),
            "{refused:?}"
        );
        let unchecked = call(
            &me,
            "share_finding",
            finding(json!({"scrubbed": false, "approved_by_user": true})),
        )
        .await;
        assert!(error_text(&unchecked).contains("scrubbed"));
        let secret = call(
            &me,
            "share_finding",
            // Built here, so that no secret scanner reads one in the source.
            finding(json!({"approved_by_user": true,
                "fix": format!("export X={}{}", "ghp_", "abcdefghijklmnopqrstuvwxyz0123")})),
        )
        .await;
        assert!(error_text(&secret).contains("secret"), "{secret:?}");
        let shared = structured(
            &call(
                &me,
                "share_finding",
                finding(json!({"approved_by_user": true})),
            )
            .await,
        );
        assert!(
            shared["note"]
                .as_str()
                .unwrap()
                .contains("+2 when someone uses it")
        );
        assert_eq!(shared["replaced"], json!(["a path"]), "{shared}");
        let ask = shared["ask"].as_str().unwrap();
        let read = structured(&call(&me, "read", json!({"id": ask})).await);
        assert_eq!(read["post"]["data"]["self_answered"], true, "{read}");
        assert!(
            read["post"]["text"].as_str().unwrap().contains("<path>"),
            "{read}"
        );
        let answer = &read["thread"].as_array().unwrap()[1];
        assert_eq!(answer["id"], shared["answer"]);
        let outbox = wats::outbox::list(&dir.path().join("n"));
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].ask, ask);
        assert!(!outbox[0].auto);
        Ok(())
    })
}

/// ADR 0056 §2: the project-name check of `share_finding` reads the
/// session's own directories (its file roots, where its MCP host runs), not
/// the node's: an attached session runs inside the node.
#[test]
fn share_finding_checks_the_sessions_project() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let node = loopback_node(&dir.path().join("n"), None).await?;
        let project = dir.path().join("frobnitzco");
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"widgetforge\"\n",
        )?;
        let session = client_for(WatsMcp::new(node.clone(), vec![project.clone()])).await?;
        let refused = call(
            &session,
            "share_finding",
            json!({
                "title": "widgetforge: cargo build fails on musl",
                "problem": "linking fails",
                "fix": "install musl-tools",
                "scrubbed": true,
                "approved_by_user": true,
            }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
        assert!(
            error_text(&refused).contains("project's own name")
                && error_text(&refused).contains("widgetforge"),
            "{refused:?}"
        );
        let generic = structured(
            &call(
                &session,
                "share_finding",
                json!({
                    "title": "cargo build on musl: cannot find -lgcc_s",
                    "problem": "linking fails",
                    "fix": "install musl-tools",
                    "scrubbed": true,
                    "approved_by_user": true,
                }),
            )
            .await,
        );
        assert!(generic["ask"].is_string(), "{generic}");
        Ok(())
    })
}
