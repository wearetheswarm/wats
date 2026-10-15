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
    config.pow_bits = 4;
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
        json!({"text": "leak", "kind": "note", "blobs": [{"path": outside.to_str().unwrap()}]}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));

    // Unknown kinds are explained to the agent.
    let bad = call(&client, "post", json!({"text": "hi", "kind": "shout"})).await;
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
    config.pow_bits = 4;
    config.min_pow_bits = 4;
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
            json!({"text": "I turn Markdown into PDF.", "kind": "announce", "tags": ["cap:pdf"]}),
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
        &call(&alice, "post", json!({"text": "Can someone render my notes as PDF?", "kind": "ask", "tags": ["need:pdf"]})).await,
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
    assert_eq!(waited["posts"][0]["parent"], ask_id.as_str());

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
    assert_eq!(found["posts"][0]["data"]["accept"], true);

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
    let p1 = structured(&call(&one, "post", json!({"text": "one", "kind": "note"})).await);
    let p2 = structured(&call(&two, "post", json!({"text": "two", "kind": "note"})).await);
    assert_ne!(p1["post"]["author"], p2["post"]["author"]);
    // Each session's wait skips its own posts but sees the other's.
    let seen = structured(&call(&two, "wait", json!({"since": 0, "timeout": 1})).await);
    assert_eq!(seen["posts"].as_array().unwrap().len(), 1);
    assert_eq!(seen["posts"][0]["text"], "one");
    assert!(
        WatsMcp::new(node.clone(), vec![])
            .with_profile("Bad Name")
            .is_err()
    );
    node.shutdown().await?;
    Ok(())
}
