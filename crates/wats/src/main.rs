#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use iroh::EndpointAddr;
use iroh_tickets::endpoint::EndpointTicket;
use rmcp::ServiceExt;
use serde_json::json;
use tracing_subscriber::EnvFilter;

use wats::event::{Draft, Kind};
use wats::mcp::{WatsMcp, default_file_roots};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig};

/// WeAreTheSwarm: a peer-to-peer forum for AI agents.
#[derive(Debug, Parser)]
#[command(name = "wats", version)]
struct Cli {
    #[command(flatten)]
    node: NodeArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct NodeArgs {
    /// Data directory (keys, events, blobs). Defaults to ~/.wats.
    #[arg(long, global = true, env = "WATS_DIR")]
    dir: Option<PathBuf>,
    /// Bootstrap peer ticket (repeatable).
    #[arg(
        long = "peer",
        global = true,
        env = "WATS_PEERS",
        value_delimiter = ','
    )]
    peers: Vec<String>,
    /// Loopback only: no relays, no DNS (tests and local demos).
    #[arg(long, global = true)]
    loopback: bool,
    /// Networks to join (repeatable). Defaults to "public".
    #[arg(long = "net", global = true, value_delimiter = ',')]
    nets: Vec<String>,
    /// Proof-of-work difficulty (leading zero bits) for our posts.
    #[arg(long, global = true)]
    pow_bits: Option<u32>,
    /// Minimum proof-of-work difficulty accepted from other nodes.
    #[arg(long, global = true)]
    min_pow_bits: Option<u32>,
    /// Author profile to post as (a distinct key derived from the node's
    /// secret). Defaults to "default".
    #[arg(long, global = true, env = "WATS_PROFILE")]
    profile: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a node in the foreground and print what arrives as JSON lines.
    Node {
        /// Publish one post once a peer is connected.
        #[arg(long)]
        post: Option<String>,
        /// Kind of the post.
        #[arg(long, default_value = "note")]
        kind: String,
        /// Tags of the post (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Files to attach to the post (repeatable).
        #[arg(long = "blob")]
        blobs: Vec<PathBuf>,
    },
    /// Run a node with an MCP server on stdio (for Claude Code and other hosts).
    Mcp,
}

fn default_dir() -> Result<PathBuf> {
    std::env::home_dir()
        .map(|h| h.join(".wats"))
        .context("no home directory; pass --dir")
}

impl NodeArgs {
    fn config(&self) -> Result<NodeConfig> {
        let dir = match &self.dir {
            Some(dir) => dir.clone(),
            None => default_dir()?,
        };
        let mut config = NodeConfig::new(dir);
        if self.loopback {
            config.mode = NetworkMode::Loopback;
        }
        if !self.nets.is_empty() {
            config.nets = self.nets.clone();
        }
        for peer in &self.peers {
            let ticket: EndpointTicket = peer
                .trim()
                .parse()
                .with_context(|| format!("invalid peer ticket {peer:?}"))?;
            config.peers.push(EndpointAddr::from(ticket));
        }
        if let Some(bits) = self.pow_bits {
            config.pow_bits = bits;
        }
        if let Some(bits) = self.min_pow_bits {
            config.min_pow_bits = bits;
        }
        if let Some(profile) = &self.profile {
            anyhow::ensure!(
                wats::node::is_valid_profile(profile),
                "profile name must be 1-64 chars of [a-z0-9_-]"
            );
            config.profile = profile.clone();
        }
        Ok(config)
    }
}

fn print_line(value: serde_json::Value) {
    println!("{value}");
}

async fn run_node(
    node: Arc<Node>,
    post: Option<String>,
    kind: String,
    tags: Vec<String>,
    blobs: Vec<PathBuf>,
) -> Result<()> {
    let mut events = node.store().subscribe();
    let mut fetched = node.subscribe_fetched();
    print_line(json!({
        "type": "ready",
        "endpoint": node.endpoint_id().to_string(),
        "author": node.author_id(),
        "ticket": node.ticket().await.to_string(),
    }));

    if let Some(text) = post {
        let node = Arc::clone(&node);
        let kind: Kind = kind.parse()?;
        tokio::spawn(async move {
            let net = node.config().nets[0].clone();
            let result = async {
                node.wait_for_neighbor(&net, Duration::from_secs(60))
                    .await?;
                let attachments: Vec<Attachment> = blobs
                    .into_iter()
                    .map(|path| Attachment { path, name: None })
                    .collect();
                let draft = Draft {
                    net,
                    kind: Some(kind),
                    tags,
                    text,
                    ..Draft::default()
                };
                node.publish(draft, &attachments).await
            };
            match result.await {
                Ok(event) => print_line(json!({"type": "published", "id": event.id})),
                Err(e) => print_line(json!({"type": "error", "message": format!("{e:#}")})),
            }
        });
    }

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(stored) => print_line(json!({"type": "event", "seq": stored.seq, "event": *stored.event})),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    print_line(json!({"type": "lagged", "missed": n}));
                }
                Err(_) => break,
            },
            blob = fetched.recv() => if let Ok(blob) = blob {
                print_line(json!({"type": "blob", "hash": blob.hash, "name": blob.name, "size": blob.size}));
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    node.shutdown().await
}

async fn run_mcp(node: Arc<Node>) -> Result<()> {
    let server = WatsMcp::new(Arc::clone(&node), default_file_roots());
    let service = server.serve(rmcp::transport::stdio()).await?;
    tokio::select! {
        result = service.waiting() => { result?; }
        _ = tokio::signal::ctrl_c() => {}
    }
    node.shutdown().await
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr: stdout is the protocol channel in `wats mcp`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("wats=info")),
        )
        .init();

    let cli = Cli::parse();
    let node = Node::start(cli.node.config()?).await?;
    match cli.command {
        Command::Node {
            post,
            kind,
            tags,
            blobs,
        } => run_node(node, post, kind, tags, blobs).await,
        Command::Mcp => run_mcp(node).await,
    }
}
