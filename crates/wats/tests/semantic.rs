//! Semantic search through MCP (ADR 0053): without its model, `search` is
//! full-text as before 0.5.0 and points to `browse`; with it (an ignored
//! test, `WATS_SEMANTIC_MODEL_DIR`), a paraphrase finds the ask.

use std::future::Future;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use wats::mcp::WatsMcp;
use wats::node::{NetworkMode, Node, NodeConfig};

fn run(test: impl Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("wats=debug")
        .with_ansi(false)
        .with_test_writer()
        .try_init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(test)
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> Value {
    let result: CallToolResult = tokio::time::timeout(
        Duration::from_secs(60),
        client.call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        ),
    )
    .await
    .expect("the call answers")
    .expect("the call works");
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.expect("structured result")
}

/// A node with semantic search on, its model in `model_dir` (or not), and
/// an MCP client.
async fn session(
    dir: &std::path::Path,
    model_dir: std::path::PathBuf,
) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
    let mut config = NodeConfig::new(dir.join("node"));
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    let mut semantic = wats::semantic::engine::Config::new(model_dir, false);
    semantic.laptop_manners = false;
    config.semantic = Some(semantic);
    let node = Node::start(config).await?;
    let server = WatsMcp::new(node, vec![]);
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
    Ok(().serve(client_io).await?)
}

const ASKS: &[(&str, &str)] = &[
    (
        "How do I fix borrow checker error E0502 in Rust?",
        "lang:rust",
    ),
    (
        "What is the best way to read a file line by line in Rust?",
        "lang:rust",
    ),
    (
        "How do Rust lifetimes work with structs holding references?",
        "lang:rust",
    ),
    ("Why does my tokio task never finish?", "lang:rust:async"),
    (
        "How do I configure a PostgreSQL connection pool for a web server?",
        "topic:postgres",
    ),
    (
        "What are the main advantages of Kotlin over Java?",
        "lang:kotlin",
    ),
    ("When was Usenet created, and why?", "topic:history"),
    ("What was IRC originally used for?", "topic:history"),
    ("Today's weather in Villeurbanne, France", "topic:weather"),
    (
        "Generate a 3D logo for a peer-to-peer project",
        "cap:image-gen",
    ),
    ("How to center a div with CSS grid?", "lang:css"),
    (
        "What does the GPL require when distributing binaries?",
        "topic:licenses",
    ),
    (
        "How can I speed up a slow SQL query with an index?",
        "topic:postgres",
    ),
    (
        "What is the difference between TCP and UDP?",
        "topic:networking",
    ),
    (
        "How do I undo the last git commit but keep the changes?",
        "tool:git",
    ),
    ("Explain how QUIC hole punching works", "topic:networking"),
    (
        "Which Python library is best for parsing PDFs?",
        "lang:python",
    ),
    ("How do I write unit tests for a Python CLI?", "lang:python"),
    (
        "What is a good recipe for sourdough bread?",
        "topic:cooking",
    ),
    ("How many moons does Jupiter have?", "topic:astronomy"),
    ("How do I set up a reverse proxy with nginx?", "tool:nginx"),
    ("What is the capital of Australia?", "topic:geography"),
    (
        "How do I convert a video to GIF with ffmpeg?",
        "tool:ffmpeg",
    ),
    (
        "What does the HTTP 429 status code mean?",
        "topic:networking",
    ),
    (
        "How to compute prime numbers efficiently in Python?",
        "lang:python",
    ),
];

async fn post_all(client: &rmcp::service::RunningService<rmcp::RoleClient, ()>) -> Vec<String> {
    let mut ids = Vec::new();
    for (title, tag) in ASKS {
        let posted = call(
            client,
            "post",
            json!({"title": title, "text": title, "kind": "ask", "tags": [tag]}),
        )
        .await;
        ids.push(posted["id"].as_str().unwrap().to_string());
    }
    ids
}

#[test]
fn without_the_model_search_is_full_text_and_points_to_browse() -> anyhow::Result<()> {
    run(async move {
        let dir = tempfile::tempdir()?;
        let client = session(dir.path(), dir.path().join("no-model")).await?;
        post_all(&client).await;
        let found = call(&client, "search", json!({"text": "rust borrow checker"})).await;
        let posts = found["posts"].as_array().unwrap();
        assert!(!posts.is_empty());
        // Full-text as before: no labels, no note.
        assert!(posts.iter().all(|p| p.get("match").is_none()), "{found}");
        assert!(found.get("note").is_none(), "{found}");
        // The directory worth browsing: lang:rust holds 4 threads (one of
        // them under lang:rust:async), the right depth.
        let hint = &found["also_browse"][0];
        assert_eq!(hint["dir"], "lang:rust", "{found}");
        assert_eq!(hint["threads"], 4);
        assert!(
            hint["hint"]
                .as_str()
                .unwrap()
                .starts_with("also browse dir lang:rust: 4 threads")
        );
        let status = call(&client, "status", json!({})).await;
        assert_eq!(status["semantic"]["state"], "off", "{status}");
        assert!(
            status["semantic"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("model not downloaded"),
            "{status}"
        );
        // Measured, locally.
        call(&client, "browse", json!({"dir": "lang:rust"})).await;
        let status = call(&client, "status", json!({})).await;
        let counts = &status["find"]["counts"];
        assert_eq!(counts["searches"], 1, "{status}");
        assert_eq!(counts["browses"], 1);
        assert_eq!(counts["browse_after_search"], 1);
        assert_eq!(counts["asks"], ASKS.len());
        assert!(counts.get("searches_semantic").is_none());
        Ok(())
    })
}

/// With the model (`WATS_SEMANTIC_MODEL_DIR`: a directory holding
/// `bge-small-en-v1.5-f16.safetensors`).
#[test]
#[ignore = "needs the model: WATS_SEMANTIC_MODEL_DIR=<dir>"]
fn a_paraphrase_finds_the_ask_by_meaning() -> anyhow::Result<()> {
    run(async move {
        let model_dir = std::path::PathBuf::from(std::env::var("WATS_SEMANTIC_MODEL_DIR")?);
        let dir = tempfile::tempdir()?;
        let client = session(dir.path(), model_dir).await?;
        let ids = post_all(&client).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let status = call(&client, "status", json!({})).await;
            if status["semantic"]["state"] == "ready" {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{status}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // Few words in common, and only small ones ("how", "the"): the
        // ask about Jupiter's moons comes first, by meaning.
        for (query, target) in [
            ("how big is the planet with the most moons", 19),
            ("bake a loaf of bread with wild yeast", 18),
        ] {
            let found = call(&client, "search", json!({"text": query})).await;
            let first = &found["posts"][0];
            assert_eq!(first["id"], ids[target].as_str(), "{found}");
            assert!(
                matches!(
                    first["match"].as_str(),
                    Some("both" | "meaning" | "meaning (weak)")
                ),
                "{found}"
            );
            assert!(found.get("note").is_none(), "{found}");
        }
        // Both lists: first, labelled both.
        let found = call(
            &client,
            "search",
            json!({"text": "Kotlin advantages compared to Java"}),
        )
        .await;
        assert_eq!(found["posts"][0]["id"], ids[5].as_str(), "{found}");
        assert_eq!(found["posts"][0]["match"], "both", "{found}");
        // Nothing in the store about it: said so.
        let found = call(
            &client,
            "search",
            json!({"text": "the best hiking trails in the Alps"}),
        )
        .await;
        assert!(
            found["note"]
                .as_str()
                .unwrap()
                .starts_with("nothing matched by meaning"),
            "{found}"
        );
        let status = call(&client, "status", json!({})).await;
        assert_eq!(status["find"]["counts"]["searches_semantic"], 4, "{status}");
        Ok(())
    })
}
