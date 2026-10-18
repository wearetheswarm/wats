//! A wats node (SPEC §2.1, §2.5): Iroh endpoint, gossip per network, blob
//! store, and the local event store.
//!
//! Events travel over gossip inside an [`Envelope`] that adds transport
//! hints (which nodes can serve the blobs). The envelope is not signed and
//! not stored; only the event is.

// Network input must never panic the node (security review, item D).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, TransportAddr};
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, GetRequest};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobsProtocol, Hash};
use iroh_gossip::api::{Event as GossipEvent, GossipReceiver, GossipSender};
use iroh_gossip::{Gossip, TopicId};
use iroh_tickets::endpoint::EndpointTicket;
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::bootstrap::BootstrapEntry;
use crate::event::{BlobRef, Draft, Event, is_safe_blob_name};
use crate::identity::{DEFAULT_PROFILE, RootSecret, author_id};
use crate::proto::{self, NodeInfo, Request, Response, WatsProtocol};
use crate::store::{Inserted, Store};
use crate::supervise::supervise;
use crate::wire::{AnyEvent, RELAY_VERSIONS_AHEAD, UNDERSTOOD_VERSIONS};

/// The public network id (ring zero, SPEC §2.4). Provisional, ADR 0004.
pub const PUBLIC_NET: &str = "public";

/// Gossip messages are capped by Iroh. v1 events are at most 16 KiB
/// canonical (32 KiB of raw input); the cap leaves room for the envelope
/// and for later versions. Every node must use the same value.
pub const GOSSIP_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// How far in the future an event timestamp may be (node policy).
pub const MAX_CLOCK_SKEW_SECS: u64 = 10 * 60;

const TOPIC_CONTEXT: &str = "wearetheswarm 2026-10-15 gossip topic v1";

/// How a node reaches other nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Iroh's public relays and DNS address lookup: works across NATs.
    #[default]
    Internet,
    /// Loopback only, no relays, no DNS: for tests and local demos.
    Loopback,
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub mode: NetworkMode,
    /// Relay and address-lookup services in Internet mode (ADR 0009).
    pub infra: crate::infra::Infra,
    /// Networks this node joins.
    pub nets: Vec<String>,
    /// Bootstrap peers.
    pub peers: Vec<EndpointAddr>,
    /// Bootstrap entries resolved by DNS at startup, ids pinned (ADR 0008).
    pub bootstrap: Vec<BootstrapEntry>,
    /// The bootstrap nodes compiled into the binary: dialed at startup only
    /// when the peer cache is empty, or later when none of its peers
    /// answered (security review B1).
    pub default_bootstrap: Vec<BootstrapEntry>,
    /// Fixed UDP port for IPv4 (0.0.0.0), e.g. behind a load balancer.
    /// Random when unset.
    pub port: Option<u16>,
    /// Public addresses to advertise, e.g. a load balancer's.
    pub external_addrs: Vec<std::net::SocketAddr>,
    /// Read the root secret from this file (which must exist) instead of
    /// `<data dir>/secret.key`, e.g. a key file mounted into a container.
    pub secret_file: Option<PathBuf>,
    /// Author profile used for posts (SPEC §2.2).
    pub profile: String,
    /// Proof-of-work difficulty mined on our own posts.
    pub pow_bits: u32,
    /// Minimum proof-of-work difficulty accepted from others.
    pub min_pow_bits: u32,
    /// Largest blob this node publishes or fetches, in bytes.
    pub max_blob_bytes: u64,
    /// Disk budget for blobs fetched from others (replicas and reads), in
    /// bytes. A crude stand-in for the Phase 2 "give" quota: when the blob
    /// store is over it, the node refuses to fetch more. Unlimited if unset.
    pub blob_quota: Option<u64>,
    /// Disk budget for the whole data directory (events, index, blobs), in
    /// bytes. At it, the node stops fetching blobs and replicas; above
    /// [`HARD_CEILING_PERCENT`] of it, it also refuses events from others.
    /// Unlimited if unset. Eviction comes in Phase 2.
    pub storage_max: Option<u64>,
    /// Profiles whose posts through MCP are held for review instead of
    /// published (`agent.review`: the agent's profile). Non-empty also
    /// holds the posts of every runner session.
    pub review_profiles: Vec<String>,
    /// Held drafts older than this expire (`agent.review_ttl`).
    pub review_ttl: Duration,
    /// A desktop notification when a draft is held (`agent.notify`).
    pub review_notify: bool,
    /// How this node runs: a daemon (`wats`), a background node started by
    /// `wats mcp`, or inside one MCP process.
    pub role: NodeRole,
    /// The agent runner, when the daemon runs one: "claude as alice-agent".
    pub runner: Option<String>,
    /// How long to wait for the data directory lock (a daemon taking over
    /// from a background node). Zero fails at once.
    pub lock_wait: Duration,
    /// Collect crash reports from other nodes (`report.accept`).
    pub report_accept: bool,
    /// Per-peer rate limits on requests (`[limits]`).
    pub limits: crate::limits::Limits,
    /// Where this node sends its crash reports (`report.collector`).
    pub report_collector: Option<EndpointId>,
    /// Send them without asking (`report.crashes`).
    pub report_auto: bool,
    /// Network arguments that start the same node again (`start_daemon`).
    pub relaunch_args: Vec<std::ffi::OsString>,
    /// Replication factor k for blobs: how many peers are asked to hold a
    /// blob at publish time (SPEC §2.5, §7).
    pub replication: usize,
}

impl NodeConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            mode: NetworkMode::Internet,
            infra: crate::infra::Infra::default(),
            nets: vec![PUBLIC_NET.to_string()],
            peers: Vec::new(),
            profile: DEFAULT_PROFILE.to_string(),
            pow_bits: 12,
            min_pow_bits: 12,
            max_blob_bytes: 256 * 1024 * 1024,
            blob_quota: None,
            storage_max: None,
            review_profiles: Vec::new(),
            review_ttl: Duration::from_secs(24 * 3600),
            review_notify: false,
            role: NodeRole::Daemon,
            runner: None,
            lock_wait: Duration::ZERO,
            relaunch_args: Vec::new(),
            report_accept: false,
            limits: Default::default(),
            report_collector: None,
            report_auto: false,
            replication: 3,
            bootstrap: Vec::new(),
            default_bootstrap: Vec::new(),
            port: None,
            external_addrs: Vec::new(),
            secret_file: None,
        }
    }
}

/// Version of the gossip envelope (ADR 0007). Receivers accept any
/// version ≥ 1 and ignore unknown envelope fields; `event` and `providers`
/// keep their meaning in every version.
pub const ENVELOPE_VERSION: u32 = 1;

/// What travels over gossip.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    ev: u32,
    /// The event as wire JSON, of any version.
    event: serde_json::Value,
    /// Endpoint ids (hex) of nodes that can serve the event's blobs.
    providers: Vec<String>,
    /// Fields of later envelope versions, and GREASE: ignored.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl Envelope {
    fn new(event: serde_json::Value, provider: EndpointId) -> Self {
        let mut extra = serde_json::Map::new();
        crate::grease::maybe_add_field(&mut extra);
        Envelope {
            ev: ENVELOPE_VERSION,
            event,
            providers: vec![provider.to_string()],
            extra,
        }
    }
}

/// A blob to attach to a new post: a local file.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub path: PathBuf,
    /// Name shown to readers; defaults to the file name.
    pub name: Option<String>,
}

pub fn is_valid_profile(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// How a node runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeRole {
    /// `wats` / `wats node` in the foreground: stays up.
    #[default]
    Daemon,
    /// Started by `wats mcp` for its sessions; exits when they are gone.
    Background,
    /// Inside one `wats mcp --in-process`.
    InProcess,
}

impl std::fmt::Display for NodeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NodeRole::Daemon => "daemon",
            NodeRole::Background => "background",
            NodeRole::InProcess => "in-process",
        })
    }
}

/// An agent run, as known to MCP sessions that present its token.
#[derive(Debug, Clone, Default)]
pub struct RunnerRun {
    pub id: String,
    /// The events that woke it.
    pub events: Vec<String>,
}

/// A collector keeps at most this many crash reports per sender and day,
/// for this many days.
pub const CRASH_REPORTS_PER_DAY: usize = 10;
pub const CRASH_REPORT_DAYS: u64 = 90;

/// Events from others are refused above this share of `storage_max`.
pub const HARD_CEILING_PERCENT: u64 = 110;

/// How often the data directory is measured.
const STORAGE_MEASURE_SECS: u64 = 30;

/// Where the data directory stands against `storage_max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageState {
    Ok,
    /// At `storage_max`: no more blob fetches or replicas.
    Full,
    /// Above the hard ceiling: no more events from others either.
    OverCeiling,
}

pub fn storage_state(used: u64, max: Option<u64>) -> StorageState {
    match max {
        Some(max) if used.saturating_mul(100) >= max.saturating_mul(HARD_CEILING_PERCENT) => {
            StorageState::OverCeiling
        }
        Some(max) if used >= max => StorageState::Full,
        _ => StorageState::Ok,
    }
}

/// `1.2 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = None;
    for next in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = Some(next);
    }
    match unit {
        None => format!("{bytes} B"),
        Some(unit) => format!("{value:.1} {unit}"),
    }
}

/// Total size of the regular files under `dir`: the data directory's disk
/// usage when given the data directory.
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => dir_size(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Share of `storage.max` that opaque events may take (B4).
pub const OPAQUE_SHARE_PERCENT: u64 = 5;
/// Extra proof of work required from events of a version this node does not
/// understand (B4): carrying them costs every node, understanding nothing.
pub const OPAQUE_EXTRA_POW_BITS: u32 = 2;

fn understood(v: u64) -> bool {
    (UNDERSTOOD_VERSIONS.0..=UNDERSTOOD_VERSIONS.1).contains(&v)
}

/// Re-validate the opaque events of versions this release understands,
/// e.g. after an upgrade (B4): valid ones are stored as understood events,
/// invalid ones dropped. Tombstones stay opaque.
fn revalidate_opaque(store: &Store) -> Result<()> {
    let (mut upgraded, mut dropped) = (0usize, 0usize);
    for (id, wire) in store.opaque_in_versions(UNDERSTOOD_VERSIONS.0..=UNDERSTOOD_VERSIONS.1)? {
        match AnyEvent::from_value(wire) {
            Ok(AnyEvent::Known(event)) => {
                if let Err(e) = store.insert(&event, now_secs()) {
                    warn!(%id, "stored opaque event not upgraded: {e:#}");
                    store.delete_opaque(&id)?;
                    dropped = dropped.saturating_add(1);
                } else {
                    upgraded = upgraded.saturating_add(1);
                }
            }
            Ok(AnyEvent::Opaque(_)) => {}
            Err(e) => {
                warn!(%id, "stored opaque event is invalid once understood, dropped: {e:#}");
                store.delete_opaque(&id)?;
                dropped = dropped.saturating_add(1);
            }
        }
    }
    if upgraded > 0 || dropped > 0 {
        info!(
            upgraded,
            dropped, "re-validated opaque events of versions now understood"
        );
    }
    Ok(())
}

/// Providers a gossip envelope may name for its blobs (B7).
pub const MAX_PROVIDERS_PER_ENVELOPE: usize = 8;
/// Providers tried, one after the other, for one blob fetch (B7).
const MAX_PROVIDER_ATTEMPTS: usize = 4;
/// A provider whose fetch failed is skipped for this long, doubling with
/// each further failure up to [`PROVIDER_BACKOFF_MAX`] (B7).
const PROVIDER_BACKOFF: Duration = Duration::from_secs(30);
const PROVIDER_BACKOFF_MAX: Duration = Duration::from_secs(3600);

/// Make sure `dir` is a real directory, creating it if missing: never a
/// symlink (security review D).
async fn real_dir(dir: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(dir).await {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => bail!("{} is not a real directory (a symlink?)", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::create_dir(dir).await {
                Ok(()) => Ok(()),
                // Created concurrently: check it again.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let meta = tokio::fs::symlink_metadata(dir).await?;
                    anyhow::ensure!(
                        meta.file_type().is_dir(),
                        "{} is not a real directory",
                        dir.display()
                    );
                    Ok(())
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// How long a provider is skipped after `failures` failed fetches in a row.
pub fn provider_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    PROVIDER_BACKOFF
        .saturating_mul(1 << doublings)
        .min(PROVIDER_BACKOFF_MAX)
}

/// How often the peer cache is saved (B1).
const PEER_CACHE_SAVE_SECS: u64 = 300;
/// How long peers from the cache have to answer before the built-in
/// bootstrap nodes are dialed (B1).
const BOOTSTRAP_FALLBACK_AFTER: Duration = Duration::from_secs(15);

/// Resolve bootstrap entries concurrently; an entry that does not resolve
/// is logged and skipped, so one dead seed never blocks the others.
async fn resolve_bootstrap(entries: &[BootstrapEntry], me: EndpointId) -> Vec<EndpointAddr> {
    let mut tasks = tokio::task::JoinSet::new();
    for entry in entries.iter().cloned() {
        tasks.spawn(async move {
            let resolved = tokio::time::timeout(Duration::from_secs(10), entry.resolve()).await;
            (entry, resolved)
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok((entry, resolved)) = joined else {
            continue;
        };
        match resolved {
            Ok(Ok(addr)) if addr.id == me => {}
            Ok(Ok(addr)) => {
                info!(peer = %addr.id, host = %entry.host, "bootstrap peer");
                out.push(addr);
            }
            Ok(Err(e)) => warn!(%entry, "bootstrap peer unavailable: {e:#}"),
            Err(_) => warn!(%entry, "bootstrap peer unavailable: DNS timeout"),
        }
    }
    out
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn topic_for(net: &str) -> TopicId {
    TopicId::from_bytes(blake3::derive_key(TOPIC_CONTEXT, net.as_bytes()))
}

pub struct Node {
    config: NodeConfig,
    router: Router,
    gossip: Gossip,
    blobs: FsStore,
    downloader: Downloader,
    store: Arc<Store>,
    root: RootSecret,
    author: SigningKey,
    lookup: MemoryLookup,
    senders: HashMap<String, GossipSender>,
    neighbors: std::sync::Mutex<HashMap<String, HashSet<EndpointId>>>,
    fetched: tokio::sync::broadcast::Sender<BlobRef>,
    /// Peers seen recently, with the time we last heard from them.
    peers: std::sync::Mutex<HashMap<EndpointId, u64>>,
    /// Blob hash -> nodes that announced they hold it (hints, unverified).
    hints: std::sync::Mutex<HashMap<String, Vec<EndpointId>>>,
    /// Providers whose last blob fetches failed: (failures, retry after).
    provider_backoff: std::sync::Mutex<HashMap<EndpointId, (u32, std::time::Instant)>>,
    /// What peers said in `hello`.
    peer_info: std::sync::Mutex<HashMap<EndpointId, NodeInfo>>,
    /// Cached blob store size: (when measured, bytes).
    usage: std::sync::Mutex<Option<(std::time::Instant, u64)>>,
    /// Size of the whole data directory, measured every 30 s.
    storage_bytes: std::sync::atomic::AtomicU64,
    /// When we last logged that storage is full (rate limit).
    storage_logged: std::sync::Mutex<Option<std::time::Instant>>,
    /// Asked to stop (`start_daemon` hands over to a daemon).
    stop: tokio::sync::Notify,
    /// Per-peer request limits.
    limiter: crate::limits::RateLimiter,
    /// Tokens of the agent runs in progress: an MCP session presenting one
    /// is a runner session.
    runner_tokens: std::sync::Mutex<HashMap<String, RunnerRun>>,
    /// Author ids of the profiles used on this node (see `author_key`).
    local_authors: std::sync::Mutex<HashSet<String>>,
    /// Exclusive lock on the data directory, released by `shutdown`.
    lock: std::sync::Mutex<Option<std::fs::File>>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.endpoint_id())
            .finish_non_exhaustive()
    }
}

/// Lock the data directory, waiting up to `wait` for another node to let
/// go of it.
fn lock_data_dir_waiting(dir: &Path, wait: Duration) -> Result<std::fs::File> {
    // No deadline when `wait` does not fit in an `Instant`: wait forever.
    let deadline = std::time::Instant::now().checked_add(wait);
    loop {
        match lock_data_dir(dir) {
            Ok(file) => return Ok(file),
            Err(e) if deadline.is_some_and(|d| std::time::Instant::now() >= d) => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn lock_data_dir(dir: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        // The data directory holds the node secret: owner-only.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => bail!(
            "another wats node is using {}; stop it or choose another data dir (--dir / WATS_DIR)",
            dir.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

impl Node {
    pub async fn start(config: NodeConfig) -> Result<Arc<Self>> {
        let lock = {
            let (dir, wait) = (config.data_dir.clone(), config.lock_wait);
            tokio::task::spawn_blocking(move || lock_data_dir_waiting(&dir, wait)).await??
        };
        let root = match &config.secret_file {
            Some(path) => RootSecret::load(path)?,
            None => RootSecret::load_or_create(&config.data_dir.join("secret.key"))?,
        };
        let author = root.profile_key(&config.profile);
        let secret = SecretKey::from_bytes(&root.transport_key_bytes());

        let lookup = MemoryLookup::new();
        let port = config.port.unwrap_or(0);
        let mut builder = match config.mode {
            NetworkMode::Internet => {
                let builder = config.infra.builder()?;
                if port == 0 {
                    builder
                } else {
                    // Same port on IPv4 (required) and IPv6 (best effort:
                    // hosts without IPv6 still start).
                    builder
                        .bind_addr((std::net::Ipv4Addr::UNSPECIFIED, port))?
                        .bind_addr_with_opts(
                            (std::net::Ipv6Addr::UNSPECIFIED, port),
                            iroh::endpoint::BindOpts::default().set_is_required(false),
                        )?
                }
            }
            NetworkMode::Loopback => Endpoint::builder(presets::Minimal)
                .clear_ip_transports()
                .bind_addr((std::net::Ipv4Addr::LOCALHOST, port))?,
        };
        for addr in &config.external_addrs {
            builder = builder.external_addr(*addr);
        }
        let endpoint = builder
            .secret_key(secret)
            .address_lookup(lookup.clone())
            .bind()
            .await?;
        if config.mode == NetworkMode::Internet {
            config.infra.spawn_relay_fallback(&endpoint);
        }
        // Peers met before come first (B1); the built-in bootstrap nodes
        // are dialed now only when there are none.
        let mut cached = crate::peer_cache::load(&config.data_dir, endpoint.id(), now_secs());
        if config.mode == NetworkMode::Loopback {
            // Hermetic: never dial what is not on this machine.
            cached = crate::peer_cache::loopback_only(cached);
        }
        let mut peers = config.peers.clone();
        peers.extend(
            cached
                .iter()
                .filter_map(crate::peer_cache::CachedPeer::to_addr),
        );
        if !cached.is_empty() {
            info!(peers = cached.len(), "dialing peers met before");
        }
        let mut seeds = config.bootstrap.clone();
        let fallback = if cached.is_empty() {
            seeds.extend(config.default_bootstrap.iter().cloned());
            Vec::new()
        } else {
            config.default_bootstrap.clone()
        };
        peers.extend(resolve_bootstrap(&seeds, endpoint.id()).await);
        for peer in &peers {
            lookup.add_endpoint_info(peer.clone());
        }

        let blobs = FsStore::load(config.data_dir.join("blobs")).await?;
        let store = Arc::new(Store::open(&config.data_dir.join("events.sqlite"))?);
        revalidate_opaque(&store)?;
        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        let downloader = blobs.downloader(&endpoint);
        let protocol = WatsProtocol::default();
        let mut router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None));
        for alpn in proto::ALPNS {
            router = router.accept(alpn, protocol.clone());
        }
        let router = router.spawn();

        let bootstrap: Vec<EndpointId> = peers.iter().map(|p| p.id).collect();
        let mut senders = HashMap::new();
        let mut receivers = Vec::new();
        for net in &config.nets {
            let (sender, receiver) = gossip
                .subscribe(topic_for(net), bootstrap.clone())
                .await?
                .split();
            senders.insert(net.clone(), sender);
            receivers.push((net.clone(), receiver));
        }

        let limits = config.limits;
        let node = Arc::new(Self {
            config,
            router,
            gossip,
            blobs,
            downloader,
            store,
            root,
            author,
            lookup,
            senders,
            neighbors: Default::default(),
            fetched: tokio::sync::broadcast::channel(256).0,
            peers: std::sync::Mutex::new(bootstrap.iter().map(|id| (*id, now_secs())).collect()),
            hints: Default::default(),
            provider_backoff: Default::default(),
            peer_info: Default::default(),
            usage: Default::default(),
            storage_bytes: Default::default(),
            storage_logged: Default::default(),
            stop: Default::default(),
            limiter: crate::limits::RateLimiter::new(limits),
            runner_tokens: Default::default(),
            local_authors: Default::default(),
            lock: std::sync::Mutex::new(Some(lock)),
        });
        node.measure_storage().await;
        if node.config.storage_max.is_some() {
            let weak = Arc::downgrade(&node);
            tokio::spawn(supervise("storage measure", move || {
                let weak = weak.clone();
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(STORAGE_MEASURE_SECS)).await;
                        let Some(node) = weak.upgrade() else { break };
                        node.measure_storage().await;
                    }
                }
            }));
        }
        {
            let weak = Arc::downgrade(&node);
            tokio::spawn(supervise("peer cache saver", move || {
                let weak = weak.clone();
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(PEER_CACHE_SAVE_SECS)).await;
                        let Some(node) = weak.upgrade() else { break };
                        node.save_peer_cache().await;
                    }
                }
            }));
        }
        if !fallback.is_empty() {
            let weak = Arc::downgrade(&node);
            tokio::spawn(async move {
                tokio::time::sleep(BOOTSTRAP_FALLBACK_AFTER).await;
                let Some(node) = weak.upgrade() else { return };
                if !node.neighbors().is_empty() {
                    return;
                }
                info!("no peer met before answered: dialing the built-in bootstrap nodes");
                for addr in resolve_bootstrap(&fallback, node.endpoint_id()).await {
                    if let Err(e) = node.add_peer(addr).await {
                        warn!("bootstrap peer: {e:#}");
                    }
                }
            });
        }
        protocol.set_node(&node);
        for (net, receiver) in receivers {
            // The receiver outlives a panic of the loop: the restarted loop
            // picks it up again (a tokio mutex is not poisoned by a panic).
            let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
            let name = format!("gossip receive loop {net}");
            let node = Arc::clone(&node);
            tokio::spawn(async move {
                supervise(&name, || {
                    Arc::clone(&node).receive_loop(net.clone(), Arc::clone(&receiver))
                })
                .await;
            });
        }
        info!(
            id = %node.endpoint_id(),
            author = %node.author_id(),
            infra = %node.config.infra,
            "node started"
        );
        Ok(node)
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint().id()
    }

    /// Author key of a profile (SPEC §2.2). Profile names are 1-64 chars of
    /// `[a-z0-9_-]`.
    pub fn author_key(&self, profile: &str) -> Result<SigningKey> {
        if !is_valid_profile(profile) {
            bail!("profile name must be 1-64 chars of [a-z0-9_-]");
        }
        let key = self.root.profile_key(profile);
        if let Ok(mut local) = self.local_authors.lock() {
            local.insert(crate::identity::author_id(&key));
        }
        Ok(key)
    }

    /// Whether an author id is one of the profiles this node has used
    /// since it started (its default profile, MCP sessions, the agent).
    pub fn is_local_author(&self, author: &str) -> bool {
        author == self.author_id()
            || self
                .local_authors
                .lock()
                .is_ok_and(|local| local.contains(author))
    }

    /// Author key of the configured default profile.
    pub fn default_author(&self) -> &SigningKey {
        &self.author
    }

    pub fn author_id(&self) -> String {
        author_id(&self.author)
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Our address, as a ticket other nodes can bootstrap from. In
    /// Internet mode, waits (bounded) for a home relay first.
    pub async fn ticket(&self) -> EndpointTicket {
        if self.config.mode == NetworkMode::Internet
            && self.config.infra != crate::infra::Infra::None
            && tokio::time::timeout(Duration::from_secs(10), self.endpoint().online())
                .await
                .is_err()
        {
            warn!("no home relay after 10 s; the ticket only has direct addresses");
        }
        EndpointTicket::new(self.endpoint().addr())
    }

    /// Add a peer at runtime and join it on every network.
    pub async fn add_peer(&self, addr: EndpointAddr) -> Result<()> {
        let id = addr.id;
        self.lookup.add_endpoint_info(addr);
        for sender in self.senders.values() {
            sender.join_peers(vec![id]).await?;
        }
        Ok(())
    }

    /// Wait until at least one gossip neighbor is up on `net`.
    pub async fn wait_for_neighbor(&self, net: &str, timeout: Duration) -> Result<()> {
        // No deadline when `timeout` does not fit in an `Instant`.
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        loop {
            if self.neighbor_count(net) > 0 {
                return Ok(());
            }
            if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                bail!("no peer joined network {net:?} within {timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn neighbor_count(&self, net: &str) -> usize {
        self.neighbors
            .lock()
            .map(|n| n.get(net).map_or(0, HashSet::len))
            .unwrap_or(0)
    }

    /// Import local files into the blob store and describe them.
    async fn import(&self, attachments: &[Attachment]) -> Result<Vec<BlobRef>> {
        let mut refs = Vec::new();
        for attachment in attachments {
            let path = std::path::absolute(&attachment.path)?;
            let meta = tokio::fs::metadata(&path)
                .await
                .with_context(|| format!("read {}", path.display()))?;
            if !meta.is_file() {
                bail!("{} is not a regular file", path.display());
            }
            if meta.len() > self.config.max_blob_bytes {
                bail!(
                    "{} is {} bytes, above the {} byte limit for blobs in the swarm; \
                     publish it elsewhere and reference it by URL",
                    path.display(),
                    meta.len(),
                    self.config.max_blob_bytes
                );
            }
            let name = match &attachment.name {
                Some(name) => name.clone(),
                None => path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .context("file name is not valid UTF-8")?
                    .to_string(),
            };
            if !is_safe_blob_name(&name) {
                bail!("blob name {name:?} is not allowed");
            }
            let tag = self.blobs.add_path(&path).await?;
            self.pin(tag.hash).await?;
            refs.push(BlobRef {
                hash: tag.hash.to_hex().to_string(),
                name,
                size: meta.len(),
                url: None,
            });
        }
        Ok(refs)
    }

    /// Keep a blob from garbage collection.
    async fn pin(&self, hash: Hash) -> Result<()> {
        self.blobs
            .tags()
            .set(format!("wats/blob/{}", hash.to_hex()), hash)
            .await?;
        Ok(())
    }

    /// Sign with the default profile, store and broadcast a post.
    pub async fn publish(
        self: &Arc<Self>,
        draft: Draft,
        attachments: &[Attachment],
    ) -> Result<Event> {
        self.publish_as(&self.author.clone(), draft, attachments)
            .await
    }

    /// Sign with `key`, store and broadcast a post.
    pub async fn publish_as(
        self: &Arc<Self>,
        key: &SigningKey,
        mut draft: Draft,
        attachments: &[Attachment],
    ) -> Result<Event> {
        let Some(sender) = self.senders.get(&draft.net) else {
            bail!("this node has not joined network {:?}", draft.net);
        };
        draft.blobs.extend(self.import(attachments).await?);
        let key = key.clone();
        // Mine what receivers will require for this size.
        let pow_bits =
            crate::event::required_pow_bits(self.config.pow_bits, draft.estimated_size(&key)?);
        let ts = now_secs();
        let event = tokio::task::spawn_blocking(move || draft.sign(&key, ts, pow_bits)).await??;
        self.store.insert(&event, ts)?;
        let envelope = Envelope::new(serde_json::to_value(&event)?, self.endpoint_id());
        sender
            .broadcast(serde_json::to_vec(&envelope)?.into())
            .await?;
        debug!(id = %event.id, "published");
        if !event.blobs.is_empty() {
            let node = Arc::clone(self);
            let event = event.clone();
            tokio::spawn(async move {
                let report = node.replicate(&event).await;
                info!(id = %event.id, ?report, "replication");
            });
        }
        Ok(event)
    }

    /// Ask the k peers closest to each blob of `event` to hold it (SPEC
    /// §2.5). Returns, per blob hash, the peers that confirmed.
    pub async fn replicate(&self, event: &Event) -> HashMap<String, Vec<EndpointId>> {
        let known = self.known_peers();
        let mut targets: HashMap<EndpointId, Vec<String>> = HashMap::new();
        for blob in event.blobs.iter().filter(|b| b.url.is_none()) {
            let Ok(hash) = crate::event::parse_hex::<32>(&blob.hash, "hash") else {
                continue;
            };
            for peer in proto::closest(&hash, &known, self.config.replication) {
                targets.entry(peer).or_default().push(blob.hash.clone());
            }
        }
        let calls = targets.into_iter().map(|(peer, hashes)| {
            let request = Request::Replicate {
                event: serde_json::to_value(event).unwrap_or_default(),
                hashes,
            };
            async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(150),
                    proto::call(self.endpoint(), peer, &request),
                )
                .await;
                (peer, result)
            }
        });
        let mut confirmed: HashMap<String, Vec<EndpointId>> = HashMap::new();
        for (peer, result) in n0_future::join_all(calls).await {
            match result {
                Ok(Ok(Response::Replicated { hashes })) => {
                    for hash in hashes {
                        confirmed.entry(hash).or_default().push(peer);
                    }
                }
                Ok(Ok(other)) => debug!(peer = %peer.fmt_short(), "replication refused: {other:?}"),
                Ok(Err(e)) => debug!(peer = %peer.fmt_short(), "replication failed: {e:#}"),
                Err(_) => debug!(peer = %peer.fmt_short(), "replication timed out"),
            }
        }
        confirmed
    }

    /// Send a crash report to a collector.
    pub async fn send_crash_report(
        &self,
        collector: EndpointId,
        report: serde_json::Value,
    ) -> Result<()> {
        let request = proto::Request::CrashReport { report };
        match proto::call(self.endpoint(), collector, &request).await? {
            proto::Response::Accepted => Ok(()),
            proto::Response::Unsupported => {
                bail!("{} does not collect crash reports", collector.fmt_short())
            }
            proto::Response::Refused { reason } => bail!("refused: {reason}"),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// Ask the process running this node to stop (it releases the data
    /// directory, e.g. for a daemon taking over).
    pub fn request_stop(&self) {
        self.stop.notify_waiters();
        self.stop.notify_one();
    }

    /// Resolves when [`Node::request_stop`] is called.
    pub async fn stop_requested(&self) {
        self.stop.notified().await;
    }

    /// A token for one agent run, valid until [`Node::end_runner_token`].
    pub fn new_runner_token(&self, run: RunnerRun) -> String {
        let mut bytes = [0u8; 16];
        let _ = getrandom::fill(&mut bytes);
        let token = hex::encode(bytes);
        if let Ok(mut tokens) = self.runner_tokens.lock() {
            tokens.insert(token.clone(), run);
        }
        token
    }

    /// The run a token belongs to, while it runs.
    pub fn runner_run(&self, token: &str) -> Option<RunnerRun> {
        self.runner_tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(token).cloned())
    }

    pub fn end_runner_token(&self, token: &str) {
        if let Ok(mut tokens) = self.runner_tokens.lock() {
            tokens.remove(token);
        }
    }

    pub fn is_runner_token(&self, token: &str) -> bool {
        self.runner_run(token).is_some()
    }

    async fn measure_storage(&self) {
        let dir = self.config.data_dir.clone();
        let bytes = tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0);
        self.storage_bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
        crate::crash::note_counts(
            bytes,
            self.known_peers().len() as u64,
            self.store.count().unwrap_or(0),
        );
    }

    /// Bytes used by the data directory (events, index, blobs), as last
    /// measured.
    pub fn storage_usage(&self) -> u64 {
        self.storage_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bytes opaque events may take: a share of `storage_max` (B4).
    pub fn opaque_quota(&self) -> Option<u64> {
        self.config
            .storage_max
            .map(|max| (max / 100).saturating_mul(OPAQUE_SHARE_PERCENT))
    }

    pub fn storage_state(&self) -> StorageState {
        storage_state(self.storage_usage(), self.config.storage_max)
    }

    /// Log, at most every 5 minutes, that storage is full.
    fn log_storage(&self, what: &str) {
        let Ok(mut last) = self.storage_logged.lock() else {
            return;
        };
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
            return;
        }
        *last = Some(std::time::Instant::now());
        warn!(
            used = %format_bytes(self.storage_usage()),
            max = %format_bytes(self.config.storage_max.unwrap_or(0)),
            "storage.max reached: {what} (raise storage.max or free space)"
        );
    }

    /// Bytes used by the blob store on disk, recomputed at most every 30 s.
    pub async fn blob_usage(&self) -> u64 {
        if let Ok(cache) = self.usage.lock()
            && let Some((at, bytes)) = *cache
            && at.elapsed() < Duration::from_secs(30)
        {
            return bytes;
        }
        let dir = self.config.data_dir.join("blobs");
        let bytes = tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0);
        if let Ok(mut cache) = self.usage.lock() {
            *cache = Some((std::time::Instant::now(), bytes));
        }
        bytes
    }

    /// Blobs fetched from other nodes from now on.
    pub fn subscribe_fetched(&self) -> tokio::sync::broadcast::Receiver<BlobRef> {
        self.fetched.subscribe()
    }

    /// Read a blob from the local store, if complete.
    pub async fn blob_bytes(&self, hash: &str) -> Result<Option<bytes::Bytes>> {
        let hash: Hash = hash.parse()?;
        if !self.blobs.has(hash).await? {
            return Ok(None);
        }
        Ok(Some(self.blobs.get_bytes(hash).await?))
    }

    /// Current gossip neighbors, across networks.
    pub fn neighbors(&self) -> Vec<EndpointId> {
        let mut all: Vec<EndpointId> = self
            .neighbors
            .lock()
            .map(|n| n.values().flatten().copied().collect())
            .unwrap_or_default();
        all.sort();
        all.dedup();
        all
    }

    /// Peers heard from recently (gossip neighbors, bootstrap peers, peers
    /// that contacted us), excluding ourselves.
    pub fn known_peers(&self) -> Vec<EndpointId> {
        let me = self.endpoint_id();
        let mut all: Vec<EndpointId> = self
            .peers
            .lock()
            .map(|p| p.keys().copied().filter(|id| *id != me).collect())
            .unwrap_or_default();
        all.sort();
        all
    }

    fn seen(&self, peer: EndpointId) {
        if let Ok(mut peers) = self.peers.lock() {
            if peers.len() >= 10_000 && !peers.contains_key(&peer) {
                // Forget the stalest peer.
                if let Some(oldest) = peers.iter().min_by_key(|(_, t)| **t).map(|(id, _)| *id) {
                    peers.remove(&oldest);
                }
            }
            peers.insert(peer, now_secs());
        }
    }

    fn add_hints(&self, hash: &str, providers: &[EndpointId]) {
        if let Ok(mut hints) = self.hints.lock() {
            if hints.len() >= 100_000 {
                hints.clear();
            }
            let entry = hints.entry(hash.to_string()).or_default();
            for p in providers {
                if !entry.contains(p) && entry.len() < 16 {
                    entry.push(*p);
                }
            }
        }
    }

    /// Where a blob probably is: announced providers first, then the known
    /// peers closest to its hash (where replication put it), then neighbors.
    fn providers_for(&self, blob: &BlobRef) -> Vec<EndpointId> {
        let mut out: Vec<EndpointId> = self
            .hints
            .lock()
            .ok()
            .and_then(|h| h.get(&blob.hash).cloned())
            .unwrap_or_default();
        if let Ok(hash) = crate::event::parse_hex::<32>(&blob.hash, "hash") {
            out.extend(proto::closest(
                &hash,
                &self.known_peers(),
                self.config.replication.saturating_mul(2),
            ));
        }
        out.extend(self.neighbors());
        let me = self.endpoint_id();
        let mut seen = HashSet::new();
        out.retain(|p| *p != me && seen.insert(*p));
        out
    }

    /// Make a blob local (downloading it from known peers if needed), then
    /// export it with [`Node::export_blob`].
    pub async fn fetch_and_export(&self, blob: &BlobRef) -> Result<Option<PathBuf>> {
        if blob.size > self.config.max_blob_bytes {
            bail!(
                "blob is larger than this node's {} byte limit",
                self.config.max_blob_bytes
            );
        }
        let hash: Hash = blob.hash.parse()?;
        if !self.blobs.has(hash).await? {
            let providers = self.providers_for(blob);
            if providers.is_empty() || !self.fetch_blob(blob, providers).await {
                return Ok(None);
            }
        }
        self.export_blob(blob).await
    }

    /// Copy a complete local blob to `<data dir>/inbox/<hash>/<name>`,
    /// read-only, and return the path. Received files are untrusted data:
    /// they are exported for reading, never executed (SPEC §2.11, §2.15).
    pub async fn export_blob(&self, blob: &BlobRef) -> Result<Option<PathBuf>> {
        if !is_safe_blob_name(&blob.name) {
            bail!("unsafe blob name {:?}", blob.name);
        }
        let hash: Hash = blob.hash.parse()?;
        if !self.blob_matches(blob).await {
            return Ok(None);
        }
        // Never follow a symlink on the way (security review D): the inbox
        // and its per-hash directory must be real directories, and the file
        // is written through a fresh, exclusively created temporary file.
        let inbox = std::path::absolute(self.config.data_dir.join("inbox"))?;
        let dir = inbox.join(hash.to_hex());
        real_dir(&inbox).await?;
        real_dir(&dir).await?;
        let path = dir.join(&blob.name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) if meta.file_type().is_file() => return Ok(Some(path)),
            Ok(_) => bail!("{} exists and is not a regular file", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        static EXPORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = EXPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(
            ".{}.{}.{n}.part",
            hash.to_hex(),
            std::process::id()
        ));
        // create_new: O_CREAT | O_EXCL, which never follows a symlink.
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
            .with_context(|| format!("create {}", tmp.display()))?;
        let copied = async {
            let mut reader = self.blobs.reader(hash);
            tokio::io::copy(&mut reader, &mut file).await?;
            file.sync_all().await?;
            anyhow::Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = copied {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        let mut perms = tokio::fs::metadata(&tmp).await?.permissions();
        perms.set_readonly(true);
        tokio::fs::set_permissions(&tmp, perms).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(Some(path))
    }

    async fn receive_loop(
        self: Arc<Self>,
        net: String,
        receiver: Arc<tokio::sync::Mutex<GossipReceiver>>,
    ) {
        let mut receiver = receiver.lock().await;
        loop {
            match receiver.next().await {
                Some(Ok(GossipEvent::Received(msg))) => {
                    self.seen(msg.delivered_from);
                    if let Err(e) = self
                        .handle_message(&net, &msg.content, msg.delivered_from)
                        .await
                    {
                        debug!(from = %msg.delivered_from, "dropped message: {e:#}");
                    }
                }
                Some(Ok(GossipEvent::NeighborUp(id))) => {
                    debug!(%net, peer = %id, "neighbor up");
                    if let Ok(mut n) = self.neighbors.lock() {
                        n.entry(net.clone()).or_default().insert(id);
                    }
                    self.seen(id);
                    let node = Arc::clone(&self);
                    let net = net.clone();
                    tokio::spawn(async move {
                        if node.peer_info(&id).is_none()
                            && let Err(e) = node.hello(id).await
                        {
                            debug!(peer = %id.fmt_short(), "hello failed: {e:#}");
                        }
                        match node.sync_from(id, &net).await {
                            Ok(n) => debug!(%net, peer = %id.fmt_short(), new = n, "synced"),
                            Err(e) => debug!(%net, peer = %id.fmt_short(), "sync failed: {e:#}"),
                        }
                    });
                }
                Some(Ok(GossipEvent::NeighborDown(id))) => {
                    debug!(%net, peer = %id, "neighbor down");
                    if let Ok(mut n) = self.neighbors.lock() {
                        n.entry(net.clone()).or_default().remove(&id);
                    }
                }
                Some(Ok(GossipEvent::Lagged)) => warn!(%net, "gossip receiver lagged"),
                Some(Err(e)) => {
                    warn!(%net, "gossip error: {e:#}");
                    break;
                }
                None => break,
            }
        }
        debug!(%net, "gossip receive loop ended");
    }

    async fn handle_message(&self, net: &str, bytes: &[u8], from: EndpointId) -> Result<()> {
        // Untrusted: parse under the strict profile (ADR 0010, A3).
        let envelope: Envelope = crate::strict_json::from_slice(bytes)?;
        if envelope.ev < 1 {
            bail!("bad envelope version {}", envelope.ev);
        }
        let Some((event, new)) = self.accept_value(envelope.event)? else {
            return Ok(());
        };
        if event.net() != net {
            bail!("event for net {:?} on topic {net:?}", event.net());
        }
        if let (true, AnyEvent::Known(event)) = (new, &event) {
            let mut providers: Vec<EndpointId> = envelope
                .providers
                .iter()
                .take(MAX_PROVIDERS_PER_ENVELOPE)
                .filter_map(|p| p.parse().ok())
                .collect();
            providers.push(from);
            for blob in &event.blobs {
                self.add_hints(&blob.hash, &providers);
            }
        }
        Ok(())
    }

    /// Verify an untrusted wire event, apply node policy, store it.
    /// Events of versions this node does not understand are verified
    /// against the invariant core and stored opaque (ADR 0007). Returns the
    /// event and whether it is new; `None` if the policy rejects it.
    fn accept_value(&self, value: serde_json::Value) -> Result<Option<(AnyEvent, bool)>> {
        let event = AnyEvent::from_value(value)?;
        if !self.senders.contains_key(event.net()) {
            bail!("not a member of network {:?}", event.net());
        }
        // Proof of work grows with size (ADR 0010, limits): node policy.
        let size = match &event {
            AnyEvent::Known(e) => e.canonical_size()?,
            AnyEvent::Opaque(r) => serde_json_canonicalizer::to_vec(&r.wire)?.len(),
        };
        if size > crate::event::MAX_EVENT_BYTES {
            bail!("event is {size} bytes canonicalized, above the relay limit");
        }
        // Opaque events of a version this node does not understand (B4):
        // relayed only a little ahead of what we understand, with more
        // proof of work, and within a share of storage.max.
        let unknown_version = match &event {
            AnyEvent::Opaque(r) => !understood(r.v),
            AnyEvent::Known(_) => false,
        };
        if let AnyEvent::Opaque(r) = &event
            && unknown_version
            && r.v > UNDERSTOOD_VERSIONS.1.saturating_add(RELAY_VERSIONS_AHEAD)
        {
            bail!(
                "event version {} is too far ahead of this node ({} understood): not relayed",
                r.v,
                UNDERSTOOD_VERSIONS.1
            );
        }
        // A proof of work of an algorithm this node does not know is not
        // measured, nor taken as no work: the event is opaque (`from_value`)
        // and only the opaque quota below bounds it (ADR 0010, PoW).
        if let Some(work) = event.pow_work() {
            let mut required = crate::event::required_pow_bits(self.config.min_pow_bits, size);
            if unknown_version {
                required = required.saturating_add(OPAQUE_EXTRA_POW_BITS);
            }
            if work < required {
                bail!(
                    "insufficient proof of work: {work} bits for {size} bytes, {required} required"
                );
            }
        }
        if event.ts() > now_secs().saturating_add(MAX_CLOCK_SKEW_SECS) {
            bail!("event timestamp is in the future");
        }
        if self.storage_state() == StorageState::OverCeiling {
            self.log_storage("refusing events from others too");
            bail!("storage full");
        }
        if matches!(event, AnyEvent::Opaque(_))
            && let Some(quota) = self.opaque_quota()
            && self.store.opaque_bytes().saturating_add(size as u64) > quota
            && self.store.get_raw(event.id())?.is_none()
        {
            self.log_storage("refusing opaque events (their quota is full)");
            bail!("opaque event quota reached");
        }
        let inserted = match &event {
            AnyEvent::Known(e) => self.store.insert(e, now_secs())?,
            AnyEvent::Opaque(r) => {
                debug!(id = %r.id, v = r.v, "storing opaque event");
                self.store.insert_raw(r, now_secs())?
            }
        };
        let new = inserted != Inserted::Duplicate;
        Ok(Some((event, new)))
    }

    /// Store and broadcast an event that is already signed (any version),
    /// e.g. a repost or an event produced by another tool.
    pub async fn publish_signed(&self, value: serde_json::Value) -> Result<AnyEvent> {
        let Some((event, _)) = self.accept_value(value.clone())? else {
            bail!("rejected by node policy");
        };
        let Some(sender) = self.senders.get(event.net()) else {
            bail!("this node has not joined network {:?}", event.net());
        };
        let envelope = Envelope::new(value, self.endpoint_id());
        sender
            .broadcast(serde_json::to_vec(&envelope)?.into())
            .await?;
        Ok(event)
    }

    /// Exchange `hello` with a peer and remember what it speaks.
    pub async fn hello(&self, peer: EndpointId) -> Result<NodeInfo> {
        // GREASE a whole request type now and then: peers must answer
        // `unsupported` and keep going.
        if crate::grease::roll() {
            let request = serde_json::json!({ "type": crate::grease::name() });
            match proto::call_value(self.endpoint(), peer, request).await {
                Ok(Response::Unsupported) => {}
                other => debug!(peer = %peer.fmt_short(), "GREASE request answered with {other:?}"),
            }
        }
        let request = Request::Hello {
            info: NodeInfo::ours_greased(),
        };
        let mut info = match proto::call(self.endpoint(), peer, &request).await? {
            Response::Hello { info } => info,
            other => bail!("unexpected hello response: {other:?}"),
        };
        crate::grease::strip(&mut info.features);
        if !NodeInfo::ours().shares_event_version(&info) {
            warn!(
                peer = %peer.fmt_short(),
                software = %info.software,
                "peer understands event versions {}..={}, we understand {}..={}; \
                 its events are relayed as opaque data. Consider upgrading.",
                info.events_min, info.events_max,
                NodeInfo::ours().events_min, NodeInfo::ours().events_max,
            );
        }
        if let Ok(mut infos) = self.peer_info.lock() {
            if infos.len() >= 10_000 {
                infos.clear();
            }
            infos.insert(peer, info.clone());
        }
        Ok(info)
    }

    /// What a peer said in `hello`, if we talked to it.
    pub fn peer_info(&self, peer: &EndpointId) -> Option<NodeInfo> {
        self.peer_info.lock().ok()?.get(peer).cloned()
    }

    /// Serve a `/wats/1` request from `peer`.
    pub(crate) async fn handle_request(&self, peer: EndpointId, request: Request) -> Response {
        self.seen(peer);
        let kind = match &request {
            Request::Hello { .. } => Some(crate::limits::Kind::Hello),
            Request::Sync { .. } => Some(crate::limits::Kind::Sync),
            Request::Replicate { .. } => Some(crate::limits::Kind::Replicate),
            Request::CrashReport { .. } => Some(crate::limits::Kind::Report),
            Request::Unsupported => None,
        };
        if let Some(kind) = kind
            && !self.limiter.allow(peer, kind)
        {
            return Response::Refused {
                reason: format!("rate limited ({}): retry later", kind.as_str()),
            };
        }
        match request {
            Request::Hello { mut info } => {
                crate::grease::strip(&mut info.features);
                if let Ok(mut infos) = self.peer_info.lock() {
                    infos.insert(peer, info);
                }
                Response::Hello {
                    info: NodeInfo::ours_greased(),
                }
            }
            Request::Unsupported => Response::Unsupported,
            Request::Sync {
                net,
                after_ts,
                after_id,
                limit,
            } => {
                if !self.senders.contains_key(&net) {
                    return Response::Refused {
                        reason: format!("not a member of network {net:?}"),
                    };
                }
                let limit = limit.clamp(1, proto::MAX_SYNC_PAGE);
                match self
                    .store
                    .page_after(&net, after_ts, &after_id, limit as usize)
                {
                    Ok(page) => Response::SyncPage {
                        more: page.len() == limit as usize,
                        events: page,
                    },
                    Err(e) => Response::Refused {
                        reason: format!("{e:#}"),
                    },
                }
            }
            Request::CrashReport { report } => {
                if !self.config.report_accept {
                    return Response::Unsupported;
                }
                match crate::crash::accept(
                    &self.config.data_dir,
                    &peer.to_string(),
                    &report,
                    CRASH_REPORTS_PER_DAY,
                    CRASH_REPORT_DAYS,
                ) {
                    Ok(path) => {
                        info!(peer = %peer.fmt_short(), path = %path.display(), "crash report received");
                        Response::Accepted
                    }
                    Err(reason) => Response::Refused { reason },
                }
            }
            Request::Replicate { event, hashes } => {
                let event = match self.accept_value(event) {
                    Ok(Some((AnyEvent::Known(event), _))) => event,
                    // Opaque events are stored; their blobs are unknown to us.
                    Ok(Some((AnyEvent::Opaque(_), _))) => {
                        return Response::Replicated { hashes: vec![] };
                    }
                    Ok(None) => {
                        return Response::Refused {
                            reason: "rejected by node policy".into(),
                        };
                    }
                    Err(e) => {
                        return Response::Refused {
                            reason: format!("{e:#}"),
                        };
                    }
                };
                let mut stored = Vec::new();
                for blob in event.blobs.iter().filter(|b| hashes.contains(&b.hash)) {
                    if blob.url.is_some() || blob.size > self.config.max_blob_bytes {
                        continue;
                    }
                    self.add_hints(&blob.hash, &[peer]);
                    // At most a few fetches at once for one peer.
                    let Some(_permit) = self.limiter.blob_fetch(peer) else {
                        continue;
                    };
                    if self.fetch_blob(blob, vec![peer]).await {
                        stored.push(blob.hash.clone());
                    }
                }
                Response::Replicated { hashes: stored }
            }
        }
    }

    /// Catch up on a network from a peer: page through its events after our
    /// sync watermark. Returns how many new events were stored.
    pub async fn sync_from(&self, peer: EndpointId, net: &str) -> Result<usize> {
        let started = now_secs();
        let watermark = self.store.sync_watermark(net)?;
        let (mut after_ts, mut after_id) =
            (watermark.saturating_sub(MAX_CLOCK_SKEW_SECS), String::new());
        let mut new: usize = 0;
        for _ in 0..10_000 {
            let request = Request::Sync {
                net: net.to_string(),
                after_ts,
                after_id: after_id.clone(),
                limit: proto::MAX_SYNC_PAGE,
            };
            let (events, more) = match proto::call(self.endpoint(), peer, &request).await? {
                Response::SyncPage { events, more } => (events, more),
                other => bail!("unexpected sync response: {other:?}"),
            };
            let mut last = None;
            for value in events {
                let (Some(ts), Some(id)) = (
                    value.get("ts").and_then(serde_json::Value::as_u64),
                    value
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                ) else {
                    bail!("peer sent a malformed sync page");
                };
                if value.get("net").and_then(serde_json::Value::as_str) != Some(net)
                    || (ts, id.as_str()) <= (after_ts, after_id.as_str())
                {
                    bail!("peer sent an out-of-order sync page");
                }
                match self.accept_value(value) {
                    Ok(Some((_, true))) => new = new.saturating_add(1),
                    Ok(_) => {}
                    Err(e) => debug!(%id, "sync: rejected event: {e:#}"),
                }
                (after_ts, after_id) = (ts, id.clone());
                last = Some(id);
            }
            if last.is_none() || !more {
                break;
            }
        }
        self.store.set_sync_watermark(net, started)?;
        Ok(new)
    }

    /// Download a blob from providers. BLAKE3 verified streaming makes the
    /// content match the hash; the content itself stays untrusted data and
    /// is never executed (SPEC §2.15).
    async fn fetch_blob(&self, blob: &BlobRef, providers: Vec<EndpointId>) -> bool {
        let Ok(hash) = blob.hash.parse::<Hash>() else {
            return false;
        };
        if self.storage_state() != StorageState::Ok {
            self.log_storage("not fetching blobs or replicas");
            return false;
        }
        if let Some(quota) = self.config.blob_quota {
            let used = self.blob_usage().await;
            if used.saturating_add(blob.size) > quota {
                warn!(
                    hash = %blob.hash,
                    used, quota, "blob quota reached, not fetching (raise --blob-quota)"
                );
                return false;
            }
        }
        // B7: one provider at a time, skipping those that failed lately.
        let candidates: Vec<EndpointId> = providers
            .into_iter()
            .filter(|p| !self.provider_backing_off(p))
            .take(MAX_PROVIDER_ATTEMPTS)
            .collect();
        if candidates.is_empty() {
            debug!(hash = %blob.hash, "no provider to fetch from (all backing off)");
            return false;
        }
        // B10: ask for the declared size only, so a blob larger than its
        // event says never costs more than what was declared and checked
        // against the limits.
        let chunks = blob.size.div_ceil(1024).max(1);
        let request = GetRequest::builder()
            .root(ChunkRanges::chunks(..chunks))
            .build(hash);
        let started = tokio::time::Instant::now();
        for provider in candidates {
            let left = Duration::from_secs(120).saturating_sub(started.elapsed());
            if left.is_zero() {
                warn!(hash = %blob.hash, "blob fetch timed out");
                return false;
            }
            let attempt = async {
                self.downloader
                    .download(request.clone(), vec![provider])
                    .await?;
                anyhow::Ok(())
            };
            match tokio::time::timeout(left.min(Duration::from_secs(60)), attempt).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    debug!(hash = %blob.hash, %provider, "blob fetch failed: {e:#}");
                    self.provider_failed(provider);
                    continue;
                }
                Err(_) => {
                    debug!(hash = %blob.hash, %provider, "blob fetch timed out");
                    self.provider_failed(provider);
                    continue;
                }
            }
            self.provider_ok(provider);
            // The content is verified against the hash; its size must also
            // be the one the event declared (B10).
            if !self.blob_matches(blob).await {
                return false;
            }
            if let Err(e) = self.pin(hash).await {
                warn!(hash = %blob.hash, "blob not pinned: {e:#}");
                return false;
            }
            info!(hash = %blob.hash, name = %blob.name, "blob fetched");
            let _ = self.fetched.send(blob.clone());
            return true;
        }
        warn!(hash = %blob.hash, "blob fetch failed from every provider tried");
        false
    }

    /// Whether the local copy of `blob` is complete and of the size its
    /// event declared (B10). Logs a mismatch: the event lies about its blob.
    async fn blob_matches(&self, blob: &BlobRef) -> bool {
        let Ok(hash) = blob.hash.parse::<Hash>() else {
            return false;
        };
        match self.blobs.status(hash).await {
            Ok(BlobStatus::Complete { size }) if size == blob.size => true,
            Ok(BlobStatus::Complete { size }) => {
                warn!(hash = %blob.hash, declared = blob.size, size, "blob size differs from its declaration: not used");
                false
            }
            Ok(BlobStatus::NotFound) => false,
            Ok(BlobStatus::Partial { .. }) => {
                warn!(hash = %blob.hash, declared = blob.size, "blob incomplete or larger than declared: not used");
                false
            }
            Err(e) => {
                warn!(hash = %blob.hash, "blob status: {e:#}");
                false
            }
        }
    }

    fn provider_backing_off(&self, provider: &EndpointId) -> bool {
        self.provider_backoff
            .lock()
            .ok()
            .and_then(|b| {
                b.get(provider)
                    .map(|(_, until)| std::time::Instant::now() < *until)
            })
            .unwrap_or(false)
    }

    fn provider_failed(&self, provider: EndpointId) {
        if let Ok(mut backoff) = self.provider_backoff.lock() {
            if backoff.len() >= 10_000 {
                let now = std::time::Instant::now();
                backoff.retain(|_, (_, until)| *until > now);
            }
            let failures = backoff
                .get(&provider)
                .map_or(0, |(n, _)| *n)
                .saturating_add(1);
            // `provider_backoff` is at most an hour: `checked_add` cannot fail.
            if let Some(until) = std::time::Instant::now().checked_add(provider_backoff(failures)) {
                backoff.insert(provider, (failures, until));
            }
        }
    }

    fn provider_ok(&self, provider: EndpointId) {
        if let Ok(mut backoff) = self.provider_backoff.lock() {
            backoff.remove(&provider);
        }
    }

    /// Persist the peers that answered `hello`, with the addresses the
    /// endpoint knows for them now (B1).
    pub async fn save_peer_cache(&self) {
        let last_seen = self.peers.lock().map(|p| p.clone()).unwrap_or_default();
        let met: Vec<EndpointId> = self
            .peer_info
            .lock()
            .map(|p| p.keys().copied().collect())
            .unwrap_or_default();
        let mut fresh = Vec::new();
        for id in met {
            let Some(info) = self.endpoint().remote_info(id).await else {
                continue;
            };
            let addr = EndpointAddr::from_parts(id, info.into_addrs().map(TransportAddr::from));
            let seen = last_seen.get(&id).copied().unwrap_or_else(now_secs);
            fresh.push(crate::peer_cache::CachedPeer::from_addr(&addr, seen));
        }
        if fresh.is_empty() {
            return;
        }
        let (dir, me) = (self.config.data_dir.clone(), self.endpoint_id());
        let saved = tokio::task::spawn_blocking(move || {
            crate::peer_cache::save(&dir, me, now_secs(), fresh)
        })
        .await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("peer cache not saved: {e:#}"),
            Err(e) => warn!("peer cache not saved: {e}"),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.save_peer_cache().await;
        self.gossip.shutdown().await.ok();
        self.router.shutdown().await?;
        // The router shuts the blob protocol down, which also closes the store.
        self.blobs.shutdown().await.ok();
        // Background tasks may still hold the node; release the data
        // directory explicitly.
        if let Some(lock) = self.lock.lock().ok().and_then(|mut l| l.take()) {
            lock.unlock().ok();
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod storage_tests {
    use super::*;

    #[test]
    fn provider_backoff_doubles_up_to_an_hour() {
        assert_eq!(provider_backoff(1), Duration::from_secs(30));
        assert_eq!(provider_backoff(2), Duration::from_secs(60));
        assert_eq!(provider_backoff(4), Duration::from_secs(240));
        assert_eq!(provider_backoff(10), Duration::from_secs(3600));
        assert_eq!(provider_backoff(u32::MAX), Duration::from_secs(3600));
    }

    #[test]
    fn opaque_events_of_understood_versions_are_revalidated() {
        use ed25519_dalek::SigningKey;
        let store = Store::open_in_memory().unwrap();
        let key = SigningKey::from_bytes(&[3; 32]);
        // A valid v1 event a previous release stored opaque.
        let event = crate::event::Draft {
            net: "public".into(),
            kind: Some(crate::event::Kind::Note),
            title: Some("t".into()),
            text: "now understood".into(),
            ..Default::default()
        }
        .sign(&key, 1, 0)
        .unwrap();
        let raw = crate::wire::RawEvent::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        store.insert_raw(&raw, 1).unwrap();
        // And one that breaks the v1 rules (an unknown kind).
        let header = serde_json::json!({"v": 1, "ts": 1, "net": "public", "thread": null,
            "parent": null, "kind": "poll"});
        let body = serde_json::json!({"text": "x", "tags": [], "data": {}, "blobs": [],
            "refs": [], "title": "t"});
        let bad = crate::wire::sign_value(
            header.as_object().unwrap().clone(),
            body.as_object().unwrap().clone(),
            &key,
            0,
        )
        .unwrap();
        let bad_raw = crate::wire::RawEvent::from_value(bad).unwrap();
        store.insert_raw(&bad_raw, 1).unwrap();
        assert!(store.opaque_bytes() > 0);

        revalidate_opaque(&store).unwrap();
        assert_eq!(
            store.get(&event.id).unwrap().unwrap().event.text,
            "now understood"
        );
        assert!(
            store.get_raw(&bad_raw.id).unwrap().is_none(),
            "invalid: dropped"
        );
        assert_eq!(store.opaque_bytes(), 0);
    }

    #[test]
    fn states_and_formats() {
        assert_eq!(storage_state(5, None), StorageState::Ok);
        assert_eq!(storage_state(99, Some(100)), StorageState::Ok);
        assert_eq!(storage_state(100, Some(100)), StorageState::Full);
        assert_eq!(storage_state(109, Some(100)), StorageState::Full);
        assert_eq!(storage_state(110, Some(100)), StorageState::OverCeiling);
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(10 << 30), "10.0 GiB");
        assert_eq!(format_bytes(1536 << 20), "1.5 GiB");
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_ignore_unknown_and_grease_fields() {
        let bytes =
            br#"{"ev":1,"event":{"x":1},"providers":[],"_grease_1a2b":[1,"x"],"route":"later"}"#;
        let envelope: Envelope = crate::strict_json::from_slice(bytes).unwrap();
        assert_eq!(envelope.ev, 1);
        assert_eq!(envelope.extra.len(), 2);
        crate::grease::set_rate(1);
        let e = Envelope::new(
            serde_json::json!({}),
            SecretKey::from_bytes(&[1; 32]).public(),
        );
        crate::grease::set_rate(crate::grease::DEFAULT_RATE);
        assert!(e.extra.keys().all(|k| crate::wire::is_grease(k)) && e.extra.len() == 1);
    }
}
