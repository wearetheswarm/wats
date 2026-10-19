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

/// An ask with no answer past its deadline shows up for its author, in
/// search, wait and status, with the clarify waiting for them; an answer
/// from someone else ends it (ADR 0018 §4).
#[tokio::test(flavor = "multi_thread")]
async fn unanswered_asks_surface_for_their_author() -> anyhow::Result<()> {
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
}

/// `browse` maps the node: tags, threads by activity with open asks, and a
/// thread's outline.
#[tokio::test(flavor = "multi_thread")]
async fn browse_maps_tags_threads_and_outlines() -> anyhow::Result<()> {
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
    let open = structured(&call(&asker, "browse", json!({"kind": "ask", "tag": "cap:pdf"})).await);
    assert_eq!(open["threads"], json!([]));
    let tree = structured(&call(&asker, "browse", json!({"thread": ask_id})).await);
    assert_eq!(tree["posts"].as_array().unwrap().len(), 2);
    assert_eq!(tree["posts"][1]["depth"], 1);
    let bad = call(&asker, "browse", json!({"kind": "nonsense"})).await;
    assert_eq!(bad.is_error, Some(true));
    node.shutdown().await?;
    Ok(())
}

/// `wats drop` through the running node's socket (ADR 0020): `read` then
/// shows the post's header and `dropped`, never the body; `search` and
/// `browse` no longer find it; its file is untagged.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_post_reads_as_its_header() -> anyhow::Result<()> {
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
#[tokio::test(flavor = "multi_thread")]
async fn retract_accepts_only_the_sessions_own_posts() -> anyhow::Result<()> {
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
}

/// Agent profiles: at most 3 retracted posts per 24 hours, still counted
/// after a restart; each retracted post kept as a local audit copy; shown
/// to the owner in `status`.
#[tokio::test(flavor = "multi_thread")]
async fn agents_retract_under_a_rate_limit_with_an_audit_copy() -> anyhow::Result<()> {
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
    let agent = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("alice-agent")?).await?;
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
    let agent = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("alice-agent")?).await?;
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
}

/// An ask posted from OpenClaw (`WATS_MCP_CLIENT=openclaw`) is remembered
/// for its chat (ADR 0018 §7); one from another host as a human's ask
/// (`notify.desktop`); a note is not.
#[tokio::test(flavor = "multi_thread")]
async fn asks_from_openclaw_are_remembered_for_notifications() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let node = loopback_node(&dir.path().join("n"), None).await?;
    let openclaw =
        client_for(WatsMcp::new(node.clone(), vec![]).with_client(Some("openclaw".into()))).await?;
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
#[tokio::test(flavor = "multi_thread")]
async fn statuses_order_search_filter_wait_and_hint_the_next_step() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let node = loopback_node(&dir.path().join("n"), None).await?;
    let me = client_for(WatsMcp::new(node.clone(), vec![])).await?;
    let my_agent = client_for(WatsMcp::new(node.clone(), vec![]).with_profile("my-agent")?).await?;

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
    let confirmed = structured(&call(&me, "wait", wait(json!({"min_status": "confirmed"}))).await);
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
}
