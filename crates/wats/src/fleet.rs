//! A fleet: several nodes on one machine, started and checked together
//! (ADR 0028). `wats fleet up -n 6` gives a machine's spare CPUs to the
//! network as six support nodes.
//!
//! - **One data directory per node**, `<base>/node-<i>`: its own node
//!   secret (so its own endpoint id and author keys), its own agent
//!   workspace, its own `config.toml`, copied from a template data
//!   directory's. The agent runtime is the same command on every node, so
//!   the nodes' agents share the user's login (one Claude account): wats
//!   copies no credential, only `config.toml`.
//! - **Each node runs as `wats --detach`** (ADR 0023), one after the other.
//!   `wats ps`, `wats attach --dir <base>/node-<i>` and `wats stop` see
//!   them as any other node.
//! - **A mesh.** Each node listens on a port of its own, written in its
//!   `config.toml`, and lists the others as bootstrap peers on
//!   `127.0.0.1`, so the fleet connects even with `--infra none`, besides
//!   the template's bootstrap nodes.
//! - **`<base>/fleet.json`** lists the nodes; `check`, `stop` and `logs`
//!   read it.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::settings::{ConfigFile, InitialMax, Settings, parse_duration, parse_size};

/// The fleet's directory under the home directory, by default.
pub const DEFAULT_BASE: &str = ".wats-fleet";
/// The list of the fleet's nodes, in its base directory.
pub const MANIFEST_NAME: &str = "fleet.json";
/// Variables a node of the fleet must not inherit: the fleet sets these per
/// node (its directory, port and key), or writes them in its `config.toml`.
pub const ENV_REMOVE: [&str; 9] = [
    "WATS_DIR",
    "WATS_PORT",
    "WATS_SECRET_FILE",
    "WATS_BOOTSTRAP",
    "WATS_STORAGE_MAX",
    "WATS_SYNC_INITIAL_MAX",
    "WATS_EXTERNAL_ADDR",
    "WATS_PROFILE",
    "WATS_MCP_SOCKET",
];

/// `wats fleet`.
#[derive(Debug, clap::Args)]
pub struct FleetCommand {
    /// The fleet's directory: one data directory per node in it
    /// (`node-1`, `node-2`…) and `fleet.json` [default: ~/.wats-fleet].
    #[arg(long, global = true, env = "WATS_FLEET_BASE")]
    pub base: Option<PathBuf>,
    #[command(subcommand)]
    pub action: FleetAction,
}

#[derive(Debug, clap::Subcommand)]
pub enum FleetAction {
    /// Create the fleet's nodes that do not exist yet (a data directory
    /// each, its config.toml copied from --from), then start in the
    /// background (as `wats --detach`) those that do not run. Each node has
    /// its own key, port and agent workspace, and lists the others as
    /// bootstrap peers. The global options (e.g. --infra, --net) apply to
    /// every node and are kept for the next `up`.
    Up(UpArgs),
    /// Check every node of the fleet: it runs, answers, runs this wats
    /// version, has a neighbor and has met every other node of the fleet,
    /// its agents run and their command is found (no model call), and its
    /// log has no error in the last hour. A table, and a non-zero exit code
    /// when a node fails (for cron).
    Check(CheckArgs),
    /// Print a Docker Compose file for N nodes (0.4.12): one service per
    /// node, each with its own published UDP port, volume and limits, a
    /// read-only root, every capability dropped and no new privileges. It
    /// drives no Docker: `docker compose -f <file> up -d` does.
    Compose(ComposeArgs),
    /// Stop every node of the fleet (as `wats stop` does, in parallel).
    Stop {
        /// How long to wait for each node's graceful stop before killing
        /// it (e.g. 10s, 1m).
        #[arg(long, default_value = "10s", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// The last log lines of every node, each prefixed with its name; -f
    /// follows them. One node in full: `wats attach -n node-<i>` (or
    /// `-n <fleet>-<i>` for a second fleet, as `wats ps` shows it).
    Logs {
        /// How many of the last lines of each node's log.
        #[arg(short = 'n', long, default_value_t = 10)]
        lines: usize,
        /// Only this node (`node-2`, or `2`).
        #[arg(long)]
        node: Option<String>,
        /// Then print what the nodes log, live, until Ctrl-C.
        #[arg(short = 'f', long)]
        follow: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct ComposeArgs {
    /// How many nodes.
    #[arg(short = 'n', long, value_parser = clap::value_parser!(u16).range(1..=500))]
    pub nodes: u16,
    /// The first node's UDP port; node i gets port-base + i - 1.
    #[arg(long, default_value_t = 7850)]
    pub port_base: u16,
    /// The services' names: `<name>-1`, `<name>-2`… (also the project's).
    #[arg(long, default_value = "wats")]
    pub name: String,
    /// The image [default: ghcr.io/wearetheswarm/wats:<this version>].
    #[arg(long)]
    pub image: Option<String>,
    /// The command each node runs, after `wats` (e.g. "--agent opencode").
    #[arg(long, default_value = "node --quiet", allow_hyphen_values = true)]
    pub command: String,
    /// CPUs per node.
    #[arg(long, default_value = "1.0")]
    pub cpus: String,
    /// Memory per node.
    #[arg(long, default_value = "2G")]
    pub memory: String,
    /// Processes per node.
    #[arg(long, default_value_t = 512)]
    pub pids: u32,
    /// Infrastructure nodes (routers, seeds): 8 gossip neighbors, a
    /// 1-minute anti-entropy and ping, and `restart: unless-stopped` with
    /// `WATS_HALT_SERVE=true` (they survive a host reboot, and a guardian
    /// halt keeps them serving instead of stopping). Otherwise `restart:
    /// on-failure`: a node a halt stops exits 0 and stays stopped.
    #[arg(long)]
    pub infra_node: bool,
    /// More environment for every node (`KEY=value`, repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
}

/// A quoted YAML scalar: JSON's string syntax is valid YAML.
fn yaml(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The Compose file of `wats fleet compose` (0.4.12).
pub fn compose_file(args: &ComposeArgs) -> Result<String> {
    use std::fmt::Write as _;
    anyhow::ensure!(
        !args.name.is_empty()
            && args
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "--name: lowercase letters, digits and '-' only (a Compose project name)"
    );
    let last = u32::from(args.port_base) + u32::from(args.nodes) - 1;
    anyhow::ensure!(
        last <= u32::from(u16::MAX),
        "--port-base {} with {} nodes goes past port 65535",
        args.port_base,
        args.nodes
    );
    let mut env: Vec<(String, String)> = Vec::new();
    if args.infra_node {
        for (k, v) in [
            ("WATS_GOSSIP_ACTIVE_VIEW", "8"),
            ("WATS_SYNC_ANTI_ENTROPY", "1m"),
            ("WATS_GOSSIP_PING", "1m"),
            ("WATS_HALT_SERVE", "true"),
        ] {
            env.push((k.into(), v.into()));
        }
    }
    for kv in &args.env {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--env {kv}: KEY=value"))?;
        anyhow::ensure!(
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "--env {kv}: the key is letters, digits and '_'"
        );
        anyhow::ensure!(
            k != "WATS_PORT",
            "--env WATS_PORT: each node has its own port"
        );
        env.push((k.into(), v.into()));
    }
    let image = args
        .image
        .clone()
        .unwrap_or_else(|| format!("ghcr.io/wearetheswarm/wats:{}", env!("CARGO_PKG_VERSION")));
    let command: Vec<String> = args.command.split_whitespace().map(yaml).collect();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# {} wats nodes, from `wats fleet compose` (wats {}): one service per node, its own\n\
         # UDP port and volume. Start: docker compose -f <this file> up -d",
        args.nodes,
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(out, "name: {}\n", yaml(&args.name));
    let _ = writeln!(out, "x-node: &node");
    let _ = writeln!(out, "  image: {}", yaml(&image));
    // The release image is linux/amd64 only: emulated on an arm64 host.
    if args.image.is_none() {
        let _ = writeln!(out, "  platform: linux/amd64");
    }
    let _ = writeln!(out, "  command: [{}]", command.join(", "));
    // A halt stops a node with status 0 (ADR 0016): on-failure leaves it
    // stopped. Infrastructure nodes serve through a halt instead, and
    // must come back after a reboot.
    let restart = match args.infra_node {
        true => "unless-stopped",
        false => "on-failure",
    };
    let _ = writeln!(out, "  restart: {restart}");
    let _ = writeln!(out, "  read_only: true");
    let _ = writeln!(out, "  tmpfs: [\"/tmp:size=256m\"]");
    let _ = writeln!(out, "  cap_drop: [ALL]");
    let _ = writeln!(out, "  security_opt: [\"no-new-privileges:true\"]");
    let _ = writeln!(out, "  stop_grace_period: 30s");
    let _ = writeln!(out, "  deploy:\n    resources:\n      limits:");
    let _ = writeln!(out, "        cpus: {}", yaml(&args.cpus));
    let _ = writeln!(out, "        memory: {}", yaml(&args.memory));
    let _ = writeln!(out, "        pids: {}", args.pids);
    let _ = writeln!(
        out,
        "  logging:\n    driver: json-file\n    options:\n      max-size: 10m\n      max-file: \"3\""
    );
    let _ = writeln!(out, "\nservices:");
    for i in 1..=u32::from(args.nodes) {
        let port = u32::from(args.port_base) + i - 1;
        let _ = writeln!(out, "  {}-{i}:", args.name);
        let _ = writeln!(out, "    <<: *node");
        let _ = writeln!(out, "    environment:");
        let _ = writeln!(out, "      WATS_PORT: \"{port}\"");
        for (k, v) in &env {
            let _ = writeln!(out, "      {k}: {}", yaml(v));
        }
        let _ = writeln!(out, "    ports: [\"{port}:{port}/udp\"]");
        let _ = writeln!(out, "    volumes: [\"{}-{i}:/data\"]", args.name);
    }
    let _ = writeln!(out, "\nvolumes:");
    for i in 1..=args.nodes {
        let _ = writeln!(out, "  {}-{i}:", args.name);
    }
    Ok(out)
}

#[derive(Debug, clap::Args)]
pub struct UpArgs {
    /// How many nodes [default: the fleet's current count]. More than the
    /// fleet has adds nodes; fewer is refused.
    #[arg(short = 'n', long)]
    pub nodes: Option<usize>,
    /// The data directory whose config.toml new nodes copy (agents
    /// included; never its key or credentials) [default: --dir, else
    /// ~/.wats].
    #[arg(long, value_name = "DATA_DIR")]
    pub from: Option<PathBuf>,
    /// Disk budget of each node (storage.max), e.g. 20G or 100G, written in
    /// every node's config.toml.
    #[arg(long, value_parser = parse_size)]
    pub storage: Option<u64>,
    /// Full-history nodes: sync.initial_max = 0 in every node's
    /// config.toml (durable support nodes; give them --storage too).
    #[arg(long)]
    pub full: bool,
    /// Run the nodes without their agents (storage and relaying only);
    /// kept for the next `up`, `--no-agent=false` undoes it.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_agent: Option<bool>,
    /// UDP ports of new nodes: this one for node-1, the next for node-2…
    /// (for a firewall rule) [default: free ports, chosen once].
    #[arg(long)]
    pub port_base: Option<u16>,
    /// Pause between two node starts (e.g. 2s, 0s).
    #[arg(long, default_value = "2s", value_parser = parse_pause)]
    pub stagger: Duration,
    /// Then run `wats fleet check`, waiting up to --wait for the mesh.
    #[arg(long)]
    pub check: bool,
    /// With --check: how long to wait for every node to pass.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub wait: Duration,
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Check again every second until every node passes, for at most this
    /// long (e.g. 30s) [default: once].
    #[arg(long, value_parser = parse_duration)]
    pub wait: Option<Duration>,
    /// Log lines this recent count (e.g. 1h, 10m); never those before the
    /// node's start.
    #[arg(long, default_value = "1h", value_parser = parse_duration)]
    pub since: Duration,
    /// Warnings in the log fail the check too, not only errors.
    #[arg(long)]
    pub strict: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// What `wats fleet` needs from the global options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The global options for every node's command line (network, proof of
    /// work…), without --dir, --port, --bootstrap, --secret-file, the
    /// storage ones and --hermetic.
    pub node_args: Vec<String>,
    /// Whether any network option was given, so that `up` replaces the
    /// ones kept in `fleet.json`.
    pub node_args_given: bool,
    /// --bootstrap entries: added to every node's `bootstrap`.
    pub bootstrap: Vec<String>,
    /// The data directory of --dir, else `~/.wats`: the default template.
    pub data_dir: PathBuf,
    /// --storage-max, as a default for `up --storage`.
    pub storage_max: Option<u64>,
    /// --sync-initial-max, as a default for `up --full`.
    pub initial_max: Option<InitialMax>,
    pub hermetic: bool,
}

/// `<base>/fleet.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// When the fleet was created (`YYYY-MM-DD HH:MM:SSZ`).
    pub created: String,
    /// The data directory new nodes copy `config.toml` from.
    pub template: PathBuf,
    /// The global options every node runs with.
    #[serde(default)]
    pub args: Vec<String>,
    /// The nodes run without their agents.
    #[serde(default)]
    pub no_agent: bool,
    pub nodes: Vec<ManifestNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestNode {
    /// `node-<i>`, also its directory in the base.
    pub name: String,
    pub endpoint_id: String,
    pub port: u16,
}

impl Manifest {
    pub fn load(base: &Path) -> Result<Option<Manifest>> {
        let path = base.join(MANIFEST_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("invalid {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save(&self, base: &Path) -> Result<()> {
        let path = base.join(MANIFEST_NAME);
        crate::background::write_file(&path, &(serde_json::to_string_pretty(self)? + "\n"))
            .with_context(|| format!("write {}", path.display()))
    }

    /// The ids of the nodes other than `name`.
    fn others(&self, name: &str) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|n| n.name != name)
            .map(|n| n.endpoint_id.clone())
            .collect()
    }
}

/// A pause: a duration, or 0 (`0`, `0s`) for none.
fn parse_pause(s: &str) -> Result<Duration, String> {
    let digits = s.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic());
    if !digits.is_empty() && digits.bytes().all(|b| b == b'0') {
        return Ok(Duration::ZERO);
    }
    parse_duration(s)
}

/// The name of node `i` (from 1).
pub fn node_name(i: usize) -> String {
    format!("node-{i}")
}

/// The fleet's directory: `--base`, else `~/.wats-fleet`.
pub fn base_dir(base: Option<&Path>) -> Result<PathBuf> {
    let base = match base {
        Some(base) => base.to_path_buf(),
        None => std::env::home_dir()
            .context("no home directory: pass --base")?
            .join(DEFAULT_BASE),
    };
    Ok(std::path::absolute(&base).unwrap_or(base))
}

/// `dir` with `~` for the home directory, for display.
fn tilde(dir: &Path) -> String {
    match std::env::home_dir()
        .as_deref()
        .and_then(|h| dir.strip_prefix(h).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// `--base <base>` to name it in a command, unless it is the default.
fn base_flag(base: &Path) -> String {
    match base_dir(None) {
        Ok(default) if default == base => String::new(),
        _ => format!(" --base {}", tilde(base)),
    }
}

/// `wats fleet …`.
pub async fn run(
    ctx: Options,
    command: FleetCommand,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let base = base_dir(command.base.as_deref())?;
    match command.action {
        FleetAction::Compose(args) => {
            print!("{}", compose_file(&args)?);
            Ok(())
        }
        FleetAction::Up(args) => up(&ctx, &base, args).await,
        FleetAction::Check(args) => {
            let manifest = existing(&base)?;
            check_command(&base, &manifest, &args).await
        }
        FleetAction::Stop { timeout } => {
            let manifest = existing(&base)?;
            stop(&ctx, &base, &manifest, timeout)
        }
        FleetAction::Logs {
            lines,
            node,
            follow,
        } => {
            let manifest = existing(&base)?;
            logs(&base, &manifest, lines, node.as_deref(), follow, shutdown).await
        }
    }
}

fn existing(base: &Path) -> Result<Manifest> {
    Manifest::load(base)?.with_context(|| {
        format!(
            "no fleet in {} (no {MANIFEST_NAME}): create one with `wats fleet up -n <N>{}`",
            tilde(base),
            base_flag(base)
        )
    })
}

/// Available bytes on the file system of `path` (or of its nearest
/// existing parent), from `df`.
fn free_bytes(path: &Path) -> Option<u64> {
    let mut dir = path;
    while !dir.exists() {
        dir = dir.parent()?;
    }
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// Warnings about running `n` nodes of `storage` bytes each here.
pub fn resource_warnings(n: usize, cpus: usize, storage: u64, free: Option<u64>) -> Vec<String> {
    let mut warnings = Vec::new();
    let spare = cpus.saturating_sub(1).max(1);
    if n > spare {
        warnings.push(format!(
            "{n} nodes on {cpus} CPU(s): more than one per CPU but one ({spare}); their agents' \
             runs and syncs will compete"
        ));
    }
    if let Some(free) = free {
        let needed = storage.saturating_mul(n as u64);
        if free < needed {
            let each = (free / n.max(1) as u64) / (1024 * 1024 * 1024);
            warnings.push(format!(
                "{n} nodes × {} = {} but {} free on this disk: give each less with --storage {}G",
                crate::node::format_bytes(storage),
                crate::node::format_bytes(needed),
                crate::node::format_bytes(free),
                each.max(1)
            ));
        }
    }
    warnings
}

/// A free UDP port, not in `taken`.
fn free_port(taken: &HashSet<u16>, held: &mut Vec<std::net::UdpSocket>) -> Result<u16> {
    for _ in 0..64 {
        let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
            .context("find a free UDP port")?;
        let port = socket.local_addr()?.port();
        // Held until every port is chosen, so that two nodes never get the
        // same one.
        held.push(socket);
        if !taken.contains(&port) {
            return Ok(port);
        }
    }
    bail!("no free UDP port found")
}

/// The template's `config.toml` text, checked; empty when it has none.
fn template_text(template: &Path) -> Result<String> {
    anyhow::ensure!(
        template.is_dir(),
        "the template {} does not exist: --from names a data directory (its config.toml is \
         copied)",
        tilde(template)
    );
    Settings::load(template).with_context(|| format!("the template {}", tilde(template)))?;
    let path = template.join(crate::settings::FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "note: no config.toml in {}: the nodes run with the defaults, without an agent",
                tilde(template)
            );
            Ok(String::new())
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// A new node's `config.toml`: the template's, with each agent's paths
/// made right for another data directory. Returns notes for the user.
fn write_new_config(dir: &Path, template: &Path, text: &str) -> Result<Vec<String>> {
    let path = dir.join(crate::settings::FILE_NAME);
    crate::background::write_file(&path, text)
        .with_context(|| format!("write {}", path.display()))?;
    let mut notes = Vec::new();
    let settings = Settings::parse(text)?;
    let mut file = ConfigFile::open(dir)?;
    // The template's name is its own: each node has its own (ADR 0023 §8).
    file.unset("node.name")?;
    let tables = std::iter::once(("agent".to_string(), &settings.agent)).chain(
        settings
            .agents
            .iter()
            .map(|(name, table)| (format!("agents.{name}"), table)),
    );
    for (prefix, table) in tables {
        // An absolute workdir would be one workspace for every node: each
        // node gets its own, the default one.
        if table.workdir.as_ref().is_some_and(|w| w.is_absolute()) {
            file.unset(&format!("{prefix}.workdir"))?;
            notes.push(format!(
                "{prefix}.workdir is one directory: each node uses its own \
                 (<node>/agent/<name>/workspace) instead"
            ));
        }
        // A relative heartbeat file is the template's.
        if let Some(heartbeat) = table.heartbeat_file.as_ref().filter(|h| h.is_relative()) {
            let absolute = std::path::absolute(template.join(heartbeat))?;
            file.set(
                &format!("{prefix}.heartbeat_file"),
                &absolute.to_string_lossy(),
            )?;
        }
    }
    file.save()?;
    Ok(notes)
}

/// Set the keys the fleet manages in a node's `config.toml`: its port, its
/// bootstrap peers (the others, then the template's and --bootstrap), and
/// --storage and --full.
fn write_fleet_keys(
    dir: &Path,
    port: Option<u16>,
    fleet_ids: &HashSet<String>,
    peers: &[String],
    extra: &[String],
    storage: Option<u64>,
    full: Option<bool>,
) -> Result<()> {
    let settings = Settings::load(dir)?;
    let mut file = ConfigFile::open(dir)?;
    if let Some(port) = port {
        file.set("port", &port.to_string())?;
    }
    let mut bootstrap: Vec<String> = peers.to_vec();
    for entry in settings.bootstrap.iter().chain(extra) {
        let id = entry.split('@').next().unwrap_or_default().trim();
        if !fleet_ids.contains(id) && !bootstrap.contains(entry) {
            bootstrap.push(entry.clone());
        }
    }
    for entry in &bootstrap {
        entry
            .parse::<crate::bootstrap::BootstrapEntry>()
            .with_context(|| format!("bootstrap entry {entry:?}"))?;
    }
    file.set("bootstrap", &bootstrap.join(","))?;
    if let Some(storage) = storage {
        file.set("storage.max", &size_text(storage))?;
    }
    match full {
        Some(true) => file.set("sync.initial_max", "0")?,
        Some(false) => {
            file.unset("sync.initial_max")?;
        }
        None => {}
    }
    file.save()
}

/// A size as config.toml writes it: `100G`, `512M`, else bytes.
pub fn size_text(bytes: u64) -> String {
    const G: u64 = 1 << 30;
    const M: u64 = 1 << 20;
    if bytes > 0 && bytes.is_multiple_of(G) {
        format!("{}G", bytes / G)
    } else if bytes > 0 && bytes.is_multiple_of(M) {
        format!("{}M", bytes / M)
    } else {
        bytes.to_string()
    }
}

/// The node's port, from its `config.toml`.
fn configured_port(dir: &Path) -> Option<u16> {
    Settings::load(dir).ok()?.port
}

/// `wats fleet up`.
async fn up(ctx: &Options, base: &Path, args: UpArgs) -> Result<()> {
    if !cfg!(unix) {
        bail!(
            "`wats fleet up` is not supported on native Windows yet (it runs each node as `wats \
             --detach`): run wats under WSL"
        );
    }
    let template = std::path::absolute(args.from.as_deref().unwrap_or(&ctx.data_dir))?;
    if base.starts_with(&template) || template.starts_with(base) {
        bail!(
            "the fleet's directory {} and the template {} must not contain each other: choose \
             another --base",
            tilde(base),
            tilde(&template)
        );
    }
    let old = Manifest::load(base)?;
    let had = old.as_ref().map_or(0, |m| m.nodes.len());
    let n = match (args.nodes, had) {
        (Some(0), _) => bail!("-n: at least 1 node"),
        (Some(n), had) if n < had => bail!(
            "the fleet in {} has {had} nodes: it does not shrink. Stop it (`wats fleet stop{}`) \
             and remove the data directories you no longer want, and their entries in {}",
            tilde(base),
            base_flag(base),
            MANIFEST_NAME
        ),
        (Some(n), _) => n,
        (None, 0) => bail!("how many nodes? `wats fleet up -n <N>`"),
        (None, had) => had,
    };
    let mut manifest = old.unwrap_or_else(|| Manifest {
        version: 1,
        created: crate::runs::format_time(crate::node::now_secs()),
        template: template.clone(),
        args: Vec::new(),
        no_agent: false,
        nodes: Vec::new(),
    });
    if ctx.node_args_given || manifest.nodes.is_empty() {
        manifest.args = ctx.node_args.clone();
    }
    if let Some(no_agent) = args.no_agent {
        manifest.no_agent = no_agent;
    }
    if args.from.is_some() {
        manifest.template = template.clone();
    }
    // A port changed by hand in a node's config.toml is its port.
    for node in &mut manifest.nodes {
        if let Some(port) = configured_port(&base.join(&node.name)) {
            node.port = port;
        }
    }
    let text = if n > had {
        template_text(&manifest.template)?
    } else {
        String::new()
    };
    let storage = args.storage.or(ctx.storage_max);
    let full = if args.full {
        Some(true)
    } else {
        match ctx.initial_max {
            Some(InitialMax::Bytes(0)) => Some(true),
            Some(_) => Some(false),
            None => None,
        }
    };

    // Is there room?
    let per_node = match storage {
        Some(storage) => storage,
        None => {
            let settings = Settings::load(&manifest.template).unwrap_or_default();
            settings
                .storage
                .max
                .as_deref()
                .map(parse_size)
                .transpose()
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    parse_size(crate::settings::DEFAULT_STORAGE_MAX).unwrap_or_default()
                })
        }
    };
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    for warning in resource_warnings(n, cpus, per_node, free_bytes(base)) {
        eprintln!("warning: {warning}");
    }

    // New nodes: a data directory, a key and a port each.
    std::fs::create_dir_all(base).with_context(|| format!("create {}", tilde(base)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut taken: HashSet<u16> = manifest.nodes.iter().map(|n| n.port).collect();
    let mut held = Vec::new();
    let mut created = Vec::new();
    for i in had + 1..=n {
        let name = node_name(i);
        let dir = base.join(&name);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", tilde(&dir)))?;
        let root = crate::identity::RootSecret::load_or_create(&dir.join("secret.key"))
            .with_context(|| format!("the key of {name}"))?;
        let endpoint_id = iroh::SecretKey::from_bytes(&root.transport_key_bytes())
            .public()
            .to_string();
        let port = match args.port_base {
            Some(first) => {
                let port = u16::try_from(usize::from(first) + i - 1)
                    .ok()
                    .filter(|p| *p > 0)
                    .context("--port-base: the ports go past 65535")?;
                anyhow::ensure!(
                    !taken.contains(&port),
                    "--port-base: port {port} is already a node's"
                );
                port
            }
            None => configured_port(&dir)
                .filter(|p| !taken.contains(p))
                .map_or_else(|| free_port(&taken, &mut held), Ok)?,
        };
        taken.insert(port);
        if !dir.join(crate::settings::FILE_NAME).exists() {
            for note in write_new_config(&dir, &manifest.template, &text)? {
                if i == had + 1 {
                    eprintln!("note: {note}");
                }
            }
        }
        manifest.nodes.push(ManifestNode {
            name: name.clone(),
            endpoint_id,
            port,
        });
        created.push(name);
    }
    drop(held);

    // Every node: its port, the others as bootstrap peers, --storage and
    // --full.
    let fleet_ids: HashSet<String> = manifest
        .nodes
        .iter()
        .map(|n| n.endpoint_id.clone())
        .collect();
    let template_bootstrap = if n > had {
        Settings::parse(&text)
            .map(|s| s.bootstrap)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    for node in &manifest.nodes {
        let dir = base.join(&node.name);
        let peers: Vec<String> = manifest
            .nodes
            .iter()
            .filter(|o| o.name != node.name)
            .map(|o| format!("{}@127.0.0.1:{}", o.endpoint_id, o.port))
            .collect();
        let is_new = created.contains(&node.name);
        let mut extra = ctx.bootstrap.clone();
        if is_new {
            extra.extend(template_bootstrap.iter().cloned());
        }
        write_fleet_keys(
            &dir,
            is_new.then_some(node.port),
            &fleet_ids,
            &peers,
            &extra,
            storage,
            full,
        )
        .with_context(|| format!("{}: config.toml", node.name))?;
    }
    manifest.save(base)?;
    // Their names (ADR 0023 §8): node-<i>, or <fleet>-<i> when another
    // node of this machine has it.
    let machine = crate::naming::Machine::of_user(crate::background::registry_dir(ctx.hermetic));
    for node in &manifest.nodes {
        let dir = base.join(&node.name);
        if let Err(e) = crate::naming::ensure(&dir, &machine) {
            eprintln!("note: {}: no name: {e}", node.name);
        }
    }

    // Start those that do not run, one after the other.
    let exe = std::env::current_exe()?;
    let mut failed = Vec::new();
    let mut started_any = false;
    let mut running_before = Vec::new();
    for node in &manifest.nodes {
        let dir = base.join(&node.name);
        if crate::node::node_running(&dir) {
            let pid = crate::node::lock_holder(&dir).map(|h| h.pid);
            println!(
                "{}  already running  pid {}",
                node.name,
                pid.map_or_else(|| "?".into(), |p| p.to_string())
            );
            running_before.push(node.name.clone());
            continue;
        }
        if started_any && !args.stagger.is_zero() {
            tokio::time::sleep(args.stagger).await;
        }
        started_any = true;
        let mut command = std::process::Command::new(&exe);
        if ctx.hermetic {
            command.arg("--hermetic");
        }
        command
            .args(&manifest.args)
            .arg("--dir")
            .arg(&dir)
            .arg("--detach");
        if manifest.no_agent {
            command.arg("--no-agent");
        }
        for name in ENV_REMOVE {
            command.env_remove(name);
        }
        let out = command
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("start {}", node.name))?;
        if out.status.success() {
            let pid = crate::node::lock_holder(&dir).map(|h| h.pid);
            println!(
                "{}  started  pid {}  port {}  endpoint {}",
                node.name,
                pid.map_or_else(|| "?".into(), |p| p.to_string()),
                node.port,
                &node.endpoint_id[..node.endpoint_id.len().min(12)]
            );
        } else {
            eprintln!(
                "{}  did not start:\n{}",
                node.name,
                String::from_utf8_lossy(&out.stderr).trim_end()
            );
            failed.push(node.name.clone());
        }
    }
    if !running_before.is_empty() && (storage.is_some() || full.is_some() || had < n) {
        eprintln!(
            "note: {} kept running: they read config.toml at start only (restart: wats fleet \
             stop{flag} && wats fleet up{flag})",
            running_before.join(", "),
            flag = base_flag(base)
        );
    }
    let flag = base_flag(base);
    println!(
        "fleet {}: {} node(s){}{}\ncheck: wats fleet check{flag} · logs: wats fleet logs{flag} · \
         stop: wats fleet stop{flag}\nan MCP client on node-1: wats mcp --dir {}",
        tilde(base),
        manifest.nodes.len(),
        if manifest.no_agent { ", no agent" } else { "" },
        if manifest.args.is_empty() {
            String::new()
        } else {
            format!(" (with {})", manifest.args.join(" "))
        },
        tilde(&base.join(node_name(1)))
    );
    if !failed.is_empty() {
        bail!(
            "{} node(s) did not start: {} (see wats fleet logs{flag})",
            failed.len(),
            failed.join(", ")
        );
    }
    if args.check {
        let check = CheckArgs {
            wait: Some(args.wait),
            since: Duration::from_secs(3600),
            strict: false,
            json: false,
        };
        check_command(base, &manifest, &check).await?;
    }
    Ok(())
}

/// What `check` found about one node.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NodeReport {
    pub name: String,
    pub data_dir: PathBuf,
    pub running: bool,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub uptime_secs: Option<u64>,
    pub network: String,
    /// The swarm it sees, per network (ADR 0026): `neighbors`, `nodes_24h`.
    pub swarm: Value,
    /// Gossip neighbors now, all of them and those of the fleet.
    pub neighbors: usize,
    pub fleet_neighbors: usize,
    /// The other nodes of the fleet it has heard from since it started.
    pub met: usize,
    pub others: usize,
    pub agents_expected: Vec<String>,
    pub agents_running: Vec<String>,
    pub log_errors: usize,
    pub log_warnings: usize,
    /// Why it fails; empty: it passes.
    pub problems: Vec<String>,
    /// Worth a look, not a failure (unless --strict).
    pub warnings: Vec<String>,
}

impl NodeReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    fn status(&self) -> &'static str {
        if !self.ok() {
            "FAIL"
        } else if !self.warnings.is_empty() {
            "warn"
        } else {
            "ok"
        }
    }
}

/// Seconds since the Unix epoch of a log line's timestamp
/// (`2026-10-21T09:12:33.123456Z …`).
pub fn log_line_time(line: &str) -> Option<u64> {
    let stamp = line.get(..19)?;
    let b = stamp.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| stamp.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok()
}

/// The level of a log line: `ERROR`, `WARN`, … (a panic counts as an
/// error).
fn log_level(line: &str) -> Option<&'static str> {
    if line.contains("panicked at") {
        return Some("ERROR");
    }
    let mut words = line.split_whitespace();
    log_line_time(words.next()?)?;
    match words.next()? {
        "ERROR" => Some("ERROR"),
        "WARN" => Some("WARN"),
        _ => None,
    }
}

/// Errors and warnings among `lines` logged at or after `since`; lines
/// without a timestamp take the previous one's.
pub fn scan_log(lines: &[String], since: u64) -> (Vec<String>, Vec<String>) {
    let (mut errors, mut warnings) = (Vec::new(), Vec::new());
    let mut at: Option<u64> = None;
    for line in lines {
        if let Some(ts) = line.split_whitespace().next().and_then(log_line_time) {
            at = Some(ts);
        }
        if at.is_none_or(|t| t < since) {
            continue;
        }
        match log_level(line) {
            Some("ERROR") => errors.push(line.clone()),
            Some("WARN") => warnings.push(line.clone()),
            _ => {}
        }
    }
    (errors, warnings)
}

/// A log line for the problem list: without its timestamp, at most 160
/// characters.
fn short_line(line: &str) -> String {
    let line = line.trim();
    let rest = match line.split_once(char::is_whitespace) {
        Some((first, rest)) if log_line_time(first).is_some() => rest.trim_start(),
        _ => line,
    };
    if rest.chars().count() > 160 {
        rest.chars().take(159).collect::<String>() + "…"
    } else {
        rest.to_string()
    }
}

/// Whether `command` (a path, or a name looked up on `PATH`) is an
/// executable file.
fn command_found(command: &Path) -> bool {
    let executable = |path: &Path| {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            meta.is_file()
        }
    };
    if command.components().count() > 1 {
        return executable(command);
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| executable(&dir.join(command))))
}

/// Check one node.
async fn check_node(
    manifest: &Manifest,
    base: &Path,
    node: &ManifestNode,
    since: u64,
    strict: bool,
) -> NodeReport {
    let dir = base.join(&node.name);
    let others = manifest.others(&node.name);
    let mut report = NodeReport {
        name: node.name.clone(),
        data_dir: dir.clone(),
        others: others.len(),
        network: "-".into(),
        ..Default::default()
    };
    // Its agents, from its config.toml.
    match Settings::load(&dir) {
        Ok(settings) => {
            if !manifest.no_agent {
                let user = crate::roster::os_user_profile();
                match crate::roster::select(&settings, &dir, &user, &[]) {
                    Ok(members) => {
                        for member in members {
                            report.agents_expected.push(member.name.clone());
                            let runtime = member.runtime.as_deref().unwrap_or_default();
                            if runtime == "openclaw" {
                                continue;
                            }
                            let command = std::env::var_os("WATS_AGENT_COMMAND")
                                .filter(|c| !c.is_empty())
                                .map(PathBuf::from)
                                .or_else(|| member.settings.command.clone())
                                .unwrap_or_else(|| PathBuf::from(runtime));
                            if !command_found(&command) {
                                report.problems.push(format!(
                                    "agent {}: its command {} is not found",
                                    member.name,
                                    command.display()
                                ));
                            }
                        }
                    }
                    Err(e) => report.problems.push(format!("agents: {e:#}")),
                }
            }
        }
        Err(e) => report.problems.push(format!("config.toml: {e:#}")),
    }
    report.running = crate::node::node_running(&dir);
    if !report.running {
        report.problems.insert(0, "not running".into());
        return report;
    }
    let info =
        crate::attach::control_within(&dir, crate::attach::Control::Info, Duration::from_secs(3))
            .await;
    let info = match info {
        Ok(Some(info)) => info,
        Ok(None) => {
            report.pid = crate::node::lock_holder(&dir).map(|h| h.pid);
            report
                .problems
                .insert(0, "runs, but does not answer on its socket".into());
            return report;
        }
        Err(e) => {
            report
                .problems
                .insert(0, format!("does not answer on its socket: {e:#}"));
            return report;
        }
    };
    report.pid = info["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
    report.version = info["version"].as_str().map(str::to_string);
    let started = info["started"].as_u64().unwrap_or(0);
    let now = crate::node::now_secs();
    report.uptime_secs = (started > 0).then(|| now.saturating_sub(started));
    let connectivity = &info["health"]["connectivity"];
    report.network = match connectivity["state"].as_str() {
        Some("online") => crate::reconnect::summary(connectivity),
        Some(state) => state.to_string(),
        None => "-".into(),
    };
    report.swarm = info["swarm"].clone();
    report.agents_running = info["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    let version = env!("CARGO_PKG_VERSION");
    if report.version.as_deref() != Some(version) {
        report.problems.push(format!(
            "runs wats {}, this one is {version}: restart it (wats fleet stop, then up)",
            report.version.as_deref().unwrap_or("?")
        ));
    }
    for name in &report.agents_expected {
        if !report.agents_running.contains(name) {
            report
                .problems
                .push(format!("agent {name} is configured but not running"));
        }
    }
    // Its neighbors, and the other nodes it has met.
    let peers = crate::attach::control_within(
        &dir,
        crate::attach::Control::Peers {
            ids: others.clone(),
        },
        Duration::from_secs(3),
    )
    .await;
    match peers {
        Ok(Some(peers)) => {
            let neighbors: Vec<&str> = peers["neighbors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            report.neighbors = neighbors.len();
            report.fleet_neighbors = neighbors
                .iter()
                .filter(|n| others.iter().any(|o| o == *n))
                .count();
            let met: BTreeSet<&str> = peers["met"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            report.met = met.len();
            if report.neighbors == 0 {
                report.problems.push("no neighbor".into());
            }
            if report.met < others.len() {
                let missing: Vec<&str> = manifest
                    .nodes
                    .iter()
                    .filter(|n| n.name != node.name && !met.contains(n.endpoint_id.as_str()))
                    .map(|n| n.name.as_str())
                    .collect();
                report
                    .problems
                    .push(format!("has not met {} of the fleet", missing.join(", ")));
            }
        }
        Ok(None) => report.problems.push("does not answer on its socket".into()),
        Err(e) => report
            .problems
            .push(format!("does not tell its peers ({e:#}): restart it")),
    }
    // Its log since it started, at most `since` ago.
    let log = crate::background::log_path(&dir);
    let lines = crate::background::tail(&log, 5000);
    let (errors, warnings) = scan_log(&lines, since.max(started));
    report.log_errors = errors.len();
    report.log_warnings = warnings.len();
    if let Some(last) = errors.last() {
        report.problems.push(format!(
            "{} error(s) in its log, the last: {}",
            errors.len(),
            short_line(last)
        ));
    }
    if let Some(last) = warnings.last() {
        let what = format!(
            "{} warning(s) in its log, the last: {}",
            warnings.len(),
            short_line(last)
        );
        if strict {
            report.problems.push(what);
        } else {
            report.warnings.push(what);
        }
    }
    report
}

/// Check every node of the fleet.
pub async fn check(
    manifest: &Manifest,
    base: &Path,
    since: Duration,
    strict: bool,
) -> Vec<NodeReport> {
    let since = crate::node::now_secs().saturating_sub(since.as_secs());
    let mut reports = Vec::new();
    for node in &manifest.nodes {
        reports.push(check_node(manifest, base, node, since, strict).await);
    }
    reports
}

/// The table of `check`.
pub fn table(reports: &[NodeReport]) -> String {
    let dash = || "-".to_string();
    let header = [
        "NODE",
        "STATUS",
        "PID",
        "VERSION",
        "UPTIME",
        "NETWORK",
        "NODES 24H",
        "FLEET",
        "AGENTS",
        "LOG",
    ];
    let rows: Vec<[String; 10]> = reports
        .iter()
        .map(|r| {
            let agents = if r.agents_expected.is_empty() {
                dash()
            } else {
                format!(
                    "{}/{} ({})",
                    r.agents_expected
                        .iter()
                        .filter(|a| r.agents_running.contains(a))
                        .count(),
                    r.agents_expected.len(),
                    r.agents_expected.join(",")
                )
            };
            [
                r.name.clone(),
                r.status().to_string(),
                r.pid.map_or_else(dash, |p| p.to_string()),
                r.version.clone().unwrap_or_else(dash),
                r.uptime_secs
                    .map_or_else(dash, crate::background::format_uptime),
                r.network.clone(),
                crate::swarm::ps_cell(Some(&r.swarm)),
                if r.running {
                    format!("met {}/{}, {} now", r.met, r.others, r.fleet_neighbors)
                } else {
                    dash()
                },
                agents,
                if !r.running {
                    dash()
                } else if r.log_errors + r.log_warnings == 0 {
                    "clean".into()
                } else {
                    format!("{} err, {} warn", r.log_errors, r.log_warnings)
                },
            ]
        })
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut text = String::new();
        for (i, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            text.push_str(cell);
            if i + 1 < cells.len() {
                text.push_str(&" ".repeat(width.saturating_sub(cell.chars().count()) + 2));
            }
        }
        text.trim_end().to_string() + "\n"
    };
    let mut out = line(header.to_vec());
    for row in &rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    for r in reports {
        for problem in &r.problems {
            out.push_str(&format!("{}: {problem}\n", r.name));
        }
        for warning in &r.warnings {
            out.push_str(&format!("{} (warning): {warning}\n", r.name));
        }
    }
    out
}

/// `wats fleet check`, and `up --check`.
async fn check_command(base: &Path, manifest: &Manifest, args: &CheckArgs) -> Result<()> {
    let deadline = args.wait.map(|w| tokio::time::Instant::now() + w);
    let reports = loop {
        let reports = check(manifest, base, args.since, args.strict).await;
        let done = reports.iter().all(NodeReport::ok)
            || deadline.is_none_or(|d| tokio::time::Instant::now() >= d);
        if done {
            break reports;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let ok = reports.iter().filter(|r| r.ok()).count();
    if args.json {
        let value = json!({
            "base": base,
            "ok": ok == reports.len(),
            "nodes": reports,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", table(&reports));
        println!(
            "fleet {}: {ok} of {} node(s) ok",
            tilde(base),
            reports.len()
        );
    }
    if ok < reports.len() {
        bail!(
            "fleet check failed: {} of {} node(s) not ok",
            reports.len() - ok,
            reports.len()
        );
    }
    Ok(())
}

/// `wats fleet stop`: `wats stop` for each running node, all at once.
fn stop(ctx: &Options, base: &Path, manifest: &Manifest, timeout: Duration) -> Result<()> {
    if !cfg!(unix) {
        bail!("`wats fleet stop` is not supported on native Windows yet: run wats under WSL");
    }
    let exe = std::env::current_exe()?;
    let mut children = Vec::new();
    for node in &manifest.nodes {
        let dir = base.join(&node.name);
        if !crate::node::node_running(&dir) {
            println!("{}  not running", node.name);
            continue;
        }
        let mut command = std::process::Command::new(&exe);
        if ctx.hermetic {
            command.arg("--hermetic");
        }
        command
            .arg("--dir")
            .arg(&dir)
            .arg("stop")
            .arg("--timeout")
            .arg(format!("{}s", timeout.as_secs().max(1)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in ENV_REMOVE {
            command.env_remove(name);
        }
        let child = command
            .spawn()
            .with_context(|| format!("stop {}", node.name))?;
        children.push((node.name.clone(), child));
    }
    let mut failed = Vec::new();
    for (name, child) in children {
        let out = child.wait_with_output()?;
        let said = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            println!("{name}  {}", said.trim());
        } else {
            eprintln!("{name}  not stopped: {}{}", said.trim(), err.trim());
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        bail!(
            "{} node(s) not stopped: {}",
            failed.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

/// The nodes `logs` shows: all, or the one `--node` names.
fn chosen<'a>(manifest: &'a Manifest, node: Option<&str>) -> Result<Vec<&'a ManifestNode>> {
    let Some(wanted) = node else {
        return Ok(manifest.nodes.iter().collect());
    };
    let name = if wanted.bytes().all(|b| b.is_ascii_digit()) {
        format!("node-{wanted}")
    } else {
        wanted.to_string()
    };
    let found: Vec<&ManifestNode> = manifest.nodes.iter().filter(|n| n.name == name).collect();
    if found.is_empty() {
        bail!(
            "no {name} in this fleet ({})",
            manifest
                .nodes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(found)
}

/// `wats fleet logs`.
async fn logs(
    base: &Path,
    manifest: &Manifest,
    lines: usize,
    node: Option<&str>,
    follow: bool,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use std::io::Write;
    let nodes = chosen(manifest, node)?;
    let width = nodes.iter().map(|n| n.name.len()).max().unwrap_or(0);
    let mut followers = Vec::new();
    let mut out = std::io::stdout().lock();
    for node in &nodes {
        let log = crate::background::log_path(&base.join(&node.name));
        followers.push((
            node.name.clone(),
            crate::background::Follower::at_end(&log),
            Vec::<u8>::new(),
        ));
        for line in crate::background::tail(&log, lines) {
            writeln!(out, "{:width$} | {line}", node.name)?;
        }
    }
    out.flush()?;
    drop(out);
    if !follow {
        return Ok(());
    }
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        let mut out = std::io::stdout().lock();
        for (name, follower, partial) in &mut followers {
            partial.extend(follower.read_new()?);
            while let Some(end) = partial.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = partial.drain(..=end).collect();
                let text = String::from_utf8_lossy(&line);
                let written = writeln!(out, "{name:width$} | {}", text.trim_end());
                if let Err(e) = written {
                    // `wats fleet logs -f | head`
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(e.into());
                }
            }
        }
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compose_args(extra: &[&str]) -> ComposeArgs {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: ComposeArgs,
        }
        let mut argv = vec!["compose"];
        argv.extend_from_slice(extra);
        <Cli as clap::Parser>::parse_from(argv).args
    }

    /// One service per node, each with its port, volume and the hardening;
    /// the infrastructure settings and extra environment on each.
    #[test]
    fn compose_gives_each_node_its_port_and_volume() {
        let file = compose_file(&compose_args(&[
            "-n",
            "3",
            "--infra-node",
            "--command",
            "--agent opencode",
            "--env",
            "KEY=a b",
        ]))
        .unwrap();
        for i in 0..3u32 {
            let port = 7850 + i;
            assert!(
                file.contains(&format!("  wats-{}:\n    <<: *node", i + 1)),
                "{file}"
            );
            assert!(file.contains(&format!("WATS_PORT: \"{port}\"")), "{file}");
            assert!(
                file.contains(&format!("ports: [\"{port}:{port}/udp\"]")),
                "{file}"
            );
            assert!(
                file.contains(&format!("volumes: [\"wats-{}:/data\"]", i + 1)),
                "{file}"
            );
        }
        assert!(!file.contains("wats-4"), "{file}");
        for line in [
            "command: [\"--agent\", \"opencode\"]",
            "platform: linux/amd64",
            "read_only: true",
            "cap_drop: [ALL]",
            "no-new-privileges:true",
            "pids: 512",
            "WATS_GOSSIP_ACTIVE_VIEW: \"8\"",
            "WATS_SYNC_ANTI_ENTROPY: \"1m\"",
            "WATS_HALT_SERVE: \"true\"",
            "restart: unless-stopped",
            "KEY: \"a b\"",
        ] {
            assert!(file.contains(line), "{line}: {file}");
        }
        // Another image: no platform forced.
        let own = compose_file(&compose_args(&["-n", "1", "--image", "me/wats:dev"])).unwrap();
        assert!(!own.contains("platform:"), "{own}");
        // Not infrastructure: on-failure, and no halt.serve.
        assert!(own.contains("restart: on-failure"), "{own}");
        assert!(!own.contains("WATS_HALT_SERVE"), "{own}");
        // Refused: a bad name, a port overflow, WATS_PORT in --env.
        assert!(compose_file(&compose_args(&["-n", "1", "--name", "Bad_Name"])).is_err());
        assert!(compose_file(&compose_args(&["-n", "10", "--port-base", "65530"])).is_err());
        assert!(compose_file(&compose_args(&["-n", "1", "--env", "WATS_PORT=1"])).is_err());
    }

    #[test]
    fn log_times_parse() {
        assert_eq!(log_line_time("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            log_line_time("2026-10-21T09:12:33.123456Z"),
            Some(1_792_573_953)
        );
        assert_eq!(log_line_time("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(log_line_time("not a time at all, no"), None);
        assert_eq!(log_line_time("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn the_log_scan_counts_recent_errors_and_warnings() {
        let lines: Vec<String> = [
            "2026-10-21T09:00:00.000000Z ERROR wats::node: too old",
            "2026-10-21T10:00:00.000000Z  INFO wats::node: started",
            "2026-10-21T10:00:01.000000Z  WARN wats::node: slow peer",
            "a banner line with ERROR in it",
            "thread 'main' panicked at src/node.rs:1:1:",
            "2026-10-21T10:00:02.000000Z ERROR wats::node: broken",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let since = log_line_time("2026-10-21T10:00:00Z").unwrap();
        let (errors, warnings) = scan_log(&lines, since);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("panicked"));
        assert!(errors[1].contains("broken"));
        assert_eq!(warnings.len(), 1);
        // Nothing before the window.
        let (errors, _) = scan_log(&lines[..1], since);
        assert!(errors.is_empty());
    }

    #[test]
    fn resources_warn_past_the_cpus_and_the_free_disk() {
        let g = 1024 * 1024 * 1024;
        assert!(resource_warnings(6, 8, 10 * g, Some(500 * g)).is_empty());
        let w = resource_warnings(8, 8, 10 * g, Some(500 * g));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("8 nodes on 8 CPU(s)"), "{w:?}");
        let w = resource_warnings(6, 8, 100 * g, Some(300 * g));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("--storage 50G"), "{w:?}");
        // One CPU: one node is fine.
        assert!(resource_warnings(1, 1, g, None).is_empty());
    }

    #[test]
    fn a_new_node_gets_its_own_workspace_and_the_template_heartbeat() {
        let tmp = tempfile::tempdir().unwrap();
        let template = tmp.path().join("template");
        let node = tmp.path().join("node-1");
        std::fs::create_dir_all(&template).unwrap();
        std::fs::create_dir_all(&node).unwrap();
        let text = "# mine\n[storage]\nmax = \"20G\"\n\n[agents.claude]\nruntime = \"claude\"\n\
                    workdir = \"/srv/shared\"\nheartbeat_file = \"beat.md\"\n";
        let notes = write_new_config(&node, &template, text).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let settings = Settings::load(&node).unwrap();
        let claude = &settings.agents["claude"];
        assert!(claude.workdir.is_none());
        assert_eq!(
            claude.heartbeat_file.as_deref(),
            Some(
                std::path::absolute(template.join("beat.md"))
                    .unwrap()
                    .as_path()
            )
        );
        assert_eq!(settings.storage.max.as_deref(), Some("20G"));
        let written = std::fs::read_to_string(node.join("config.toml")).unwrap();
        assert!(written.contains("# mine"), "{written}");

        // The fleet's keys: port, peers first, the template's bootstrap kept.
        let a = iroh::SecretKey::from_bytes(&[1; 32]).public().to_string();
        let b = iroh::SecretKey::from_bytes(&[2; 32]).public().to_string();
        let router = iroh::SecretKey::from_bytes(&[3; 32]).public().to_string();
        let fleet: HashSet<String> = [a.clone(), b.clone()].into_iter().collect();
        let peer = format!("{b}@127.0.0.1:4000");
        let extra = format!("{router}@router.example.org:7842");
        write_fleet_keys(
            &node,
            Some(4001),
            &fleet,
            std::slice::from_ref(&peer),
            std::slice::from_ref(&extra),
            Some(100 * 1024 * 1024 * 1024),
            Some(true),
        )
        .unwrap();
        let settings = Settings::load(&node).unwrap();
        assert_eq!(settings.port, Some(4001));
        assert_eq!(settings.bootstrap, vec![peer.clone(), extra.clone()]);
        assert_eq!(settings.sync.initial_max.as_deref(), Some("0"));
        assert_eq!(settings.storage.max.as_deref(), Some("100G"));
        // Again, with another port for b: the old entry goes.
        let moved = format!("{b}@127.0.0.1:5000");
        write_fleet_keys(
            &node,
            None,
            &fleet,
            std::slice::from_ref(&moved),
            &[],
            None,
            None,
        )
        .unwrap();
        let settings = Settings::load(&node).unwrap();
        assert_eq!(settings.port, Some(4001));
        assert_eq!(settings.bootstrap, vec![moved, extra]);
        assert_eq!(settings.sync.initial_max.as_deref(), Some("0"));
    }
}
