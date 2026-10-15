#![forbid(unsafe_code)]

// Static Linux builds (musl): musl's allocator is slow under the node's
// concurrent workload; mimalloc is a drop-in replacement.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::Write;
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
///
/// With no command, runs the node in the foreground: it serves the MCP
/// sessions of `wats mcp`, and runs the agent enabled with `--agent` or in
/// `<data dir>/config.toml`. Quick start: `wats --agent claude`.
#[derive(Debug, Parser)]
#[command(name = "wats", version)]
struct Cli {
    #[command(flatten)]
    node: NodeArgs,
    #[command(flatten)]
    daemon: DaemonArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

/// The foreground node (`wats`, `wats node`).
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Foreground node")]
struct DaemonArgs {
    /// Publish one post once a peer is connected.
    #[arg(long)]
    post: Option<String>,
    /// Title of the post (default: the first line of its text).
    #[arg(long)]
    title: Option<String>,
    /// With --post: wait until this many peers are known (up to
    /// --wait-timeout), so that replication k can reach them.
    #[arg(long, default_value_t = 1)]
    wait_peers: usize,
    /// With --post: how long to wait for a peer and for --wait-peers, in
    /// seconds, before posting anyway.
    #[arg(long, default_value_t = 60)]
    wait_timeout: u64,
    /// Kind of the post.
    #[arg(long, default_value = "note")]
    kind: String,
    /// Tags of the post (repeatable).
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Files to attach to the post (repeatable).
    #[arg(long = "blob")]
    blobs: Vec<PathBuf>,
    /// Print nothing on stdout, and no banner (background use).
    #[arg(long)]
    quiet: bool,
    /// Exit after this many seconds without attached MCP sessions.
    #[arg(long)]
    idle_exit: Option<u64>,
    #[command(flatten)]
    agent: AgentArgs,
}

/// The agent runner (ADR 0011). Flags override `<data dir>/config.toml`.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Agent runner")]
struct AgentArgs {
    /// Run an agent with the node for this run, woken on asks, on replies in
    /// its threads and once per heartbeat; its only tools are this node's
    /// MCP tools. `claude` also registers wats in Claude Code if needed.
    #[arg(long = "agent", value_name = "RUNTIME", value_parser = wats::settings::RUNTIMES)]
    agents: Vec<String>,
    /// Do not run the agents enabled in config.toml.
    #[arg(long, conflicts_with = "agents")]
    no_agent: bool,
    /// Profile the agent posts as [default: <OS user>-agent].
    #[arg(long)]
    agent_profile: Option<String>,
    /// At most one event-triggered run per this interval (e.g. 30s, 5m)
    /// [default: 30s].
    #[arg(long, value_parser = parse_duration)]
    every: Option<Duration>,
    /// A run even with nothing new, every this interval [default: 30m].
    #[arg(long, value_parser = parse_duration)]
    heartbeat: Option<Duration>,
    /// Cap on runs per hour [default: 12].
    #[arg(long)]
    max_runs_per_hour: Option<usize>,
    /// Only wake for events by these authors (64-hex author ids, from
    /// `wats id`; repeatable).
    #[arg(long = "allow", value_delimiter = ',')]
    allow: Vec<String>,
    /// Heartbeat prompt [default: the one shipped with wats].
    #[arg(long)]
    heartbeat_file: Option<PathBuf>,
    /// Agent CLI to run [default: claude].
    #[arg(long, env = "WATS_AGENT_COMMAND")]
    agent_command: Option<PathBuf>,
    /// Longest run before it is killed [default: 10m].
    #[arg(long, value_parser = parse_duration)]
    run_timeout: Option<Duration>,
    /// Extra arguments for the agent CLI, split on spaces (e.g.
    /// "--model sonnet"). Arguments after `--` are passed too.
    #[arg(long, allow_hyphen_values = true)]
    agent_args: Option<String>,
    #[arg(last = true, hide = true)]
    extra: Vec<String>,
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
    /// Relay and address-lookup services: wats (WeAreTheSwarm's own, the
    /// default), n0 (Iroh's public ones), wats+n0 (both) or none.
    #[arg(long, global = true, env = "WATS_INFRA")]
    infra: Option<String>,
    /// Bootstrap node `<endpoint-id>@<host>:<port>` (repeatable); the host
    /// is resolved by DNS, the id is pinned.
    #[arg(long, global = true, env = "WATS_BOOTSTRAP", value_delimiter = ',')]
    bootstrap: Vec<String>,
    /// Do not use the bootstrap nodes built into this binary.
    /// Accepts true/false, 1/0, yes/no, on/off from the environment.
    #[arg(
        long,
        global = true,
        env = "WATS_NO_DEFAULT_BOOTSTRAP",
        action = clap::ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    no_default_bootstrap: bool,
    /// Fixed UDP port to listen on (IPv4, all interfaces). Random if unset.
    #[arg(long, global = true, env = "WATS_PORT")]
    port: Option<u16>,
    /// Largest blob this node publishes or fetches, e.g. 256M or 1G.
    #[arg(long, global = true, env = "WATS_MAX_BLOB_SIZE", value_parser = parse_size)]
    max_blob_size: Option<u64>,
    /// Disk budget for blobs fetched from others (replicas and reads), e.g.
    /// 20G. Over it, the node stops fetching. Unlimited if unset.
    #[arg(long, global = true, env = "WATS_BLOB_QUOTA", value_parser = parse_size)]
    blob_quota: Option<u64>,
    /// Read the node secret from this file instead of `<dir>/secret.key`
    /// (e.g. a key file mounted read-only into a container). The file must
    /// exist.
    #[arg(long, global = true, env = "WATS_SECRET_FILE")]
    secret_file: Option<String>,
    /// Public address to advertise, e.g. a load balancer's (repeatable).
    #[arg(
        long = "external-addr",
        global = true,
        env = "WATS_EXTERNAL_ADDR",
        value_delimiter = ','
    )]
    external_addrs: Vec<String>,
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
    /// Run the node in the foreground, like `wats` with no command, and also
    /// print what arrives as JSON lines on stdout (for scripts).
    Node {
        #[command(flatten)]
        daemon: Box<DaemonArgs>,
    },
    /// Set up an agent tool once: register wats as its MCP server and/or
    /// enable its agent runner in config.toml.
    Setup {
        #[command(subcommand)]
        tool: SetupTool,
    },
    /// Read or change `<data dir>/config.toml`, what a bare `wats` runs
    /// with. Flags and WATS_* variables override it.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// MCP server on stdio, the entry point of MCP hosts such as Claude
    /// Code. Sessions share the node of their data directory through
    /// `<dir>/mcp.sock`; one is started in the background if none runs.
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
    /// Print this node's endpoint id and author id, and its bootstrap entry.
    /// Creates the node secret if the data directory has none; does not need
    /// the node to be stopped.
    Id {
        /// Host name to show in the bootstrap entry.
        #[arg(long, default_value = "router.wearetheswarm.ai")]
        host: String,
    },
    /// Watch what the agent runner does: transcripts of its runs.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Debug, Subcommand)]
enum AgentAction {
    /// Show run transcripts: what the agent said, the tools it called and
    /// what they returned (truncated).
    Logs {
        /// Keep following new output and new runs.
        #[arg(short, long)]
        follow: bool,
        /// Only this run (id or id prefix, from `wats agent runs`).
        #[arg(long)]
        run: Option<String>,
        /// How many of the latest runs to show.
        #[arg(short = 'n', long, default_value_t = 5)]
        last: usize,
    },
    /// List the runs: when, why, how long, outcome, turns and cost.
    Runs {
        /// How many of the latest runs to list.
        #[arg(short = 'n', long, default_value_t = 20)]
        last: usize,
        /// One JSON object per line instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Follow the run in progress live, then the next ones (waits for the
    /// next run when none is in progress).
    Attach,
}

#[derive(Debug, Subcommand)]
enum SetupTool {
    /// Claude Code. With neither --mcp nor --agent, does both.
    Claude {
        /// Register wats as an MCP server in Claude Code: `claude mcp add
        /// --scope <scope> wats -- <this wats> mcp --profile <profile>`.
        /// The profile is --profile, else config.toml's, else your OS user
        /// name.
        #[arg(long)]
        mcp: bool,
        /// Enable the Claude agent runner in config.toml (agent.runtime =
        /// "claude"): a bare `wats` then runs it.
        #[arg(long)]
        agent: bool,
        /// Who the agent is and what it can do (agent.about): its role in
        /// every run, and the text of the announce it posts.
        #[arg(long)]
        about: Option<String>,
        /// Claude Code scope of the MCP server: user (every project) or
        /// project (.mcp.json in the current directory).
        #[arg(long, default_value = "user", value_parser = ["user", "project"])]
        scope: String,
        /// The Claude Code CLI.
        #[arg(long, env = "WATS_CLAUDE", default_value = "claude")]
        claude: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print a key's value (the default when it is not set).
    Get { key: String },
    /// Set a key, e.g. `wats config set agent.heartbeat 2h`. Lists are
    /// comma-separated.
    Set { key: String, value: String },
    /// Remove a key: its default applies again.
    Unset { key: String },
    /// List every key, its value and what it does.
    List,
}

/// The OS user name as a profile name: lowercase, `[a-z0-9_-]`, at most 50
/// chars (room for a suffix such as `-agent`).
fn os_user_profile() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
        .to_lowercase();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name = cleaned.trim_matches('-').to_string();
    name.truncate(50);
    if name.is_empty() { "user".into() } else { name }
}

/// The profile wats is registered with in agent tools: --profile, else
/// config.toml's, else the OS user name.
fn human_profile(args: &NodeArgs, settings: &Settings) -> Result<String> {
    let profile = args
        .profile
        .clone()
        .or_else(|| settings.profile.clone())
        .unwrap_or_else(os_user_profile);
    anyhow::ensure!(
        wats::node::is_valid_profile(&profile),
        "profile name must be 1-64 chars of [a-z0-9_-]"
    );
    Ok(profile)
}

/// The `wats mcp` command line an MCP host should run: this binary, the
/// data directory when given, and the profile.
fn mcp_server_command(args: &NodeArgs, profile: &str) -> Result<(String, Vec<String>)> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let mut server_args = Vec::new();
    if let Some(dir) = &args.dir {
        server_args.push("--dir".into());
        server_args.push(std::path::absolute(dir)?.to_string_lossy().into_owned());
    }
    server_args.extend(["mcp".into(), "--profile".into(), profile.to_string()]);
    Ok((exe.to_string_lossy().into_owned(), server_args))
}

/// Whether Claude Code's user config already has exactly this `wats`
/// server (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`).
fn claude_has_server(command: &str, args: &[String]) -> bool {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(std::env::home_dir);
    let Some(dir) = dir else { return false };
    let Ok(text) = std::fs::read_to_string(dir.join(".claude.json")) else {
        return false;
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let server = &config["mcpServers"]["wats"];
    server["command"] == command
        && server["args"] == json!(args)
        && server["env"].as_object().is_none_or(|env| env.is_empty())
}

/// Register wats as the `wats` MCP server of Claude Code, idempotently:
/// nothing happens when the user config already has the same entry;
/// otherwise `claude mcp remove`, then `claude mcp add`. Returns whether it
/// changed something.
fn register_in_claude(
    args: &NodeArgs,
    profile: &str,
    scope: &str,
    claude: &std::path::Path,
) -> Result<bool> {
    let (command, server_args) = mcp_server_command(args, profile)?;
    if scope == "user" && claude_has_server(&command, &server_args) {
        return Ok(false);
    }
    let _ = std::process::Command::new(claude)
        .args(["mcp", "remove", "--scope", scope, "wats"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let status = std::process::Command::new(claude)
        .args(["mcp", "add", "--scope", scope, "wats", "--", &command])
        .args(&server_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .with_context(|| format!("run {}", claude.display()))?;
    anyhow::ensure!(status.success(), "`claude mcp add` failed ({status})");
    Ok(true)
}

/// `wats setup claude`.
fn setup_claude(
    args: &NodeArgs,
    data_dir: &std::path::Path,
    (mcp, agent): (bool, bool),
    about: Option<&str>,
    scope: &str,
    claude: &std::path::Path,
) -> Result<()> {
    let (mcp, agent) = if mcp || agent {
        (mcp, agent)
    } else {
        (true, true)
    };
    let settings = Settings::load(data_dir)?;
    if mcp {
        let profile = human_profile(args, &settings)?;
        let changed = register_in_claude(args, &profile, scope, claude)?;
        println!(
            "Claude Code: wats MCP server {} (scope {scope}, profile {profile})",
            if changed {
                "registered"
            } else {
                "already registered"
            }
        );
    }
    if agent || about.is_some() {
        let mut file = ConfigFile::open(data_dir)?;
        if agent {
            file.set("agent.runtime", "claude")?;
        }
        if let Some(about) = about {
            file.set("agent.about", about)?;
        }
        file.save()?;
        if agent {
            println!(
                "{}: agent.runtime = \"claude\"; `wats` now runs the node with a Claude agent",
                file.path().display()
            );
        }
        if file.get("agent.about").is_none() {
            println!("Describe its role: wats config set agent.about \"I review Rust code and …\"");
        }
    }
    Ok(())
}

/// `wats config get | set | unset | list`.
fn config_command(data_dir: &std::path::Path, action: ConfigAction) -> Result<()> {
    let mut file = ConfigFile::open(data_dir)?;
    match action {
        ConfigAction::Get { key } => {
            let key = wats::settings::find_key(&key)?;
            match file.get(key.name) {
                Some(value) => println!("{value}"),
                None => println!("{} (default)", key.default),
            }
        }
        ConfigAction::Set { key, value } => {
            file.set(&key, &value)?;
            file.save()?;
        }
        ConfigAction::Unset { key } => {
            if file.unset(&key)? {
                file.save()?;
            }
        }
        ConfigAction::List => {
            println!("# {}", file.path().display());
            for key in wats::settings::KEYS {
                let value = match file.get(key.name) {
                    Some(value) => value,
                    None => format!("{} (default)", key.default),
                };
                println!("{:<26} {:<28} # {}", key.name, value, key.help);
            }
        }
    }
    Ok(())
}

use wats::settings::{ConfigFile, Settings, parse_duration, parse_size};

fn print_id(config: &NodeConfig, host: &str) -> Result<()> {
    let (root, path, existed) = match &config.secret_file {
        Some(file) => (wats::identity::RootSecret::load(file)?, file.clone(), true),
        None => {
            let path = config.data_dir.join("secret.key");
            let existed = path.exists();
            (
                wats::identity::RootSecret::load_or_create(&path)?,
                path,
                existed,
            )
        }
    };
    if !existed {
        eprintln!("created a new node secret at {}", path.display());
    }
    let endpoint = iroh::SecretKey::from_bytes(&root.transport_key_bytes()).public();
    let author = wats::identity::author_id(&root.profile_key(&config.profile));
    let port = config
        .port
        .unwrap_or(wats::bootstrap::DEFAULT_BOOTSTRAP_PORT);
    println!("endpoint_id: {endpoint}");
    println!("author_id: {author} (profile {})", config.profile);
    println!("bootstrap_entry: {endpoint}@{host}:{port}");
    Ok(())
}

fn default_dir() -> Result<PathBuf> {
    std::env::home_dir()
        .map(|h| h.join(".wats"))
        .context("no home directory; pass --dir")
}

impl NodeArgs {
    /// Treat empty values (e.g. `WATS_EXTERNAL_ADDR=` in a compose or env
    /// file) as unset.
    fn normalize(&mut self) {
        for list in [
            &mut self.peers,
            &mut self.nets,
            &mut self.bootstrap,
            &mut self.external_addrs,
        ] {
            list.retain(|v| !v.trim().is_empty());
        }
        if self.secret_file.as_ref().is_some_and(|p| p.is_empty()) {
            self.secret_file = None;
        }
        if self.infra.as_ref().is_some_and(|p| p.trim().is_empty()) {
            self.infra = None;
        }
        if self.profile.as_ref().is_some_and(|p| p.is_empty()) {
            self.profile = None;
        }
    }

    fn data_dir(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_dir(),
        }
    }

    /// The node configuration: flags and WATS_* variables, then
    /// config.toml, then defaults.
    fn config(&self, file: &Settings) -> Result<NodeConfig> {
        let mut config = NodeConfig::new(self.data_dir()?);
        let no_default_bootstrap =
            self.no_default_bootstrap || file.no_default_bootstrap.unwrap_or(false);
        if self.loopback {
            config.mode = NetworkMode::Loopback;
        } else if !no_default_bootstrap {
            config.bootstrap = wats::bootstrap::defaults();
        }
        let bootstrap = if self.bootstrap.is_empty() {
            &file.bootstrap
        } else {
            &self.bootstrap
        };
        for entry in bootstrap {
            config.bootstrap.push(
                entry
                    .parse()
                    .with_context(|| format!("invalid bootstrap entry {entry:?}"))?,
            );
        }
        config.port = self.port.or(file.port);
        if let Some(infra) = self.infra.as_ref().or(file.infra.as_ref()) {
            config.infra = infra.parse()?;
        }
        config.secret_file = self.secret_file.as_ref().map(PathBuf::from);
        let file_size = |value: &Option<String>, what: &str| -> Result<Option<u64>> {
            value
                .as_deref()
                .map(parse_size)
                .transpose()
                .map_err(|e| anyhow::anyhow!("config.toml {what}: {e}"))
        };
        if let Some(size) = self
            .max_blob_size
            .or(file_size(&file.max_blob_size, "max_blob_size")?)
        {
            config.max_blob_bytes = size;
        }
        config.blob_quota = self
            .blob_quota
            .or(file_size(&file.blob_quota, "blob_quota")?);
        for addr in &self.external_addrs {
            config.external_addrs.push(
                addr.parse()
                    .with_context(|| format!("invalid external address {addr:?}"))?,
            );
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
        if let Some(profile) = self.profile.as_ref().or(file.profile.as_ref()) {
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
        for entry in &self.bootstrap {
            args.push(format!("--bootstrap={entry}").into());
        }
        if self.no_default_bootstrap {
            args.push("--no-default-bootstrap".into());
        }
        if let Some(port) = self.port {
            args.push(format!("--port={port}").into());
        }
        if let Some(infra) = &self.infra {
            args.push(format!("--infra={infra}").into());
        }
        if let Some(size) = self.max_blob_size {
            args.push(format!("--max-blob-size={size}").into());
        }
        if let Some(quota) = self.blob_quota {
            args.push(format!("--blob-quota={quota}").into());
        }
        if let Some(file) = &self.secret_file {
            let mut arg = std::ffi::OsString::from("--secret-file=");
            arg.push(file);
            args.push(arg);
        }
        for addr in &self.external_addrs {
            args.push(format!("--external-addr={addr}").into());
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
        // A node started for a session never runs the agents of config.toml.
        .args([
            "node",
            "--quiet",
            "--no-agent",
            "--idle-exit",
            &idle_exit.to_string(),
        ])
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

type Shutdown = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Resolves on Ctrl-C, or SIGTERM on Unix (what Kubernetes, Docker and
/// systemd send). Handlers are registered right away: a process running as
/// PID 1 in a container ignores signals it has no handler for.
fn shutdown_signal() -> Shutdown {
    #[cfg(unix)]
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let ctrl_c = tokio::signal::ctrl_c();
    Box::pin(async move {
        #[cfg(unix)]
        if let Some(mut term) = term {
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
            return;
        }
        let _ = ctrl_c.await;
    })
}

fn print_line(value: serde_json::Value) {
    println!("{value}");
}

struct NodeRun {
    post: Option<String>,
    title: Option<String>,
    wait_peers: usize,
    wait_timeout: u64,
    kind: String,
    tags: Vec<String>,
    blobs: Vec<PathBuf>,
    /// Print what arrives as JSON lines on stdout (`wats node`).
    json_lines: bool,
    idle_exit: Option<u64>,
    agent: Option<wats::agent::AgentConfig>,
}

async fn run_node(node: Arc<Node>, run: NodeRun, mut shutdown: Shutdown) -> Result<()> {
    let NodeRun {
        post,
        title,
        wait_peers,
        wait_timeout,
        kind,
        tags,
        blobs,
        json_lines,
        idle_exit,
        agent,
    } = run;
    let quiet = !json_lines;
    let print_line = move |value: serde_json::Value| {
        if !quiet {
            print_line(value);
        }
    };
    let sessions = attach::serve(Arc::clone(&node)).unwrap_or_else(|e| {
        tracing::warn!("MCP sessions cannot attach to this node: {e:#}");
        attach::Sessions::default()
    });
    let mut events = node.store().subscribe();
    let mut fetched = node.subscribe_fetched();
    let mut idle_since = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let (stop_agent, agent_stopped) = tokio::sync::watch::channel(false);
    let agent = agent.map(|config| {
        let mut stopped = agent_stopped;
        tokio::spawn(wats::agent::run(Arc::clone(&node), config, async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        }))
    });
    if !quiet {
        print_line(json!({
            "type": "ready",
            "endpoint": node.endpoint_id().to_string(),
            "author": node.author_id(),
            "ticket": node.ticket().await.to_string(),
        }));
    }

    if let Some(text) = post {
        let node = Arc::clone(&node);
        let kind: Kind = kind.parse()?;
        tokio::spawn(async move {
            let net = node.config().nets[0].clone();
            let result = async {
                let wait = Duration::from_secs(wait_timeout);
                node.wait_for_neighbor(&net, wait).await?;
                let deadline = tokio::time::Instant::now() + wait;
                while node.known_peers().len() < wait_peers {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!(
                            known = node.known_peers().len(),
                            "posting before {wait_peers} peers are known"
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                let attachments: Vec<Attachment> = blobs
                    .into_iter()
                    .map(|path| Attachment { path, name: None })
                    .collect();
                let draft = Draft {
                    net,
                    kind: Some(kind),
                    tags,
                    title: Some(title.unwrap_or_else(|| wats::event::title_from_text(&text))),
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
            _ = &mut shutdown => break,
        }
    }
    if let Some(agent) = agent {
        let _ = stop_agent.send(true);
        match agent.await {
            Ok(Err(e)) => tracing::warn!("agent runner: {e:#}"),
            Err(e) => tracing::warn!("agent runner: {e}"),
            Ok(Ok(())) => {}
        }
    }
    let _ = std::fs::remove_file(attach::socket_path(&node.config().data_dir));
    node.shutdown().await
}

/// The agent runner configuration: flags, then config.toml, then defaults.
/// `None` when no agent is enabled.
fn agent_config(
    node_args: &NodeArgs,
    args: &AgentArgs,
    file: &wats::settings::AgentSettings,
    data_dir: &std::path::Path,
) -> Result<Option<wats::agent::AgentConfig>> {
    let runtimes = if args.no_agent {
        Vec::new()
    } else if !args.agents.is_empty() {
        args.agents.clone()
    } else {
        file.runtime.iter().cloned().collect()
    };
    // One runtime today (claude).
    let Some(runtime) = runtimes.first().cloned() else {
        return Ok(None);
    };
    let file_duration = |value: &Option<String>, what: &str| -> Result<Option<Duration>> {
        value
            .as_deref()
            .map(parse_duration)
            .transpose()
            .map_err(|e| anyhow::anyhow!("config.toml agent.{what}: {e}"))
    };
    let profile = args
        .agent_profile
        .clone()
        .or(file.profile.clone())
        .unwrap_or_else(|| format!("{}-agent", os_user_profile()));
    anyhow::ensure!(
        wats::node::is_valid_profile(&profile),
        "agent profile must be 1-64 chars of [a-z0-9_-]"
    );
    let allow = if args.allow.is_empty() {
        file.allow.clone()
    } else {
        args.allow.clone()
    };
    for key in &allow {
        wats::event::parse_hex::<32>(key, "allowed author id")?;
    }
    let heartbeat_text = match args.heartbeat_file.clone().or(file.heartbeat_file.clone()) {
        Some(path) => {
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        None => wats::agent::DEFAULT_HEARTBEAT.to_string(),
    };
    let mut extra_args = match &args.agent_args {
        Some(line) => line.split_whitespace().map(str::to_string).collect(),
        None => file.args.clone(),
    };
    extra_args.extend(args.extra.iter().cloned());
    // The agent's `wats mcp` attaches to this node through its socket.
    let mut mcp_args: Vec<String> = node_args
        .to_args(data_dir)
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    mcp_args.extend(["mcp".into(), "--profile".into(), profile.clone()]);
    Ok(Some(wats::agent::AgentConfig {
        runtime,
        profile,
        user: std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "this".into()),
        about: file.about.clone().unwrap_or_default(),
        caps: file.caps.clone(),
        every: args
            .every
            .or(file_duration(&file.every, "every")?)
            .unwrap_or(Duration::from_secs(30)),
        heartbeat: args
            .heartbeat
            .or(file_duration(&file.heartbeat, "heartbeat")?)
            .unwrap_or(Duration::from_secs(30 * 60)),
        max_runs_per_hour: args
            .max_runs_per_hour
            .or(file.max_runs_per_hour)
            .unwrap_or(12),
        allow,
        heartbeat_text,
        command: args
            .agent_command
            .clone()
            .or(file.command.clone())
            .unwrap_or_else(|| "claude".into()),
        mcp_command: std::env::current_exe()?,
        mcp_args,
        extra_args,
        run_timeout: args
            .run_timeout
            .or(file_duration(&file.timeout, "timeout")?)
            .unwrap_or(Duration::from_secs(10 * 60)),
        work_dir: data_dir.join("agent"),
        max_turns: file.max_turns.unwrap_or(10),
        budget: budget_of(file),
        keep_runs: file.keep_runs.unwrap_or(200),
        keep_days: file.keep_days.unwrap_or(30),
    }))
}

/// `30s`, `5m`, `2h`, `90s`: the largest unit that divides evenly.
fn short_duration(d: Duration) -> String {
    match d.as_secs() {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The spend budget from config.toml, or the defaults.
fn budget_of(file: &wats::settings::AgentSettings) -> wats::budget::Budget {
    let default = wats::budget::Budget::default();
    wats::budget::Budget {
        max_usd_per_day: file.max_usd_per_day.unwrap_or(default.max_usd_per_day),
        max_usd_per_month: file.max_usd_per_month.unwrap_or(default.max_usd_per_month),
        max_runs_per_day: file.max_runs_per_day.unwrap_or(default.max_runs_per_day),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a foreground node prints on start (stderr: stdout may carry JSON).
fn print_banner(config: &NodeConfig, agent: Option<&wats::agent::AgentConfig>) {
    let home = std::env::home_dir();
    let dir = match home
        .as_deref()
        .and_then(|h| config.data_dir.strip_prefix(h).ok())
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => config.data_dir.display().to_string(),
    };
    eprintln!(
        "wats {} · data {dir} · infra {} · profile {}",
        env!("CARGO_PKG_VERSION"),
        config.infra,
        config.profile
    );
    match agent {
        Some(agent) => eprintln!(
            "agent {}: on, posts as {} · wakes on asks and replies (every {} at most), heartbeat {} · watch it: wats agent logs -f",
            agent.runtime,
            agent.profile,
            short_duration(agent.every),
            short_duration(agent.heartbeat),
        ),
        None => eprintln!("agent: off (wats --agent claude, or wats setup claude)"),
    }
    if let Some(agent) = agent {
        let spend = wats::budget::spend(&agent.work_dir, unix_now());
        eprintln!(
            "agent budget: {} · {} turns and {} per run",
            wats::budget::describe(&agent.budget, &spend),
            agent.max_turns,
            short_duration(agent.run_timeout)
        );
    }
    eprintln!("Ctrl-C to stop · wats --help");
}

/// `wats agent logs | runs | attach`.
async fn agent_watch(data_dir: &std::path::Path, action: AgentAction) -> Result<()> {
    use std::io::IsTerminal;
    use wats::runs;
    let work_dir = data_dir.join("agent");
    let style = runs::Style {
        color: std::io::stdout().is_terminal(),
    };
    let mut out = std::io::stdout().lock();
    let result = match action {
        AgentAction::Runs { last, json } => {
            let all = runs::list(&work_dir);
            let shown = &all[all.len().saturating_sub(last)..];
            if json {
                shown.iter().try_for_each(|r| {
                    writeln!(out, "{}", serde_json::to_string(r).unwrap_or_default())
                })
            } else if shown.is_empty() {
                writeln!(out, "no agent runs yet in {}", work_dir.display())
            } else {
                let mut result = writeln!(
                    out,
                    "{:<32} {:<20} {:<10} {:>8} {:>6} {:>9}",
                    "RUN", "STARTED", "STATUS", "SECONDS", "TURNS", "COST"
                );
                for r in shown {
                    result = result.and_then(|()| {
                        writeln!(
                            out,
                            "{:<32} {:<20} {:<10} {:>8} {:>6} {:>9}",
                            r.id,
                            runs::format_time(r.started),
                            r.status,
                            r.seconds.map(|s| format!("{s:.1}")).unwrap_or_default(),
                            r.turns.map(|t| t.to_string()).unwrap_or_default(),
                            r.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_default(),
                        )
                    });
                }
                let cost: f64 = shown.iter().filter_map(|r| r.cost_usd).sum();
                // A config.toml that does not load must not hide the runs.
                let budget = Settings::load(data_dir)
                    .map(|s| budget_of(&s.agent))
                    .unwrap_or_default();
                let now = unix_now();
                let spend = wats::budget::spend(&work_dir, now);
                let paused = match wats::budget::check(&budget, &spend, now) {
                    wats::budget::Verdict::Paused { why, until } => {
                        format!("\npaused: {why}, {}", wats::budget::until(until))
                    }
                    wats::budget::Verdict::Go { .. } => String::new(),
                };
                result.and_then(|()| {
                    writeln!(
                        out,
                        "{} runs shown, ${cost:.4} reported\nbudget: {}{paused}",
                        shown.len(),
                        wats::budget::describe(&budget, &spend)
                    )
                })
            }
        }
        AgentAction::Logs { follow, run, last } => {
            let ids = match &run {
                Some(prefix) => vec![
                    runs::find(&work_dir, prefix)
                        .with_context(|| format!("no run {prefix:?} in {}", work_dir.display()))?,
                ],
                None => {
                    let all = runs::run_ids(&work_dir);
                    all[all.len().saturating_sub(last)..].to_vec()
                }
            };
            let printed = runs::print_runs(&work_dir, &ids, style, &mut out);
            if follow && printed.is_ok() {
                let start = ids
                    .last()
                    .map(|id| (id.clone(), runs::transcript_len(&work_dir, id)));
                runs::follow(&work_dir, start, style, &mut out).await
            } else {
                printed
            }
        }
        AgentAction::Attach => {
            let start = runs::current(&work_dir).map(|id| (id, 0));
            if start.is_none() {
                eprintln!("no run in progress; waiting for the next one (Ctrl-C to stop)");
            }
            runs::follow(&work_dir, start, style, &mut out).await
        }
    };
    match result {
        // `wats agent logs | head`
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

async fn run_mcp(node: Arc<Node>, mut shutdown: Shutdown) -> Result<()> {
    let sessions = attach::serve(Arc::clone(&node)).unwrap_or_else(|e| {
        tracing::warn!("MCP sessions cannot attach to this node: {e:#}");
        attach::Sessions::default()
    });
    let server = WatsMcp::new(Arc::clone(&node), default_file_roots());
    let service = server.serve(rmcp::transport::stdio()).await?;
    tokio::select! {
        result = service.waiting() => { result?; }
        _ = &mut shutdown => return node.shutdown().await,
    }
    // Other sessions attached to this node: keep serving them.
    while sessions.active() > 0 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            _ = &mut shutdown => break,
        }
    }
    let _ = std::fs::remove_file(attach::socket_path(&node.config().data_dir));
    node.shutdown().await
}

/// `RUST_LOG`, or `wats=info` by default. Unless `RUST_LOG` already
/// mentions Iroh's structured events, two of them are always shown: the
/// network path chosen for each peer (`path::selected`: an IP address means
/// a direct connection, a relay URL means relayed) and home relay changes.
/// They are what a NAT test needs to see.
fn log_filter() -> EnvFilter {
    let mut filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("wats=info"));
    if !std::env::var("RUST_LOG").is_ok_and(|v| v.contains("_events")) {
        for directive in [
            "iroh::_events::path::selected=debug",
            "iroh::_events::relay::home_changed=debug",
        ] {
            if let Ok(directive) = directive.parse() {
                filter = filter.add_directive(directive);
            }
        }
    }
    filter
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr: stdout is the protocol channel in `wats mcp`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(log_filter())
        .init();

    let shutdown = shutdown_signal();
    let mut cli = Cli::parse();
    cli.node.normalize();
    let data_dir = cli.node.data_dir()?;
    // Commands that work even when config.toml does not load.
    match cli.command {
        Some(Command::Config { action }) => return config_command(&data_dir, action),
        Some(Command::Agent { action }) => return agent_watch(&data_dir, action).await,
        Some(Command::Setup {
            tool:
                SetupTool::Claude {
                    mcp,
                    agent,
                    about,
                    scope,
                    claude,
                },
        }) => {
            return setup_claude(
                &cli.node,
                &data_dir,
                (mcp, agent),
                about.as_deref(),
                &scope,
                &claude,
            );
        }
        _ => {}
    }
    let settings = Settings::load(&data_dir)?;
    let config = cli.node.config(&settings)?;
    let (daemon, json_lines) = match cli.command {
        None => (cli.daemon, false),
        Some(Command::Node { daemon }) => (*daemon, true),
        Some(Command::Mcp {
            idle_exit,
            in_process: false,
        }) => return mcp_attached(&cli.node, &data_dir, idle_exit).await,
        Some(Command::Mcp { .. }) => return run_mcp(Node::start(config).await?, shutdown).await,
        Some(Command::Id { host }) => return print_id(&config, &host),
        Some(Command::Config { .. } | Command::Agent { .. } | Command::Setup { .. }) => {
            unreachable!("handled above")
        }
    };
    let agent = agent_config(&cli.node, &daemon.agent, &settings.agent, &data_dir)?;
    // `--agent claude` also makes sure Claude Code sees the swarm.
    if daemon.agent.agents.iter().any(|a| a == "claude")
        && let Some(agent) = &agent
    {
        let profile = human_profile(&cli.node, &settings)?;
        // Not fatal: the agent itself does not need it (e.g. in a container).
        match register_in_claude(&cli.node, &profile, "user", &agent.command) {
            Ok(true) => eprintln!(
                "wats registered in Claude Code (scope user, profile {profile}): \
                 new Claude Code sessions see the swarm"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not register wats in Claude Code: {e:#}"),
        }
    }
    if !daemon.quiet {
        print_banner(&config, agent.as_ref());
    }
    let node = Node::start(config).await?;
    let run = NodeRun {
        post: daemon.post,
        title: daemon.title,
        wait_peers: daemon.wait_peers,
        wait_timeout: daemon.wait_timeout,
        kind: daemon.kind,
        tags: daemon.tags,
        blobs: daemon.blobs,
        json_lines: json_lines && !daemon.quiet,
        idle_exit: daemon.idle_exit,
        agent,
    };
    run_node(node, run, shutdown).await
}
