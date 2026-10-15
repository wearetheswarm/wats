//! MCP interface over an in-process duplex transport.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

use wats::mcp::WatsMcp;
use wats::node::{NetworkMode, Node, NodeConfig};

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
