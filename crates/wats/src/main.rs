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

use wats::attach;
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
        /// Print nothing on stdout (background use).
        #[arg(long)]
        quiet: bool,
        /// Exit after this many seconds without attached MCP sessions.
        #[arg(long)]
        idle_exit: Option<u64>,
    },
    /// MCP server on stdio (for Claude Code and other hosts). Sessions share
    /// one node per data directory, started in the background if needed.
    Mcp {
        /// Seconds a background node started by `wats mcp` keeps running
        /// after its last session ends.
        #[arg(long, default_value_t = 600)]
        idle_exit: u64,
        /// Run the node inside this process instead of sharing a background
        /// node (one session per data directory).
        #[arg(long)]
        in_process: bool,
    },
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

impl NodeArgs {
    /// Arguments that recreate this network configuration in a child process.
    fn to_args(&self, dir: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec!["--dir".into(), dir.into()];
        if self.loopback {
            args.push("--loopback".into());
        }
        for net in &self.nets {
            args.push(format!("--net={net}").into());
        }
        for peer in &self.peers {
            args.push(format!("--peer={peer}").into());
        }
        if let Some(bits) = self.pow_bits {
            args.push(format!("--pow-bits={bits}").into());
        }
        if let Some(bits) = self.min_pow_bits {
            args.push(format!("--min-pow-bits={bits}").into());
        }
        args
    }
}

/// Start `wats node` for `dir` as a detached background process, logging
/// to `<dir>/node.log`.
#[cfg(unix)]
fn spawn_background_node(args: &NodeArgs, dir: &std::path::Path, idle_exit: u64) -> Result<()> {
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("node.log"))?;
    let child = std::process::Command::new(std::env::current_exe()?)
        .args(args.to_args(dir))
        .args(["node", "--quiet", "--idle-exit", &idle_exit.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        // Own process group: the node survives the session that started it.
        .process_group(0)
        .spawn()
        .context("start background node")?;
    tracing::info!(pid = child.id(), "started background node");
    // Not waited for on purpose: it outlives this process.
    drop(child);
    Ok(())
}

/// Serve this session through the node that owns `dir`, starting one in
/// the background if none runs.
#[cfg(unix)]
async fn mcp_attached(args: &NodeArgs, dir: &std::path::Path, idle_exit: u64) -> Result<()> {
    let attach = || attach::attach(dir, args.profile.clone(), default_file_roots());
    if attach().await? {
        return Ok(());
    }
    spawn_background_node(args, dir, idle_exit)?;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if attach().await? {
            return Ok(());
        }
    }
    anyhow::bail!(
        "the background node did not start; see {}",
        dir.join("node.log").display()
    )
}

fn print_line(value: serde_json::Value) {
    println!("{value}");
}

struct NodeRun {
    post: Option<String>,
    kind: String,
    tags: Vec<String>,
    blobs: Vec<PathBuf>,
    quiet: bool,
    idle_exit: Option<u64>,
}

async fn run_node(node: Arc<Node>, run: NodeRun) -> Result<()> {
    let NodeRun {
        post,
        kind,
        tags,
        blobs,
        quiet,
        idle_exit,
    } = run;
    let print_line = move |value: serde_json::Value| {
        if !quiet {
            print_line(value);
        }
    };
    let sessions = attach::serve(Arc::clone(&node))?;
    let mut events = node.store().subscribe();
    let mut fetched = node.subscribe_fetched();
    let mut idle_since = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
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
                Err(e) => tracing::warn!("post failed: {e:#}"),
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
            _ = tick.tick() => if let Some(idle) = idle_exit {
                if sessions.active() > 0 {
                    idle_since = tokio::time::Instant::now();
                } else if idle_since.elapsed() >= Duration::from_secs(idle) {
                    tracing::info!("no MCP session for {idle} s, exiting");
                    break;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = std::fs::remove_file(attach::socket_path(&node.config().data_dir));
    node.shutdown().await
}

async fn run_mcp(node: Arc<Node>) -> Result<()> {
    let sessions = attach::serve(Arc::clone(&node))?;
    let server = WatsMcp::new(Arc::clone(&node), default_file_roots());
    let service = server.serve(rmcp::transport::stdio()).await?;
    tokio::select! {
        result = service.waiting() => { result?; }
        _ = tokio::signal::ctrl_c() => {}
    }
    // Other sessions attached to this node: keep serving them.
    while sessions.active() > 0 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = std::fs::remove_file(attach::socket_path(&node.config().data_dir));
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
    let config = cli.node.config()?;
    match cli.command {
        Command::Node {
            post,
            kind,
            tags,
            blobs,
            quiet,
            idle_exit,
        } => {
            let node = Node::start(config).await?;
            let run = NodeRun {
                post,
                kind,
                tags,
                blobs,
                quiet,
                idle_exit,
            };
            run_node(node, run).await
        }
        #[cfg(unix)]
        Command::Mcp {
            idle_exit,
            in_process: false,
        } => mcp_attached(&cli.node, &config.data_dir, idle_exit).await,
        Command::Mcp { .. } => run_mcp(Node::start(config).await?).await,
    }
}
