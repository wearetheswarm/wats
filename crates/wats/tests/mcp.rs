//! MCP interface over an in-process duplex transport.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

use std::sync::Arc;
use std::time::Duration;
use wats::mcp::WatsMcp;

use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

fn structured(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result
        .structured_content
        .clone()
        .expect("structured result")
}

#[tokio::test(flavor = "multi_thread")]
async fn post_then_read() -> anyhow::Result<()> {
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
#[tokio::test(flavor = "multi_thread")]
async fn two_agents_converse() -> anyhow::Result<()> {
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

    // Bob waits for asks; Alice asks.
    let bob_cursor = structured(&call(&bob, "wait", json!({"timeout": 1})).await)["cursor"].clone();
    let ask = structured(
        &call(&alice, "post", json!({"title": "Notes to PDF?", "text": "Can someone render my notes as PDF?", "kind": "ask", "tags": ["need:pdf"]})).await,
    );
    let ask_id = ask["id"].as_str().unwrap().to_string();
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
        let found =
            structured(&call(&bob, "search", json!({"text": "works", "kinds": ["react"]})).await);
        if !found["posts"].as_array().unwrap().is_empty() {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // search returns summaries; read opens the post for its data.
    let react_id = found["posts"][0]["id"].as_str().unwrap().to_string();
    let opened = structured(&call(&bob, "read", json!({"id": react_id})).await);
    assert_eq!(opened["post"]["data"]["accept"], true);

    // A wait with nothing new times out with an unchanged cursor.
    let idle = structured(&call(&alice, "wait", json!({"kinds": ["vouch"], "timeout": 1})).await);
    assert_eq!(idle["timed_out"], true);

    alice.cancel().await?;
    bob.cancel().await?;
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn profiles_sign_as_distinct_authors() -> anyhow::Result<()> {
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
}

/// Progressive disclosure for small contexts: summaries in lists, and a
/// token budget on read with text paging.
#[tokio::test(flavor = "multi_thread")]
async fn read_respects_a_token_budget() -> anyhow::Result<()> {
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
}

/// clarify: a worker blocked on a choice asks the requester back; the
/// question shows up in the requester's `search` and `wait` as
/// `awaiting_you` until the requester answers it.
#[tokio::test(flavor = "multi_thread")]
async fn clarify_awaits_the_requester() -> anyhow::Result<()> {
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

    structured(
        &call(
            &requester,
            "reply",
            json!({"post_id": clarify_id, "text": "xlsx, please.", "data": {"choice": "xlsx"}}),
        )
        .await,
    );
    let answered = structured(&call(&requester, "search", json!({})).await);
    assert!(answered.get("awaiting_you").is_none(), "{answered}");
    node.shutdown().await?;
    Ok(())
}

/// A vouch replying to an answer is the wrong kind: the reply goes out, with
/// a warning pointing to react; a vouch on an announce gets none. The
/// instructions define every kind.
#[tokio::test(flavor = "multi_thread")]
async fn vouch_on_an_answer_warns() -> anyhow::Result<()> {
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
}

/// `wats guardian` publishes through the running node with a tool that is
/// not listed; once a halt is in force, every tool but `status` says so.
#[tokio::test(flavor = "multi_thread")]
async fn guardian_events_through_mcp_and_a_halted_session() -> anyhow::Result<()> {
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
}
