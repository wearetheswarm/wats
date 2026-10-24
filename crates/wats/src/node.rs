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

pub mod durability;
pub mod history;
pub mod links;
pub mod proof;
pub mod rotation;

use crate::bootstrap::BootstrapEntry;
use crate::event::{BlobRef, Draft, Event, Kind, is_safe_blob_name};
use crate::identity::{DEFAULT_PROFILE, RootSecret, author_id};
use crate::proto::{self, NodeInfo, Request, Response, WatsProtocol};
use crate::store::{DropOutcome, DropReason, Inserted, Store};
use crate::supervise::supervise;
use crate::wire::{AnyEvent, RELAY_VERSIONS_AHEAD, UNDERSTOOD_VERSIONS};

/// The public network id (SPEC §2.4). Provisional, ADR 0004.
pub const PUBLIC_NET: &str = "public";

/// Gossip messages are capped by Iroh. v1 events are at most 16 KiB
/// canonical (32 KiB of raw input); the cap leaves room for the envelope
/// and for later versions. Every node must use the same value.
pub const GOSSIP_MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Default of `gossip.active_view`: the gossip neighbors each node keeps
/// (HyParView's active view, its default).
pub const GOSSIP_ACTIVE_VIEW: usize = 5;
/// Default of `gossip.ping`: how often a node pings its gossip neighbors
/// ([`links`]).
pub const GOSSIP_PING_EVERY: Duration = Duration::from_secs(60);

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
    /// Test only: no IP transport and no address lookup; peers are reached
    /// only through the relay at `http://127.0.0.1:<port>` (plain HTTP:
    /// the relayed QUIC traffic is end-to-end encrypted anyway). For
    /// hermetic chaos tests where the relay is load-bearing. Never set
    /// from the command line.
    LocalRelay { port: u16 },
}

impl NetworkMode {
    /// Whether the node stays on this machine (loopback or a local relay):
    /// no infrastructure services, no peer reached through the Internet.
    pub fn is_local(self) -> bool {
        matches!(self, NetworkMode::Loopback | NetworkMode::LocalRelay { .. })
    }
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
    /// Bootstrap entries resolved by DNS at startup and at every
    /// reconnection attempt, ids pinned (ADR 0008).
    pub bootstrap: Vec<BootstrapEntry>,
    /// The bootstrap nodes compiled into the binary: dialed at startup only
    /// when the peer cache is empty, or later when no peer answered
    /// (security review B1).
    pub default_bootstrap: Vec<BootstrapEntry>,
    /// How bootstrap entries are resolved: DNS (tests inject failures).
    pub resolver: crate::bootstrap::Resolver,
    /// How fast the node dials again with no connected peer
    /// ([`crate::reconnect`]).
    pub reconnect: crate::reconnect::Backoff,
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
    /// Proof-of-work difficulty mined on our own posts. `None`: the network
    /// schedule at the post's time, plus the new-author bits while this
    /// node has known our author for less than a few days (ADR 0010).
    pub pow_bits: Option<u32>,
    /// Minimum proof-of-work difficulty accepted from others. `None`: the
    /// network schedule at the event's time, plus the new-author bits on
    /// events pushed to us (gossip, replicate, publish), not on events we
    /// pull (sync). `Some`: a flat difficulty (tests, private networks).
    pub min_pow_bits: Option<u32>,
    /// The network's base difficulty keyed to `ts`
    /// ([`crate::event::POW_SCHEDULE`]), used when `pow_bits` or
    /// `min_pow_bits` is `None`.
    pub pow_schedule: Vec<(u64, u32)>,
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
    /// Share of `storage_max` for the event store, in percent
    /// (`storage.events_share`, ADR 0025 §2).
    pub events_share: u64,
    /// Budget of the initial fill and of each catch-up
    /// (`sync.initial_max`, ADR 0025 §2).
    pub initial_max: crate::settings::InitialMax,
    /// How the history task runs ([`history::Tuning`]); tests shorten it.
    pub history: history::Tuning,
    /// Gossip neighbors kept (`gossip.active_view`).
    pub gossip_active_view: usize,
    /// How often the gossip neighbors are pinged (`gossip.ping`; zero:
    /// never, and no link check nor diversity join, see [`links`]).
    pub gossip_ping: Duration,
    /// Delete the blobs nothing keeps, every `blob_gc_interval`
    /// (`storage.gc`): no tag and listed by no kept event, e.g. the files
    /// of a dropped body (ADR 0020 §2) or of a fetch that failed half-way.
    pub blob_gc: bool,
    /// How often the blob collection runs: [`BLOB_GC_INTERVAL`]; tests
    /// shorten it.
    pub blob_gc_interval: Duration,
    /// Profiles whose posts through MCP are held for review instead of
    /// published (`agent.review`: the agent's profile). Non-empty also
    /// holds the posts of every runner session.
    pub review_profiles: Vec<String>,
    /// Held drafts older than this expire (`agent.review_ttl`).
    pub review_ttl: Duration,
    /// A desktop notification when a draft is held (`agent.notify`).
    pub review_notify: bool,
    /// Agent posts carry `data.via`, their declared provenance
    /// (`node.declare_via`, ADR 0036).
    pub declare_via: bool,
    /// How this node runs: a daemon (`wats`), a background node started by
    /// `wats mcp`, or inside one MCP process.
    pub role: NodeRole,
    /// The agent runners this node runs (ADR 0021), one per agent:
    /// "claude as alice-agent".
    pub runners: Vec<String>,
    /// Every agent of config.toml and of this run, for `status`.
    pub agents: Vec<crate::agent::AgentInfo>,
    /// How long to wait for the data directory lock (a daemon taking over
    /// from a background node). Zero fails at once.
    pub lock_wait: Duration,
    /// Collect crash reports from other nodes (`report.accept`).
    pub report_accept: bool,
    /// Per-peer rate limits on requests (`[limits]`).
    pub limits: crate::limits::Limits,
    /// Where this node sends its crash reports (`report.collector`).
    pub report_collector: Option<EndpointId>,
    /// The inbox crash reports are sealed to (`report.inbox`, else the
    /// compiled-in maintainers' key). When set, reports go there instead of
    /// to `report_collector` (ADR 0015 §5).
    pub report_inbox: Option<crate::inbox::InboxPublic>,
    /// Send them without asking (`report.crashes`).
    pub report_auto: bool,
    /// Network arguments that start the same node again (`start_daemon`).
    pub relaunch_args: Vec<std::ffi::OsString>,
    /// The log file this node's stderr goes to, when it runs detached
    /// (ADR 0023): recorded in the lock file for `wats ps` and `wats
    /// attach`.
    pub log_file: Option<PathBuf>,
    /// The per-user registry of running nodes (ADR 0023), if any: this
    /// node records its data directory there while it holds the lock.
    pub registry: Option<PathBuf>,
    /// Replication factor k for blobs: how many peers are asked to hold a
    /// blob at publish time (SPEC §2.5, §7), and how many replicas repair
    /// keeps (ADR 0029).
    pub replication: usize,
    /// Replica repair, knowledge replicas and last-copy protection (ADR
    /// 0029); tests shorten it.
    pub durability: durability::Tuning,
    /// Stored v2 `submit` events older than this are dropped, and older
    /// ones refused (`submit.retention`, ADR 0015 §5). `None` keeps them.
    pub submit_retention: Option<Duration>,
    /// The daily release check and `update.auto` (`[update]`). Off unless
    /// set; never in loopback mode.
    pub update: crate::update::UpdatePolicy,
    /// Guardian halts (ADR 0016): `halt.accept`, `halt.serve`, and the
    /// compiled guardian set (tests inject their own; never config.toml).
    pub halt: crate::halt::HaltConfig,
    /// Warn when the node has had no connected peer for this long
    /// (`health.no_peers_after`); `None` never does.
    pub no_peers_after: Option<Duration>,
    /// An ask with no answer after this long (or past its `data.deadline`)
    /// is listed as unanswered for its author (`ask.unanswered_after`,
    /// ADR 0018 §4); `None` never.
    pub unanswered_after: Option<Duration>,
    /// Runners without the capability an ask names (`cap:<x>`) wake for it
    /// only when no agent claimed or answered it within this
    /// (`ask.cap_grace`, ADR 0022 §1).
    pub cap_grace: Duration,
    /// The default cap on an ask's responders, from distinct nodes
    /// (`ask.max_responders`, ADR 0022 §2.2); `None`: none unless the ask
    /// sets `data.max`.
    pub max_responders: Option<usize>,
    /// The random delay before a runner wakes for an ask, then counts
    /// again, drawn from this range (ADR 0022 §2.3): 0 to
    /// [`crate::delivery::CLAIM_JITTER`]. Not a config key: tests shorten
    /// it, or fix it per node (`d..=d`) to order the runners.
    pub claim_jitter: std::ops::RangeInclusive<Duration>,
    /// Labelers (author ids) whose `erasure` labels this node applies
    /// (`labels.subscribe`, ADR 0020 §5.2): opt-in, none by default.
    pub erasure_labelers: Vec<String>,
    /// The human's OpenClaw, told when its swarm asks get replies (ADR
    /// 0018 §7); set when the hook token exists.
    pub openclaw_notify: Option<crate::openclaw::Notify>,
    /// A desktop notification when someone answers or clarifies an ask of
    /// the human's (`notify.desktop`); `None`: off (headless, loopback).
    pub desktop_notify: Option<crate::desktop::Notifier>,
    /// Reputation (ADR 0019 §2): `reputation.threshold`, and how long a
    /// network's table is reused once the store changed.
    pub reputation: crate::reputation::Policy,
}

impl NodeConfig {
    /// An infrastructure node (the router): it collects crash reports
    /// (`report.accept`) or serves halt events (`halt.serve`). It is not
    /// told to run an agent.
    pub fn is_infra(&self) -> bool {
        self.report_accept || self.halt.serve
    }

    /// The agent runners, joined ("claude as alice-agent, openclaw as
    /// alice-openclaw"); `None` when this node runs none.
    pub fn runner(&self) -> Option<String> {
        (!self.runners.is_empty()).then(|| self.runners.join(", "))
    }

    /// The profiles the agent runners post as (each runner is "claude as
    /// alice-agent").
    pub fn runner_profiles(&self) -> Vec<&str> {
        self.runners
            .iter()
            .filter_map(|r| r.rsplit_once(" as ").map(|(_, profile)| profile))
            .collect()
    }

    /// Whether `profile` is one an agent runner posts as.
    pub fn is_runner_profile(&self, profile: &str) -> bool {
        self.runner_profiles().contains(&profile)
    }

    /// Whether `profile` is an agent's, not the human's: an agent runner's,
    /// a profile held for review, or the profile of an agent of
    /// config.toml (running or not).
    pub fn is_agent_profile(&self, profile: &str) -> bool {
        self.is_runner_profile(profile)
            || self.review_profiles.iter().any(|p| p == profile)
            || self.agents.iter().any(|a| a.profile == profile)
    }

    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            mode: NetworkMode::Internet,
            infra: crate::infra::Infra::default(),
            nets: vec![PUBLIC_NET.to_string()],
            peers: Vec::new(),
            profile: DEFAULT_PROFILE.to_string(),
            pow_bits: None,
            min_pow_bits: None,
            pow_schedule: crate::event::POW_SCHEDULE.to_vec(),
            max_blob_bytes: 256 * 1024 * 1024,
            blob_quota: None,
            storage_max: None,
            events_share: crate::settings::DEFAULT_EVENTS_SHARE,
            initial_max: Default::default(),
            history: Default::default(),
            gossip_active_view: GOSSIP_ACTIVE_VIEW,
            gossip_ping: GOSSIP_PING_EVERY,
            blob_gc: true,
            blob_gc_interval: BLOB_GC_INTERVAL,
            review_profiles: Vec::new(),
            review_ttl: Duration::from_secs(24 * 3600),
            review_notify: false,
            declare_via: true,
            role: NodeRole::Daemon,
            runners: Vec::new(),
            agents: Vec::new(),
            lock_wait: Duration::ZERO,
            relaunch_args: Vec::new(),
            log_file: None,
            registry: None,
            report_accept: false,
            limits: Default::default(),
            report_collector: None,
            report_inbox: None,
            report_auto: false,
            replication: 3,
            durability: Default::default(),
            submit_retention: Some(DEFAULT_SUBMIT_RETENTION),
            update: Default::default(),
            halt: Default::default(),
            no_peers_after: Some(crate::health::DEFAULT_NO_PEERS_AFTER),
            unanswered_after: Some(crate::unanswered::DEFAULT_AFTER),
            cap_grace: crate::delivery::DEFAULT_CAP_GRACE,
            max_responders: Some(crate::delivery::DEFAULT_MAX_RESPONDERS),
            claim_jitter: Duration::ZERO..=crate::delivery::CLAIM_JITTER,
            erasure_labelers: Vec::new(),
            openclaw_notify: None,
            desktop_notify: None,
            reputation: Default::default(),
            bootstrap: Vec::new(),
            default_bootstrap: Vec::new(),
            resolver: Default::default(),
            reconnect: Default::default(),
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

/// `name` with `-n` before its extension: `chart.png` → `chart-1.png`,
/// `notes` → `notes-1`.
pub fn numbered(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{name}-{n}"),
    }
}

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

/// Author ids of the profiles a node's config names (default profile,
/// agents, runners, review), from its secret: its own profiles, as far as
/// a command without the running node can tell (`wats reputation`).
pub fn configured_authors(root: &RootSecret, config: &NodeConfig) -> HashSet<String> {
    configured_profiles(root, config).into_keys().collect()
}

/// The authors of [`configured_authors`], each with its profile name.
pub fn configured_profiles(root: &RootSecret, config: &NodeConfig) -> HashMap<String, String> {
    let profiles = std::iter::once(config.profile.as_str())
        .chain(config.runner_profiles())
        .chain(config.agents.iter().map(|a| a.profile.as_str()))
        .chain(config.review_profiles.iter().map(String::as_str));
    profiles
        .filter(|p| is_valid_profile(p))
        .map(|profile| (author_id(&root.profile_key(profile)), profile.to_string()))
        .collect()
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
    /// The profile of the agent the run is for (empty: unknown).
    pub profile: String,
    /// Directories it may share files from besides its session's working
    /// directory: a Codex run's own image directory
    /// (`$CODEX_HOME/generated_images/<thread id>`), registered by the
    /// runner when the thread starts.
    pub file_roots: Vec<PathBuf>,
    /// What the runner knows of what produces its posts (ADR 0036).
    pub via: crate::via::Via,
}

/// What a node did about a guardian halt (ADR 0016 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltOutcome {
    /// No halt acted on.
    Running,
    /// `halt.serve`: up, serving halt events only; agent and MCP stopped.
    Serving,
    /// A node on other networks too: left `public`, keeps the others.
    LeftPublic,
    /// Relayed the halt and wrote the marker: the process must stop
    /// (agent, MCP sessions) and exit with status 0.
    Exit(crate::halt::Decision),
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

/// ADR 0012's measure of a data directory with no node running on it:
/// [`dir_size`] less the free pages of its event store, which the file
/// keeps but reuses (ADR 0025 §7).
pub fn data_usage(dir: &Path) -> u64 {
    let free = Store::open_read_only_with(&dir.join("events.sqlite"), Duration::from_millis(200))
        .map(|store| store.free_bytes())
        .unwrap_or(0);
    dir_size(dir).saturating_sub(free)
}

/// How an event reached this node, for the proof-of-work policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Gossip, a replicate request, or a local publish: live events,
    /// subject to the new-author bits.
    Pushed,
    /// A sync page this node asked for: history, at the schedule only.
    Pulled,
    /// A `get` on demand, for a `read` (ADR 0025 §6): history, as pulled,
    /// and the only way an evicted body comes back (ADR 0020 decision 4).
    Fetched,
}

/// Largest canonical size of an event of a version this node does not
/// understand that it stores and relays (policy, ADR 0010 A4 addendum).
/// Understood versions keep their own validity caps (16 KiB for v1 and v2).
pub const OPAQUE_RELAY_MAX_BYTES: usize = 32 * 1024;

/// Share of `storage.max` that opaque events may take (B4).
pub const OPAQUE_SHARE_PERCENT: u64 = 5;
/// Extra proof of work required from events of a version this node does not
/// understand (B4): carrying them costs every node, understanding nothing.
pub const OPAQUE_EXTRA_POW_BITS: u32 = 2;

/// Extra proof of work required from a v2 `submit` (ADR 0015 §5), on top of
/// the public network's schedule and size bits. A submission pays no
/// new-author bits: its author is a one-off key by design.
pub const SUBMIT_EXTRA_POW_BITS: u32 = 2;
/// Default retention of v2 `submit` events (`submit.retention`): 30 days.
pub const DEFAULT_SUBMIT_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);
/// How often the store is tidied: expired submissions and drop records.
const SUBMIT_TIDY_SECS: u64 = 3600;

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
/// A fetch on read tries its providers this many times, the next after
/// this pause, then twice it.
const READ_FETCH_TRIES: u32 = 3;
const READ_FETCH_PAUSE: Duration = Duration::from_millis(500);
/// A blob pushed to this node (`replicate`) is fetched from its pusher up
/// to this many times, with the pauses of a read, all tries within
/// [`PUSH_FETCH_BUDGET`] (the 60 s one try may take) and within what is
/// left of the request's [`REPLICATE_BUDGET`].
const PUSH_FETCH_TRIES: u32 = 3;
const PUSH_FETCH_BUDGET: Duration = Duration::from_secs(60);
/// The most time the blobs of one `replicate` request take here, fetch
/// slot waits included, all its blobs together: [`PUSH_TIMEOUT`] less
/// 30 s for accepting the event and answering, so the answer still comes
/// before the pusher gives up on it. What is not fetched by then is not
/// taken (`durability.replicate_budget`; tests shorten it).
const REPLICATE_BUDGET: Duration = Duration::from_secs(120);
/// A provider whose fetch failed is skipped for this long, doubling with
/// each further failure up to [`PROVIDER_BACKOFF_MAX`] (B7).
const PROVIDER_BACKOFF: Duration = Duration::from_secs(30);
const PROVIDER_BACKOFF_MAX: Duration = Duration::from_secs(3600);

/// Replication requests a publisher has running at once to one peer: the
/// default `limits.blob_fetches_per_peer` of receivers, 0.4.0 and 0.4.1
/// included, which skip the blobs past it. A receiver fetches the blobs of
/// one request one after the other.
const PUSHES_PER_PEER: usize = 4;
/// Pushes to one peer waiting for one of its [`PUSHES_PER_PEER`] slots;
/// past these, or after [`PUSH_WAIT`], a push is skipped (logged).
const PUSH_QUEUE: usize = 64;
const PUSH_WAIT: Duration = Duration::from_secs(600);
/// How long a publisher waits for the answer to one replication request.
const PUSH_TIMEOUT: Duration = Duration::from_secs(150);

/// How often the blob store collects the blobs nothing keeps
/// (`storage.gc`).
pub const BLOB_GC_INTERVAL: Duration = Duration::from_secs(3600);

/// How often blobs found unreadable are fetched again from peers.
const BLOB_REPAIR_SECS: u64 = 300;
/// The longest a blob repair waits to connect to one provider.
const REPAIR_CONNECT_LIMIT: Duration = Duration::from_secs(5);

/// What the blob collection keeps besides the blob store's own tags and
/// temporary tags: the blobs a kept event lists, and those being brought
/// in ([`BlobHold`]).
///
/// The collection of iroh-blobs runs in three steps, each reading what to
/// keep at its own time: our callback ([`protect_blobs`]), then it clears
/// its set of blobs written since the last run and lists the tags and the
/// temporary tags (the mark), then it deletes what is in none of them and
/// was not written meanwhile (the sweep). It does not keep a download in
/// progress by itself: a partial blob is written to disk without telling
/// its database, so one that started before the run is deleted under the
/// download. [`BlobHold`] keeps it.
#[derive(Debug, Default)]
struct BlobKeep {
    /// Hashes being fetched, with how many hold each.
    held: std::sync::Mutex<HashMap<Hash, usize>>,
    /// The blob store, for the callback (set once it is open).
    blobs: std::sync::OnceLock<iroh_blobs::api::Store>,
    /// Collections begun (callbacks run), for tests.
    runs: std::sync::atomic::AtomicU64,
    /// When a collection last read what to keep (Unix milliseconds, 0:
    /// none yet): a run our callback aborts does not count.
    last_kept: std::sync::atomic::AtomicU64,
    /// Blobs recorded complete whose content cannot be read (the file is
    /// gone), to fetch again ([`Node::repair_blob`]).
    unreadable: std::sync::Mutex<HashSet<Hash>>,
}

impl BlobKeep {
    /// Note `hash` as unreadable; true the first time (until repaired),
    /// for one WARN per blob.
    fn note_unreadable(&self, hash: Hash) -> bool {
        self.unreadable.lock().is_ok_and(|mut u| u.insert(hash))
    }
}

/// Keeps a blob from the collection while it is brought in (a fetch for an
/// event, a replica), until it is pinned: in [`BlobKeep::held`], which the
/// callback of every run reads, and under a temporary tag, which the mark
/// of a run already past its callback reads.
struct BlobHold {
    keep: Arc<BlobKeep>,
    hash: Hash,
    _tag: Option<iroh_blobs::api::TempTag>,
}

impl Drop for BlobHold {
    fn drop(&mut self) {
        if let Ok(mut held) = self.keep.held.lock()
            && let std::collections::hash_map::Entry::Occupied(mut entry) = held.entry(self.hash)
        {
            *entry.get_mut() = entry.get().saturating_sub(1);
            if *entry.get() == 0 {
                entry.remove();
            }
        }
    }
}

/// The blobs one collection must keep, beyond the tags and the temporary
/// tags its own mark reads later: the temporary tags, the held blobs, and
/// the blobs every kept full event lists (tombstones and opaque events
/// list none), so a fetched blob between its event and its tag too. Read
/// in the order the hand-offs happen (an import's temporary tag before its
/// tag, a hold before its tag), so a blob handed from one to the next
/// meanwhile is seen in one of them. Logs, at INFO, what the run deletes.
async fn blobs_to_keep(keep: &BlobKeep, store: &Store) -> Result<HashSet<Hash>> {
    let blobs = keep.blobs.get().context("blob store not open yet")?;
    let mut live = HashSet::new();
    let mut temp = blobs.tags().list_temp_tags().await?;
    while let Some(tag) = temp.next().await {
        live.insert(tag.hash);
    }
    match keep.held.lock() {
        Ok(held) => live.extend(held.keys().copied()),
        Err(_) => bail!("held blobs unreadable"),
    }
    live.extend(
        store
            .referenced_blobs()?
            .iter()
            .filter_map(|h| h.parse::<Hash>().ok()),
    );
    // What the sweep deletes, for the log: every blob in no tag and not
    // kept above (at most: a blob written during the run is spared).
    let mut tagged = HashSet::new();
    let mut tags = blobs.tags().list().await?;
    while let Some(tag) = tags.next().await {
        tagged.insert(tag?.hash);
    }
    let mut doomed = Vec::new();
    let mut all = blobs.blobs().list().stream().await?;
    while let Some(hash) = all.next().await {
        let hash = hash?;
        if !live.contains(&hash) && !tagged.contains(&hash) {
            doomed.push(hash);
        }
    }
    if !doomed.is_empty() {
        let mut bytes: u64 = 0;
        for hash in &doomed {
            bytes = bytes.saturating_add(match blobs.blobs().status(*hash).await? {
                BlobStatus::Complete { size } => size,
                BlobStatus::Partial { size } => size.unwrap_or(0),
                BlobStatus::NotFound => 0,
            });
        }
        info!(
            blobs = doomed.len(),
            bytes,
            "blob collection: deleting {} nothing keeps",
            format_bytes(bytes)
        );
    }
    Ok(live)
}

/// The protect callback of the blob collection: adds [`blobs_to_keep`] to
/// `live`. Any error aborts this run (logged at WARN), so that nothing a
/// failed read would have kept is deleted; the next run tries again.
fn protect_blobs<'a>(
    keep: &Arc<BlobKeep>,
    store: &Arc<Store>,
    live: &'a mut HashSet<Hash>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = iroh_blobs::store::ProtectOutcome> + Send + Sync + 'a>,
> {
    use iroh_blobs::store::ProtectOutcome;
    keep.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (keep, store) = (Arc::clone(keep), Arc::clone(store));
    // On its own task: the callback's future must be Sync, a join handle is.
    let task = tokio::spawn(async move {
        let kept = blobs_to_keep(&keep, &store).await;
        if kept.is_ok() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            keep.last_kept
                .store(now, std::sync::atomic::Ordering::Relaxed);
        }
        kept
    });
    Box::pin(async move {
        match task.await {
            Ok(Ok(kept)) => {
                live.extend(kept);
                ProtectOutcome::Continue
            }
            Ok(Err(e)) => {
                warn!("blob collection skipped: {e:#}");
                ProtectOutcome::Abort
            }
            Err(e) => {
                warn!("blob collection skipped: {e}");
                ProtectOutcome::Abort
            }
        }
    })
}

/// Constrain a closure to the signature of a collection callback.
fn gc_callback<F>(f: F) -> iroh_blobs::store::ProtectCb
where
    F: for<'a> Fn(
            &'a mut HashSet<Hash>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = iroh_blobs::store::ProtectOutcome>
                    + Send
                    + Sync
                    + 'a,
            >,
        > + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

/// How long iroh-blobs may take to open a blob store whose directories and
/// database [`check_blob_store`] could open.
const BLOB_STORE_OPEN_LIMIT: Duration = Duration::from_secs(30);

/// What to check when the blob store in `dir` cannot be opened.
fn blob_store_error(dir: &Path) -> String {
    format!(
        "cannot open the blob store in {}: check that this user can read and write it, \
         that the disk is not full or failing, and that blobs.db is not damaged",
        dir.display()
    )
}

/// Check that the blob store in `dir` can be opened: its directories, and
/// its database read and written. iroh-blobs never returns when it cannot
/// open a store (its runtime, dropped on one of its own threads, waits for
/// itself), so this comes first, and says why.
async fn check_blob_store(dir: &Path) -> Result<()> {
    let root = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for sub in ["data", "temp"] {
            let path = root.join(sub);
            std::fs::create_dir_all(&path).with_context(|| format!("{}", path.display()))?;
        }
        let path = root.join("blobs.db");
        let db = redb::Database::create(&path).with_context(|| format!("{}", path.display()))?;
        db.begin_write()?.commit()?;
        Ok(())
    })
    .await?
    .with_context(|| blob_store_error(dir))
}

/// Open the blob store in `dir` with `options`, or fail with an error
/// that names it: checked first ([`check_blob_store`]), then given
/// [`BLOB_STORE_OPEN_LIMIT`] for a failure the check missed. A store that
/// never opens leaves one thread stuck, of the store's own runtime: the
/// caller's runtime does not wait for it, the node does not start, and the
/// process exits with the error.
async fn open_blob_store(
    dir: &Path,
    options: iroh_blobs::store::fs::options::Options,
) -> Result<FsStore> {
    let open = async { Ok(FsStore::load_with_opts(dir.join("blobs.db"), options).await?) };
    open_blob_store_within(dir, BLOB_STORE_OPEN_LIMIT, open).await
}

/// [`open_blob_store`], with `open` as the opening and `limit` its limit.
async fn open_blob_store_within(
    dir: &Path,
    limit: Duration,
    open: impl Future<Output = Result<FsStore>>,
) -> Result<FsStore> {
    check_blob_store(dir).await?;
    match tokio::time::timeout(limit, open).await {
        Ok(Ok(blobs)) => Ok(blobs),
        Ok(Err(e)) => Err(e).with_context(|| blob_store_error(dir)),
        Err(_) => Err(anyhow::anyhow!(
            "{}: it did not open within {limit:?}",
            blob_store_error(dir)
        )),
    }
}

/// Open the blob store of `dir`, collecting what nothing keeps every
/// `interval` unless `gc` is off.
async fn load_blobs(
    dir: &Path,
    gc: bool,
    interval: Duration,
    keep: &Arc<BlobKeep>,
    store: &Arc<Store>,
) -> Result<FsStore> {
    {
        // Before anything writes, and on a store no collection runs on yet:
        // a collection's pending deletion already hides a blob from
        // `status`, its file must stay until the deletion commits.
        let options = iroh_blobs::store::fs::options::Options::new(dir);
        let blobs = open_blob_store(dir, options).await?;
        match check_files(dir, &blobs, gc).await {
            Ok(check) => {
                if check.removed > 0 {
                    info!(
                        files = check.removed,
                        "blob collection: removed the files a crash left behind"
                    );
                }
                for hash in check.unreadable {
                    if keep.note_unreadable(hash) {
                        warn!(%hash, "blob recorded but its file is gone: it will be fetched again from peers");
                    }
                }
            }
            Err(e) => warn!("blob files not checked: {e:#}"),
        }
        blobs.shutdown().await?;
    }
    let mut options = iroh_blobs::store::fs::options::Options::new(dir);
    if gc {
        let (keep, store) = (Arc::clone(keep), Arc::clone(store));
        options.gc = Some(iroh_blobs::store::GcConfig {
            interval,
            add_protected: Some(gc_callback(move |live| protect_blobs(&keep, &store, live))),
        });
    }
    let blobs = open_blob_store(dir, options).await?;
    keep.blobs.set(std::ops::Deref::deref(&blobs).clone()).ok();
    Ok(blobs)
}

/// What [`check_files`] found.
#[derive(Debug, Default)]
struct FileCheck {
    /// Files removed.
    removed: usize,
    /// Blobs recorded complete whose data file is gone.
    unreadable: Vec<Hash>,
}

/// Check the files of the blob store of `dir`, before anything else uses
/// it (nothing writes, nothing collects). With `remove`, remove what a
/// crash left: the files of blobs its database no longer has (a
/// collection commits the deletion, then removes the files: a crash in
/// between leaves them, and no later run sees them), and the temporary
/// files of imports. Also lists the blobs recorded complete whose data
/// file is gone (a race in the blob store, a disk fault).
async fn check_files(dir: &Path, blobs: &FsStore, remove: bool) -> Result<FileCheck> {
    let mut check = FileCheck::default();
    let mut files: HashMap<Hash, Vec<PathBuf>> = HashMap::new();
    let mut has_data: HashSet<Hash> = HashSet::new();
    let mut entries = tokio::fs::read_dir(dir.join("data")).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some((hex, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        if !matches!(ext, "data" | "obao4" | "sizes4" | "bitfield") || hex.len() != 64 {
            continue;
        }
        if let Ok(hash) = hex.parse::<Hash>() {
            if ext == "data" {
                has_data.insert(hash);
            }
            files.entry(hash).or_default().push(entry.path());
        }
    }
    if remove {
        for (hash, paths) in &files {
            if matches!(blobs.blobs().status(*hash).await?, BlobStatus::NotFound) {
                for path in paths {
                    tokio::fs::remove_file(path).await?;
                    check.removed = check.removed.saturating_add(1);
                }
            }
        }
        let mut temp = tokio::fs::read_dir(dir.join("temp")).await?;
        while let Some(entry) = temp.next_entry().await? {
            if entry.file_type().await?.is_file() {
                tokio::fs::remove_file(entry.path()).await?;
                check.removed = check.removed.saturating_add(1);
            }
        }
    }
    // Above this size the store keeps the data in a file, not inline.
    let inlined = iroh_blobs::store::fs::options::InlineOptions::default().max_data_inlined;
    let mut recorded = blobs.blobs().list().stream().await?;
    let mut without_file = Vec::new();
    while let Some(hash) = recorded.next().await {
        let hash = hash?;
        if !has_data.contains(&hash) {
            without_file.push(hash);
        }
    }
    for hash in without_file {
        if let BlobStatus::Complete { size } = blobs.blobs().status(hash).await?
            && size > inlined
        {
            check.unreadable.push(hash);
        }
    }
    Ok(check)
}

/// Delete every tag of blob `hash` (`wats/blob/<hash>`, and the automatic
/// tag of an import by an older release): the next collection deletes
/// it, unless a kept event lists it.
async fn unpin_blob(blobs: &FsStore, hash: Hash) -> Result<()> {
    let mut names = Vec::new();
    let mut tags = blobs.tags().list().await?;
    while let Some(tag) = tags.next().await {
        let tag = tag?;
        if tag.hash == hash {
            names.push(tag.name);
        }
    }
    for name in names {
        blobs.tags().delete(name).await?;
    }
    Ok(())
}

/// Untag the blobs a drop freed (best effort: a failure is logged, the
/// body is gone anyway).
async fn unpin_freed(blobs: &FsStore, outcome: &DropOutcome) {
    for hash in &outcome.freed_blobs {
        let Ok(parsed) = hash.parse::<Hash>() else {
            continue;
        };
        if let Err(e) = unpin_blob(blobs, parsed).await {
            warn!(%hash, "blob of a dropped body not untagged: {e:#}");
        }
    }
}

/// [`Node::drop_body`] with no node running on `data_dir` (`wats drop`):
/// holds the data directory lock meanwhile. The blobs it frees are deleted
/// by the next collection of a running node.
pub async fn drop_body_offline(
    data_dir: &Path,
    id: &str,
    reason: DropReason,
    detail: Option<&str>,
) -> Result<DropOutcome> {
    let path = data_dir.join("events.sqlite");
    if !path.exists() {
        bail!("no event store in {}", data_dir.display());
    }
    let lock = lock_data_dir(data_dir, &Holder::role("cli"))?;
    let store = Store::open(&path)?;
    let outcome = store.drop_body(id, reason, None, detail, now_secs())?;
    if !outcome.freed_blobs.is_empty() {
        let dir = data_dir.join("blobs");
        let options = iroh_blobs::store::fs::options::Options::new(&dir);
        let blobs = open_blob_store(&dir, options).await?;
        unpin_freed(&blobs, &outcome).await;
        blobs.shutdown().await.ok();
    }
    drop(lock);
    Ok(outcome)
}

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
/// From the first neighbor to the `swarm: …` line of the log (ADR 0026).
const SWARM_LINE_SETTLE: Duration = Duration::from_secs(10);
/// How often the reconnect loop looks at the peers while online.
const RECONNECT_WATCH_EVERY: Duration = Duration::from_secs(1);
/// At most one catch-up with every neighbor per this long, on network
/// changes while online.
const CATCH_UP_EVERY: Duration = Duration::from_secs(30);
/// How long a reconnection attempt waits for its connection to a peer
/// ([`Node::redial`]). A handshake, even through a relay, takes well
/// under a second; this leaves room for the QUIC retransmits (1, 3 and
/// 7 s after the first packet) on a bad link. The next attempt dials
/// afresh.
const REDIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve bootstrap entries concurrently; an entry that does not resolve
/// is logged and skipped, so one dead seed never blocks the others.
async fn resolve_bootstrap(
    entries: &[BootstrapEntry],
    me: EndpointId,
    resolver: &crate::bootstrap::Resolver,
) -> Vec<EndpointAddr> {
    let mut tasks = tokio::task::JoinSet::new();
    for entry in entries.iter().cloned() {
        let resolver = resolver.clone();
        tasks.spawn(async move {
            let resolved =
                tokio::time::timeout(Duration::from_secs(10), resolver.resolve(&entry)).await;
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

/// The reconnect loop ([`crate::reconnect`]): while the node has no
/// gossip neighbor, dial everything it knows again, with backoff; never
/// give up. Holds the node only while it works, so a stopped node ends it.
async fn reconnect_loop(weak: std::sync::Weak<Node>) {
    use crate::reconnect::jitter;
    use tokio::time::Instant;
    let Some(node) = weak.upgrade() else { return };
    let backoff = node.config.reconnect;
    drop(node);
    // The dials of `start` get `start_grace` to answer before the first
    // attempt; the connectivity stays `connecting` meanwhile.
    let mut starting = true;
    let mut attempt: u32 = 1;
    let mut next: Option<Instant> = Instant::now().checked_add(backoff.start_grace);
    // The attempt whose wait was last reported.
    let mut reported: u32 = 0;
    let mut last_catch_up: Option<Instant> = None;
    let mut last_redial: Option<Instant> = None;
    loop {
        let Some(node) = weak.upgrade() else { return };
        if node.endpoint().is_closed() {
            return;
        }
        let changed = node
            .network_changed
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        let peers = node.neighbors().len();
        let now = Instant::now();
        if peers > 0 {
            let transition = node
                .connectivity
                .lock()
                .ok()
                .and_then(|mut c| c.online(peers, now_secs()));
            if let Some(transition) = transition {
                transition.log();
            }
            (starting, attempt, next, reported) = (false, 1, None, 0);
            if changed && last_catch_up.is_none_or(|t| t.elapsed() >= CATCH_UP_EVERY) {
                last_catch_up = Some(now);
                node.catch_up_all();
            }
            let kick = Arc::clone(&node.reconnect_kick);
            drop(node);
            let _ = tokio::time::timeout(RECONNECT_WATCH_EVERY, kick.notified()).await;
            continue;
        }
        if changed {
            // A new network: dial at once (but not twice within `first`
            // when the network flaps), the backoff starts over.
            let soonest = last_redial
                .and_then(|t| t.checked_add(backoff.first))
                .map_or(now, |t| t.max(now));
            (attempt, next, reported) = (1, Some(soonest), 0);
        }
        // Just went offline: the first, short wait.
        let at = *next.get_or_insert_with(|| {
            now.checked_add(backoff.delay(attempt, jitter()))
                .unwrap_or(now)
        });
        if !starting && reported != attempt {
            reported = attempt;
            let dialable = node.has_dial_targets();
            let wait = at.saturating_duration_since(now);
            let transition = node
                .connectivity
                .lock()
                .ok()
                .and_then(|mut c| c.offline(attempt, wait, dialable, now_secs()));
            if let Some(transition) = transition {
                transition.log();
            }
        }
        if now >= at {
            last_redial = Some(now);
            node.redial().await;
            starting = false;
            attempt = attempt.saturating_add(1);
            next = Instant::now().checked_add(backoff.delay(attempt, jitter()));
            continue;
        }
        let kick = Arc::clone(&node.reconnect_kick);
        drop(node);
        let _ = tokio::time::timeout(at.saturating_duration_since(now), kick.notified()).await;
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The public ids of a node's agents, in its data directory, for `who` on
/// the other nodes of the machine (ADR 0044).
pub const AGENTS_FILE: &str = "agents.json";

/// The agents of this machine's nodes: each data directory's
/// [`AGENTS_FILE`], with the node's name. Unreadable files are skipped.
pub fn machine_agents(
    machine: &crate::naming::Machine,
) -> Vec<(String, PathBuf, serde_json::Value)> {
    let mut out = Vec::new();
    for dir in machine.dirs() {
        let Ok(bytes) = std::fs::read(dir.join(AGENTS_FILE)) else {
            continue;
        };
        let Ok(serde_json::Value::Object(file)) =
            serde_json::from_slice::<serde_json::Value>(&bytes)
        else {
            continue;
        };
        let name = crate::naming::display(&dir, machine.default_dir.as_deref());
        for agent in file
            .get("agents")
            .and_then(|a| a.as_array())
            .into_iter()
            .flatten()
        {
            out.push((name.clone(), dir.clone(), agent.clone()));
        }
    }
    out
}

/// A crash report's collector gets this long to be reached, and this long
/// to answer: the report stays here otherwise, for the next try.
const REPORT_CONNECT_LIMIT: Duration = Duration::from_secs(20);
const REPORT_CALL_LIMIT: Duration = Duration::from_secs(60);

/// Handling one gossip message longer than this is logged: the receive
/// loop of its network hears nothing else meanwhile.
const SLOW_HANDLING: Duration = Duration::from_secs(30);

/// After a network's gossip receiver ended, the wait before subscribing
/// to its topic again.
const RESUBSCRIBE_AFTER: Duration = Duration::from_secs(5);

pub fn topic_for(net: &str) -> TopicId {
    TopicId::from_bytes(blake3::derive_key(TOPIC_CONTEXT, net.as_bytes()))
}

pub struct Node {
    config: NodeConfig,
    /// When this node started (Unix seconds).
    started_at: u64,
    router: Router,
    gossip: Gossip,
    blobs: FsStore,
    downloader: Downloader,
    store: Arc<Store>,
    root: RootSecret,
    author: SigningKey,
    lookup: MemoryLookup,
    /// One gossip sender per network joined; a network left (the public
    /// one on a halt, ADR 0016 §4) is removed.
    senders: std::sync::Mutex<HashMap<String, GossipSender>>,
    /// The gossip receive loop of each network joined, to leave it.
    receive_loops: std::sync::Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// The halt state of the public network as last evaluated.
    halt_view: tokio::sync::watch::Sender<crate::halt::HaltView>,
    /// What this node did about it (see [`Node::enforce_halts`]).
    halt_outcome: tokio::sync::watch::Sender<HaltOutcome>,
    /// `halt.serve` with a halt in effect: only halt events in and out.
    halt_serving: std::sync::atomic::AtomicBool,
    /// Peers that confirmed this node's push of the halt.
    halt_pushed: std::sync::atomic::AtomicUsize,
    /// Networks left on a halt.
    left_nets: std::sync::Mutex<Vec<String>>,
    neighbors: std::sync::Mutex<HashMap<String, HashSet<EndpointId>>>,
    fetched: tokio::sync::broadcast::Sender<BlobRef>,
    /// Peers seen recently, with the time we last heard from them.
    peers: std::sync::Mutex<HashMap<EndpointId, u64>>,
    /// The nodes heard from per network in the last 24 h, kept in
    /// `swarm.json` (ADR 0026).
    swarm: std::sync::Mutex<crate::swarm::Sightings>,
    /// Blob hash -> nodes that announced they hold it (hints, unverified).
    hints: std::sync::Mutex<HashMap<String, Vec<EndpointId>>>,
    /// Providers whose last blob fetches failed: (failures, retry after).
    provider_backoff: std::sync::Mutex<HashMap<EndpointId, (u32, std::time::Instant)>>,
    /// What peers said in `hello`.
    peer_info: std::sync::Mutex<HashMap<EndpointId, NodeInfo>>,
    /// Cached blob store size: (when measured, bytes).
    usage: std::sync::Mutex<Option<(std::time::Instant, u64)>>,
    /// What the reconnect loop sees ([`crate::reconnect`]).
    connectivity: std::sync::Mutex<crate::reconnect::Connectivity>,
    /// Wakes the reconnect loop: a neighbor up, a network change.
    reconnect_kick: Arc<tokio::sync::Notify>,
    /// A network change since the reconnect loop last looked.
    network_changed: std::sync::atomic::AtomicBool,
    /// Peers the reconnect loop is dialing ([`Node::redial`]): one dial
    /// per peer at a time.
    redialing: std::sync::Mutex<HashSet<EndpointId>>,
    /// Size of the whole data directory, measured every 30 s.
    storage_bytes: std::sync::atomic::AtomicU64,
    /// When we last logged that storage is full (rate limit).
    storage_logged: std::sync::Mutex<Option<std::time::Instant>>,
    /// Asked to stop (`start_daemon` hands over to a daemon).
    stop: tokio::sync::Notify,
    /// Per-peer request limits.
    limiter: crate::limits::RateLimiter,
    /// Replication requests this node has running, per peer it pushes to.
    pushes: crate::limits::PeerSlots,
    /// Load challenges (ADR 0014 addendum).
    challenger: crate::challenge::Challenger,
    /// `/wats` requests being served.
    in_flight: std::sync::atomic::AtomicUsize,
    /// What the blob collection keeps besides tags (see [`BlobKeep`]).
    blob_keep: Arc<BlobKeep>,
    /// Tokens of the agent runs in progress: an MCP session presenting one
    /// is a runner session.
    runner_tokens: std::sync::Mutex<HashMap<String, RunnerRun>>,
    /// Author ids of the profiles used on this node, with their profile
    /// names (see `author_key`).
    local_authors: std::sync::Mutex<HashMap<String, String>>,
    /// Reputation tables per network (ADR 0019 §2).
    reputation: crate::reputation::Cache,
    /// Exclusive lock on the data directory, released by `shutdown`.
    lock: std::sync::Mutex<Option<std::fs::File>>,
    /// This node's view of the infrastructure (see [`crate::health`]).
    health: std::sync::Mutex<crate::health::Health>,
    /// Bootstrap nodes and peers configured, for the bootstrap check.
    seed_ids: Vec<EndpointId>,
    /// Gossip links: what each neighbor last delivered, resets ([`links`]).
    links: links::Links,
    /// The history tasks (ADR 0025).
    history: history::History,
    /// Rotation at the cap (ADR 0025 §7).
    rotation: rotation::Rotation,
    /// Replica repair and last-copy protection (ADR 0029).
    durability: durability::State,
    /// Possession proofs: the blob requests refused meanwhile (ADR 0029).
    proof_guard: Arc<proof::Guard>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.endpoint_id())
            .finish_non_exhaustive()
    }
}

/// Counts a `/wats` request in flight while it lives.
struct InFlight<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> InFlight<'a> {
    fn enter(count: &'a std::sync::atomic::AtomicUsize) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        InFlight(count)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How long a daemon waits for a background node to hand the data
/// directory over (ADR 0021 §4).
const TAKEOVER_WAIT: Duration = Duration::from_secs(30);

/// Lock the data directory, waiting up to `wait` for another node to let
/// go of it. `role` is recorded for the next process that finds it held.
fn lock_data_dir_waiting(dir: &Path, wait: Duration, who: &Holder) -> Result<std::fs::File> {
    // No deadline when `wait` does not fit in an `Instant`: wait forever.
    let deadline = std::time::Instant::now().checked_add(wait);
    loop {
        match lock_data_dir(dir, who) {
            Ok(file) => return Ok(file),
            Err(e) if deadline.is_some_and(|d| std::time::Instant::now() >= d) => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Whether a node holds the lock of `dir` (`wats upgrade` says to restart
/// it). Creates nothing.
pub fn node_running(dir: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("lock"))
    else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// The role recorded by the process holding the lock of `dir`
/// (`daemon`, `background`, `in-process`, `cli`), if it wrote one.
fn lock_holder_role(dir: &Path) -> Option<String> {
    lock_holder(dir).map(|h| h.role).filter(|r| !r.is_empty())
}

/// What the lock file of `dir` records about its holder, or about the
/// last one: only [`node_running`] says whether it still holds it.
pub fn lock_holder(dir: &Path) -> Option<LockHolder> {
    let text = std::fs::read_to_string(dir.join("lock")).ok()?;
    serde_json::from_str::<LockHolder>(&text).ok()
}

/// How the process taking a data directory lock describes itself.
#[derive(Debug, Default)]
struct Holder {
    role: String,
    profile: Option<String>,
    log: Option<PathBuf>,
}

impl Holder {
    fn role(role: &str) -> Self {
        Holder {
            role: role.to_string(),
            ..Holder::default()
        }
    }
}

fn lock_data_dir(dir: &Path, who: &Holder) -> Result<std::fs::File> {
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
        Ok(()) => {
            write_lock_holder(&file, who);
            Ok(file)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let holder = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<LockHolder>(&text).ok());
            match holder {
                Some(h) => bail!(
                    "another wats node is using {}: pid {}, wats {} ({}), started {}. \
                     Stop it (`kill {}`), or choose another data dir (--dir / WATS_DIR)",
                    dir.display(),
                    h.pid,
                    h.version,
                    h.exe,
                    h.started,
                    h.pid
                ),
                // A node older than this message writes no holder.
                None => bail!(
                    "another wats node is using {}; stop it or choose another data dir (--dir / WATS_DIR)",
                    dir.display()
                ),
            }
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Who holds the data directory lock, written into the lock file by the
/// holder so that a second `wats` can say which process to stop, and
/// `wats ps` what runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockHolder {
    pub pid: u32,
    pub version: String,
    pub exe: String,
    pub started: String,
    /// `daemon`, `background` (started by `wats mcp`), `in-process` or
    /// `cli`; empty when written by an older release.
    #[serde(default)]
    pub role: String,
    /// When it took the lock (Unix seconds); 0 from an older release.
    #[serde(default)]
    pub started_secs: u64,
    /// The node's profile (ADR 0023).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Its log file, when it runs detached (ADR 0023).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
    /// The node is up: [`Node::start`] returned (`wats --detach` waits
    /// for it).
    #[serde(default)]
    pub up: bool,
}

/// Record this process in the lock file it holds (best effort).
fn write_lock_holder(file: &std::fs::File, who: &Holder) {
    let now = now_secs();
    let holder = LockHolder {
        role: who.role.clone(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        exe: std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown path".into()),
        started: crate::runs::format_time(now),
        started_secs: now,
        profile: who.profile.clone(),
        log: who.log.clone(),
        up: false,
    };
    rewrite_lock_holder(file, &holder);
}

/// Replace the holder record in the lock file this process holds.
fn rewrite_lock_holder(mut file: &std::fs::File, holder: &LockHolder) {
    use std::io::{Seek, Write};
    if let Ok(json) = serde_json::to_vec(holder) {
        let _ = file
            .set_len(0)
            .and_then(|()| file.seek(std::io::SeekFrom::Start(0)))
            .and_then(|_| file.write_all(&json))
            .and_then(|()| file.flush());
    }
}

impl Node {
    pub async fn start(config: NodeConfig) -> Result<Arc<Self>> {
        let lock = {
            let (dir, wait) = (config.data_dir.clone(), config.lock_wait);
            let who = Arc::new(Holder {
                role: config.role.to_string(),
                profile: Some(config.profile.clone()),
                log: config.log_file.clone(),
            });
            let first = {
                let (dir, who) = (dir.clone(), Arc::clone(&who));
                tokio::task::spawn_blocking(move || lock_data_dir_waiting(&dir, wait, &who)).await?
            };
            match first {
                Ok(lock) => lock,
                // A daemon takes over from a background node started by
                // `wats mcp` (ADR 0021 §4): it asks it to hand over, then
                // waits for the lock. Its sessions reconnect by themselves.
                Err(e)
                    if config.role == NodeRole::Daemon
                        && lock_holder_role(&dir).as_deref() == Some("background") =>
                {
                    info!("a background node holds the data directory: asking it to hand over");
                    if let Err(asked) =
                        crate::attach::control(&dir, crate::attach::Control::HandOver).await
                    {
                        warn!("the background node did not take the hand-over request: {asked:#}");
                        return Err(e);
                    }
                    tokio::task::spawn_blocking(move || {
                        lock_data_dir_waiting(&dir, TAKEOVER_WAIT, &who)
                    })
                    .await??
                }
                Err(e) => return Err(e),
            }
        };
        // A node runs again: the stop of the last one, by `wats stop` or a
        // hand-over, is over. MCP sessions that said so reconnect.
        crate::background::clear_stop_record(&config.data_dir);
        // For `wats ps` (ADR 0023): where to look for this node.
        if let Some(registry) = &config.registry {
            if let Err(e) = crate::background::register(registry, &config.data_dir) {
                debug!("could not record this node in {}: {e}", registry.display());
            }
            crate::background::prune_known(registry, crate::background::KNOWN_KEEP);
        }
        // Its name on this machine, drawn at its first start (ADR 0023
        // §8): local only, in nothing it sends.
        let machine = crate::naming::Machine::of_user(config.registry.clone());
        if let Err(e) = crate::naming::ensure(&config.data_dir, &machine) {
            debug!("could not name this node: {e}");
        }
        let root = match &config.secret_file {
            Some(path) => RootSecret::load(path)?,
            None => RootSecret::load_or_create(&config.data_dir.join("secret.key"))?,
        };
        let author = root.profile_key(&config.profile);
        let secret = SecretKey::from_bytes(&root.transport_key_bytes());
        // The agents' public ids, for the `who` of other nodes on this
        // machine: two data directories derive the same profile names.
        let agents: Vec<serde_json::Value> = config
            .agents
            .iter()
            .filter(|a| is_valid_profile(&a.profile))
            .map(|a| {
                serde_json::json!({
                    "name": a.name,
                    "profile": a.profile,
                    "author": author_id(&root.profile_key(&a.profile)),
                    "runtime": a.runtime,
                })
            })
            .collect();
        if let Err(e) = crate::background::write_file(
            &config.data_dir.join(AGENTS_FILE),
            &serde_json::json!({ "agents": agents }).to_string(),
        ) {
            debug!("could not write {AGENTS_FILE}: {e}");
        }

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
            NetworkMode::LocalRelay { port: relay_port } => {
                let url: iroh::RelayUrl = format!("http://127.0.0.1:{relay_port}").parse()?;
                Endpoint::builder(presets::Minimal)
                    .clear_ip_transports()
                    .relay_mode(iroh::RelayMode::Custom(url.into()))
            }
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
        if config.mode.is_local() {
            // Hermetic: never dial what is not on this machine.
            cached = crate::peer_cache::loopback_only(cached);
        }
        // Peers met before have a reputation: no load challenge for them.
        let reputable: HashSet<EndpointId> =
            cached.iter().filter_map(|p| p.id.parse().ok()).collect();
        let mut peers = config.peers.clone();
        peers.extend(
            cached
                .iter()
                .filter_map(crate::peer_cache::CachedPeer::to_addr),
        );
        if !cached.is_empty() {
            info!(peers = cached.len(), "dialing peers met before");
        }
        let seed_ids: Vec<EndpointId> = config
            .peers
            .iter()
            .map(|p| p.id)
            .chain(config.bootstrap.iter().map(|b| b.id))
            .chain(config.default_bootstrap.iter().map(|b| b.id))
            .filter(|id| *id != endpoint.id())
            .collect();
        let mut plan = crate::health::Plan::new(
            config.mode.is_local(),
            &config.infra,
            seed_ids.len(),
            cached.len(),
            config.no_peers_after,
        );
        plan.storage = config.blob_gc;
        let health = crate::health::Health::new(plan);
        let mut seeds = config.bootstrap.clone();
        if cached.is_empty() {
            seeds.extend(config.default_bootstrap.iter().cloned());
        }
        peers.extend(resolve_bootstrap(&seeds, endpoint.id(), &config.resolver).await);
        for peer in &peers {
            lookup.add_endpoint_info(peer.clone());
        }

        let store = Arc::new(Store::open(&config.data_dir.join("events.sqlite"))?);
        revalidate_opaque(&store)?;
        let blob_keep = Arc::new(BlobKeep::default());
        let blobs = load_blobs(
            &config.data_dir.join("blobs"),
            config.blob_gc,
            config.blob_gc_interval,
            &blob_keep,
            &store,
        )
        .await?;
        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_BYTES)
            .membership_config(iroh_gossip::proto::HyparviewConfig {
                active_view_capacity: config.gossip_active_view.max(1),
                ..Default::default()
            })
            .spawn(endpoint.clone());
        let downloader = blobs.downloader(&endpoint);
        let proof_guard = Arc::new(proof::Guard::default());
        let protocol = WatsProtocol::default();
        let endpoint_id = endpoint.id();
        let mut router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(
                iroh_blobs::ALPN,
                BlobsProtocol::new(&blobs, Some(proof_guard.event_sender())),
            );
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
        let swarm = crate::swarm::Sightings::load(
            &config.data_dir,
            &config.nets,
            Some(endpoint_id),
            now_secs(),
        );
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
            senders: std::sync::Mutex::new(senders),
            receive_loops: Default::default(),
            halt_view: tokio::sync::watch::channel(Default::default()).0,
            halt_outcome: tokio::sync::watch::channel(HaltOutcome::Running).0,
            halt_serving: Default::default(),
            halt_pushed: Default::default(),
            left_nets: Default::default(),
            neighbors: Default::default(),
            fetched: tokio::sync::broadcast::channel(256).0,
            peers: std::sync::Mutex::new(bootstrap.iter().map(|id| (*id, now_secs())).collect()),
            swarm: std::sync::Mutex::new(swarm),
            hints: Default::default(),
            provider_backoff: Default::default(),
            peer_info: Default::default(),
            usage: Default::default(),
            storage_bytes: Default::default(),
            storage_logged: Default::default(),
            stop: Default::default(),
            limiter: crate::limits::RateLimiter::new(limits),
            pushes: crate::limits::PeerSlots::new(PUSHES_PER_PEER, PUSH_QUEUE),
            challenger: crate::challenge::Challenger::new(
                limits.challenge_base_bits,
                limits.challenge_max_bits,
                reputable,
            ),
            in_flight: Default::default(),
            blob_keep,
            runner_tokens: Default::default(),
            local_authors: Default::default(),
            reputation: Default::default(),
            lock: std::sync::Mutex::new(Some(lock)),
            started_at: now_secs(),
            health: std::sync::Mutex::new(health),
            seed_ids,
            links: Default::default(),
            connectivity: std::sync::Mutex::new(crate::reconnect::Connectivity::new(now_secs())),
            reconnect_kick: Default::default(),
            network_changed: Default::default(),
            redialing: Default::default(),
            history: Default::default(),
            rotation: Default::default(),
            durability: Default::default(),
            proof_guard,
        });
        node.measure_storage().await;
        // Brief lines for `wats attach` (ADR 0051).
        tokio::spawn(crate::brief::watch(Arc::downgrade(&node)));
        if node.events_share().is_some() {
            // Rotation at the cap (ADR 0025 §7): checked at start, then
            // every 30 s, in the background.
            let weak = Arc::downgrade(&node);
            let every = node.config.history.rotation_check;
            tokio::spawn(supervise("rotation", move || {
                let weak = weak.clone();
                async move {
                    loop {
                        let Some(node) = weak.upgrade() else { break };
                        if let Err(e) = node.rotate_if_full().await {
                            warn!("rotation: {e:#}");
                        }
                        drop(node);
                        tokio::time::sleep(every).await;
                    }
                }
            }));
        }
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
            // Blobs found recorded but unreadable (at start, or by a read
            // that could not restore them yet), restored from peers.
            let weak = Arc::downgrade(&node);
            tokio::spawn(supervise("blob repair", move || {
                let weak = weak.clone();
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(BLOB_REPAIR_SECS)).await;
                        let Some(node) = weak.upgrade() else { break };
                        node.repair_unreadable().await;
                    }
                }
            }));
        }
        {
            // Store tidying: expired submissions (ADR 0015 §5) and expired
            // `evicted` drop records (ADR 0020 §2).
            let weak = Arc::downgrade(&node);
            tokio::spawn(supervise("store tidy", move || {
                let weak = weak.clone();
                async move {
                    loop {
                        if let Some(node) = weak.upgrade() {
                            if let Err(e) = node.drop_expired_submits() {
                                warn!("dropping expired submissions: {e:#}");
                            }
                            if let Err(e) = node.expire_drop_records() {
                                warn!("expiring drop records: {e:#}");
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(SUBMIT_TIDY_SECS)).await;
                        if weak.strong_count() == 0 {
                            break;
                        }
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
                        node.save_swarm().await;
                    }
                }
            }));
        }
        if crate::update::may_check(&node.config.update, node.config.mode.is_local()) {
            let (policy, dir) = (node.config.update.clone(), node.config.data_dir.clone());
            let weak = Arc::downgrade(&node);
            tokio::spawn(supervise("update check", move || {
                let (policy, dir, weak) = (policy.clone(), dir.clone(), weak.clone());
                async move {
                    tokio::select! {
                        () = crate::update::run(policy, dir) => {}
                        () = async {
                            loop {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                                if weak.upgrade().is_none() {
                                    break;
                                }
                            }
                        } => {}
                    }
                }
            }));
        }
        node.spawn_reconnect();
        node.spawn_repair();
        node.spawn_swarm_line();
        if !node.config.erasure_labelers.is_empty() {
            node.apply_held_erasures().await;
        }
        node.spawn_asker_notify();
        node.spawn_desktop_notify();
        node.spawn_health_checks();
        if !node.config.halt.accept {
            warn!("this node ignores guardian halts (halt.accept = false)");
        }
        node.evaluate_halt();
        // The replica bytes, before `replicate` is served (N9).
        if let Err(e) = node.count_replica_bytes().await {
            warn!("replica bytes not counted: {e:#}");
        }
        protocol.set_node(&node);
        {
            // This node's profiles, recorded so that rotation keeps their
            // posts across restarts (ADR 0025 §7). Off the start: peers
            // already talk to the node.
            let node = Arc::clone(&node);
            tokio::task::spawn_blocking(move || {
                for author in node.local_authors() {
                    let profile = node.local_profile(&author).unwrap_or_default();
                    if let Err(e) = node.store.note_local_key(&author, &profile, now_secs()) {
                        warn!("profile key not recorded: {e:#}");
                    }
                }
            });
        }
        // History in the background, one task per network (ADR 0025 §4):
        // nothing here waits for it.
        if let Some((max, share)) = node.initial_max_clamped() {
            warn!(
                "sync.initial_max ({}) is larger than the events share of storage.max ({}): \
                 the fill stops at the share",
                format_bytes(max),
                format_bytes(share)
            );
        }
        for net in node.config.nets.clone() {
            node.spawn_history(&net);
            node.spawn_links(&net);
        }
        for (net, receiver) in receivers {
            // The receiver outlives a panic of the loop: the restarted loop
            // picks it up again (a tokio mutex is not poisoned by a panic).
            let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
            let name = format!("gossip receive loop {net}");
            let loop_node = Arc::clone(&node);
            let loop_net = net.clone();
            let handle = tokio::spawn(async move {
                let mut receiver = receiver;
                loop {
                    let ended = supervise(&name, || {
                        Arc::clone(&loop_node).receive_loop(loop_net.clone(), Arc::clone(&receiver))
                    })
                    .await;
                    // Cancelled, or the network was left (a halt).
                    if ended.is_none() || !loop_node.joined(&loop_net) {
                        return;
                    }
                    // The topic's receiver ended (a gossip error): the
                    // node would go on sending and never hear anything on
                    // this network again. Subscribe anew, never polling
                    // the ended receiver again.
                    receiver = loop {
                        tokio::time::sleep(RESUBSCRIBE_AFTER).await;
                        if !loop_node.joined(&loop_net) {
                            return;
                        }
                        if let Some(fresh) = loop_node.resubscribe(&loop_net).await {
                            break Arc::new(tokio::sync::Mutex::new(fresh));
                        }
                    };
                }
            });
            if let Ok(mut loops) = node.receive_loops.lock() {
                loops.insert(net, handle.abort_handle());
            }
        }
        info!(
            id = %node.endpoint_id(),
            author = %node.author_id(),
            infra = %node.config.infra,
            "node started"
        );
        // Up: what `wats --detach` waits for (ADR 0023).
        if let Some(mut holder) = lock_holder(&node.config.data_dir)
            && holder.pid == std::process::id()
            && let Ok(lock) = node.lock.lock()
            && let Some(file) = lock.as_ref()
        {
            holder.up = true;
            rewrite_lock_holder(file, &holder);
        }
        Ok(node)
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    /// The `health` value of `status`: this node's view of the
    /// infrastructure (see [`crate::health`]).
    pub fn health_json(&self) -> serde_json::Value {
        let mut health = self
            .health
            .lock()
            .map(|h| h.json())
            .unwrap_or_else(|_| serde_json::json!({"state": "unknown"}));
        if let Some(map) = health.as_object_mut() {
            map.insert("connectivity".into(), self.connectivity_json());
        }
        health
    }

    /// The `connectivity` value of `health` (see [`crate::reconnect`]).
    pub fn connectivity_json(&self) -> serde_json::Value {
        self.connectivity
            .lock()
            .map(|c| c.json(now_secs()))
            .unwrap_or_else(|_| serde_json::json!({"state": "unknown"}))
    }

    /// Something changed the network (new addresses, the home relay
    /// back): the reconnect loop starts its backoff over.
    pub fn network_changed(&self) {
        self.network_changed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.reconnect_kick.notify_one();
    }

    /// The peer cache, as this node may dial it.
    fn cached_peers(&self) -> Vec<crate::peer_cache::CachedPeer> {
        let cached = crate::peer_cache::load(&self.config.data_dir, self.endpoint_id(), now_secs());
        if self.config.mode.is_local() {
            crate::peer_cache::loopback_only(cached)
        } else {
            cached
        }
    }

    /// Whether the node knows anywhere to dial.
    fn has_dial_targets(&self) -> bool {
        !self.config.peers.is_empty()
            || !self.config.bootstrap.is_empty()
            || !self.config.default_bootstrap.is_empty()
            || !self.cached_peers().is_empty()
    }

    /// Where the node can dial: the configured peers, the peer cache, and
    /// the bootstrap entries, the built-in ones included, resolved now
    /// (every attempt resolves again). Only used with no connected peer,
    /// so the built-in nodes are dialed only when no peer answered (B1).
    async fn dial_targets(&self) -> Vec<EndpointAddr> {
        let me = self.endpoint_id();
        let cached = self.cached_peers();
        let mut seeds = self.config.bootstrap.clone();
        seeds.extend(self.config.default_bootstrap.iter().cloned());
        let mut targets: Vec<EndpointAddr> = self.config.peers.clone();
        targets.extend(
            cached
                .iter()
                .filter_map(crate::peer_cache::CachedPeer::to_addr),
        );
        targets.extend(resolve_bootstrap(&seeds, me, &self.config.resolver).await);
        targets.retain(|t| t.id != me);
        targets
    }

    /// One reconnection attempt: a fresh gossip connection to every
    /// target, handed to gossip; the target is then joined on every
    /// network over it. Returns whether there was anything to dial.
    ///
    /// The node dials by itself, not through `join_peers` alone, because
    /// iroh-gossip (0.101) dials a peer only when nothing is queued for
    /// it. While its dial is pending, `join_peers` only queues a `Join`;
    /// once that dial failed (a QUIC handshake gives up after 30 s), the
    /// queue stays and gossip never dials that peer again: only a
    /// connection clears it. A relay down for more than half a minute
    /// left a node cut off from its peers for good. And a dial still
    /// pending gets through only on its next handshake retransmit, up to
    /// 10 s after the path is back. A fresh connection, handed to gossip
    /// like an incoming one, clears that state and flushes the queue.
    ///
    /// The dials run in the background, so the reconnect loop keeps its
    /// backoff: at most one dial per target per attempt, and none while
    /// the previous one to that target, bounded by
    /// [`REDIAL_CONNECT_TIMEOUT`], is still waiting. A target that does
    /// not answer is not joined (that would only queue one more `Join`,
    /// or start a dial gossip may never retry); the next attempt dials it
    /// again.
    async fn redial(self: &Arc<Self>) -> bool {
        let targets = self.dial_targets().await;
        if targets.is_empty() {
            return false;
        }
        debug!(targets = targets.len(), "dialing peers again");
        for addr in targets {
            let id = addr.id;
            self.lookup.add_endpoint_info(addr.clone());
            let idle = self
                .redialing
                .lock()
                .map(|mut r| r.insert(id))
                .unwrap_or(false);
            if !idle {
                debug!(peer = %id.fmt_short(), "redial: still dialing");
                continue;
            }
            let endpoint = self.endpoint().clone();
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                let dialed = tokio::time::timeout(
                    REDIAL_CONNECT_TIMEOUT,
                    endpoint.connect(addr.clone(), iroh_gossip::ALPN),
                )
                .await;
                let Some(node) = weak.upgrade() else { return };
                if let Ok(mut r) = node.redialing.lock() {
                    r.remove(&id);
                }
                let conn = match dialed {
                    Ok(Ok(conn)) => conn,
                    Ok(Err(e)) => {
                        debug!(peer = %id.fmt_short(), "redial: {e:#}");
                        return;
                    }
                    Err(_) => {
                        debug!(
                            peer = %id.fmt_short(),
                            "redial: no answer within {REDIAL_CONNECT_TIMEOUT:?}"
                        );
                        return;
                    }
                };
                debug!(peer = %id.fmt_short(), "redial: connected");
                if let Err(e) = node.gossip.handle_connection(conn).await {
                    debug!(peer = %id.fmt_short(), "redial: {e:#}");
                    return;
                }
                if let Err(e) = node.add_peer(addr).await {
                    debug!(peer = %id.fmt_short(), "redial: {e:#}");
                }
            });
        }
        true
    }

    /// Catch up with every neighbor (after a network change while online:
    /// gossip may have lost messages without losing its neighbors): a
    /// newest-first catch-up range, then a check of each neighbor (ADR
    /// 0025 §5).
    fn catch_up_all(self: &Arc<Self>) {
        let nets: Vec<(String, Vec<EndpointId>)> = self
            .neighbors
            .lock()
            .map(|n| {
                n.iter()
                    .map(|(net, peers)| (net.clone(), peers.iter().copied().collect()))
                    .collect()
            })
            .unwrap_or_default();
        for (net, peers) in nets {
            if !self.config.history.legacy {
                self.kick_history(&net, &peers, true);
                continue;
            }
            for peer in peers {
                let node = Arc::clone(self);
                let net = net.clone();
                tokio::spawn(async move {
                    match node.sync_from(peer, &net).await {
                        Ok(n) => debug!(%net, peer = %peer.fmt_short(), new = n, "caught up"),
                        Err(e) => debug!(%net, peer = %peer.fmt_short(), "catch-up failed: {e:#}"),
                    }
                });
            }
        }
    }

    /// Push replies to the human's asks to its MCP host (ADR 0018 §7):
    /// for an `answer` or a `clarify` by someone else in the thread of an
    /// ask posted from OpenClaw, wake OpenClaw's main session with one line.
    fn spawn_asker_notify(self: &Arc<Self>) {
        let Some(notify) = self.config.openclaw_notify.clone() else {
            return;
        };
        let mut events = self.store.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            // At most this many wakes per hour, so a busy thread cannot
            // flood the human's chat.
            const PER_HOUR: usize = 20;
            let mut sent: std::collections::VecDeque<std::time::Instant> = Default::default();
            let mut dropped_logged = false;
            loop {
                let stored = match events.recv().await {
                    Ok(stored) => stored,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(node) = weak.upgrade() else { break };
                let reply = stored.event;
                if !matches!(reply.kind, Kind::Answer | Kind::Clarify) {
                    continue;
                }
                let root = reply.thread_root().to_string();
                if root == reply.id
                    || node.store.ask_notify(&root).ok().flatten().as_deref() != Some("openclaw")
                {
                    continue;
                }
                let Some(ask) = node.store.get(&root).ok().flatten() else {
                    continue;
                };
                if ask.event.author == reply.author {
                    continue;
                }
                let title = ask
                    .event
                    .title
                    .clone()
                    .unwrap_or_else(|| crate::event::title_from_text(&ask.event.text));
                let from = format!("agent {}", reply.author.get(..12).unwrap_or(&reply.author));
                let text = crate::openclaw::notify_text(&title, &reply, &from);
                let now = std::time::Instant::now();
                while sent
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(3600))
                {
                    sent.pop_front();
                }
                if sent.len() >= PER_HOUR {
                    if !dropped_logged {
                        warn!("OpenClaw notifications paused: {PER_HOUR} in the last hour");
                        dropped_logged = true;
                    }
                    continue;
                }
                dropped_logged = false;
                sent.push_back(now);
                let notify = notify.clone();
                tokio::spawn(async move {
                    match crate::openclaw::wake(&notify, &text).await {
                        Ok(()) => {
                            info!(ask = %root, reply = %reply.id, "told OpenClaw about a reply to its ask")
                        }
                        Err(e) => {
                            warn!(ask = %root, "could not tell OpenClaw about a reply: {e:#}")
                        }
                    }
                });
            }
        });
    }

    /// A desktop notification for the human (`notify.desktop`): for an
    /// `answer` or a `clarify` by someone else in the thread of an ask of
    /// the human's ([`Node::is_human_ask`]), never an agent's. Asks posted
    /// from OpenClaw are left to its chat when that is set up. At most
    /// [`crate::desktop::PER_HOUR`] per hour.
    fn spawn_desktop_notify(self: &Arc<Self>) {
        let Some(notifier) = self.config.desktop_notify.clone() else {
            return;
        };
        let mut events = self.store.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut limit = crate::desktop::RateLimit::default();
            loop {
                let stored = match events.recv().await {
                    Ok(stored) => stored,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(node) = weak.upgrade() else { break };
                let reply = stored.event;
                if !matches!(reply.kind, Kind::Answer | Kind::Clarify) {
                    continue;
                }
                let root = reply.thread_root().to_string();
                if root == reply.id
                    // An old reply caught up after a restart is not news.
                    || reply.ts.saturating_add(crate::unanswered::WINDOW.as_secs()) < now_secs()
                {
                    continue;
                }
                let Some(ask) = node.store.get(&root).ok().flatten() else {
                    continue;
                };
                if ask.event.author == reply.author || !node.is_human_ask(&ask.event) {
                    continue;
                }
                if node.config.openclaw_notify.is_some()
                    && node.store.ask_notify(&root).ok().flatten().as_deref() == Some("openclaw")
                {
                    continue;
                }
                if !limit.allow(std::time::Instant::now()) {
                    continue;
                }
                let title = ask
                    .event
                    .title
                    .clone()
                    .unwrap_or_else(|| crate::event::title_from_text(&ask.event.text));
                let local = node.local_profile(&reply.author);
                let text = crate::desktop::reply_text(&title, &reply, local.as_deref());
                let notifier = notifier.clone();
                tokio::spawn(async move {
                    match notifier.send(crate::desktop::TITLE, &text).await {
                        Ok(()) => {
                            info!(ask = %root, reply = %reply.id, "desktop notification sent")
                        }
                        Err(e) => debug!(ask = %root, "desktop notification failed: {e:#}"),
                    }
                });
            }
        });
    }

    /// One `swarm: …` line per network in the log (ADR 0026), once: when
    /// the node first has a neighbor, and the gossip had
    /// [`SWARM_LINE_SETTLE`] to bring the others.
    fn spawn_swarm_line(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(node) = weak.upgrade() else { return };
                if node.endpoint().is_closed() {
                    return;
                }
                if !node.neighbors().is_empty() {
                    break;
                }
            }
            tokio::time::sleep(SWARM_LINE_SETTLE).await;
            let Some(node) = weak.upgrade() else { return };
            for line in crate::swarm::lines(&node.swarm_json(), true) {
                info!("{line}");
            }
        });
    }

    /// The reconnect loop ([`crate::reconnect`]) and the network watchers
    /// that start its backoff over.
    fn spawn_reconnect(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(supervise("reconnect", move || {
            let weak = weak.clone();
            async move { reconnect_loop(weak).await }
        }));
        // Loopback: fixed addresses and no relay, nothing to watch.
        if self.config.mode == NetworkMode::Loopback {
            return;
        }
        let weak = Arc::downgrade(self);
        let mut addr = self.endpoint().watch_addr();
        tokio::spawn(async move {
            use iroh::Watcher as _;
            while addr.updated().await.is_ok() {
                let Some(node) = weak.upgrade() else { break };
                debug!("our addresses changed");
                node.network_changed();
            }
        });
        let weak = Arc::downgrade(self);
        let mut relays = self.endpoint().home_relay_status();
        tokio::spawn(async move {
            use iroh::Watcher as _;
            let connected =
                |s: &Vec<iroh::endpoint::RelayStatus>| s.iter().any(|r| r.is_connected());
            let mut was = connected(&relays.get());
            while let Ok(status) = relays.updated().await {
                let Some(node) = weak.upgrade() else { break };
                let now = connected(&status);
                if now && !was {
                    debug!("home relay connected again");
                    node.network_changed();
                }
                was = now;
            }
        });
    }

    fn health_applies(&self, what: crate::health::What) -> bool {
        self.health.lock().map(|h| h.applies(what)).unwrap_or(false)
    }

    /// Feeds observations to the health state machine; logs what changed.
    fn observe_health(&self, observed: Vec<(crate::health::What, Option<crate::health::Problem>)>) {
        let now = now_secs();
        let transitions: Vec<crate::health::Transition> = match self.health.lock() {
            Ok(mut health) => observed
                .into_iter()
                .filter_map(|(what, problem)| health.observe(what, problem, now))
                .collect(),
            Err(_) => return,
        };
        for transition in transitions {
            transition.log();
        }
    }

    /// The health checks: relay, bootstrap and peers every
    /// [`crate::health::CHECK_EVERY`], the directories every
    /// [`crate::health::LOOKUP_PROBE_EVERY`]. Checks that cannot apply
    /// never run.
    fn spawn_health_checks(self: &Arc<Self>) {
        use crate::health::What;
        let weak = Arc::downgrade(self);
        tokio::spawn(supervise("health checks", move || {
            let weak = weak.clone();
            async move {
                loop {
                    tokio::time::sleep(crate::health::CHECK_EVERY).await;
                    let Some(node) = weak.upgrade() else { break };
                    if node.endpoint().is_closed() {
                        break;
                    }
                    node.check_health().await;
                }
            }
        }));
        if self.health_applies(What::Lookup) {
            let weak = Arc::downgrade(self);
            tokio::spawn(supervise("lookup probe", move || {
                let weak = weak.clone();
                async move {
                    tokio::time::sleep(crate::health::LOOKUP_FIRST_PROBE_AFTER).await;
                    loop {
                        let Some(node) = weak.upgrade() else { break };
                        if node.endpoint().is_closed() {
                            break;
                        }
                        let mut probes = Vec::new();
                        for directory in node.config.infra.publish_to() {
                            let probe =
                                crate::health::probe(node.endpoint(), &directory, now_secs()).await;
                            probes.push((directory, probe));
                        }
                        if node.endpoint().is_closed() {
                            break;
                        }
                        let problem = crate::health::lookup_problem(&probes);
                        node.observe_health(vec![(What::Lookup, problem)]);
                        drop(node);
                        tokio::time::sleep(crate::health::LOOKUP_PROBE_EVERY).await;
                    }
                }
            }));
        }
    }

    async fn check_health(&self) {
        use crate::health::{
            What, bootstrap_problem, peers_problem, relay_problem, storage_problem,
        };
        use iroh::Watcher as _;
        let mut observed = Vec::new();
        if self.health_applies(What::Relay) {
            let home: Vec<(String, bool)> = self
                .endpoint()
                .home_relay_status()
                .get()
                .iter()
                .map(|s| (s.url().to_string(), s.is_connected()))
                .collect();
            observed.push((What::Relay, relay_problem(&self.config.infra, &home)));
        }
        let neighbors = self.neighbors();
        if self.health_applies(What::Bootstrap) {
            let mut reachable = self.seed_ids.iter().any(|id| neighbors.contains(id));
            for id in &self.seed_ids {
                if reachable {
                    break;
                }
                if let Some(info) = self.endpoint().remote_info(*id).await {
                    reachable = info
                        .addrs()
                        .any(|a| matches!(a.usage(), iroh::endpoint::TransportAddrUsage::Active));
                }
            }
            let newest = if reachable {
                None
            } else {
                let mut cached =
                    crate::peer_cache::load(&self.config.data_dir, self.endpoint_id(), now_secs());
                if self.config.mode.is_local() {
                    cached = crate::peer_cache::loopback_only(cached);
                }
                cached.iter().map(|p| p.last_seen).max()
            };
            observed.push((
                What::Bootstrap,
                bootstrap_problem(reachable, newest, now_secs()),
            ));
        }
        if self.health_applies(What::Peers) {
            observed.push((What::Peers, peers_problem(neighbors.len())));
        }
        if self.health_applies(What::Storage) {
            observed.push((
                What::Storage,
                storage_problem(self.blob_gc_stopped(now_secs())),
            ));
        }
        self.observe_health(observed);
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
        let author = crate::identity::author_id(&key);
        let new = self
            .local_authors
            .lock()
            .is_ok_and(|mut local| local.insert(author.clone(), profile.to_string()).is_none());
        // Rotation keeps this profile's posts, across restarts (ADR 0025 §7).
        if new && let Err(e) = self.store.note_local_key(&author, profile, now_secs()) {
            warn!(profile, "profile key not recorded: {e:#}");
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
                .is_ok_and(|local| local.contains_key(author))
    }

    /// The profile of a local author id: the default profile's, or one used
    /// since the node started.
    pub fn local_profile(&self, author: &str) -> Option<String> {
        if author == self.author_id() {
            return Some(self.config.profile.clone());
        }
        self.local_authors
            .lock()
            .ok()
            .and_then(|local| local.get(author).cloned())
    }

    /// Whether `ask` is the human's (`notify.desktop`): asked from a human
    /// MCP session (its `ask_notify` row, kept across restarts), or by a
    /// local profile that is not an agent's ([`NodeConfig::is_agent_profile`]).
    pub fn is_human_ask(&self, ask: &Event) -> bool {
        if ask.kind != Kind::Ask || ask.thread.is_some() {
            return false;
        }
        let profile = self.local_profile(&ask.author);
        if profile
            .as_deref()
            .is_some_and(|p| self.config.is_agent_profile(p))
        {
            return false;
        }
        profile.is_some() || self.store.ask_notify(&ask.id).ok().flatten().is_some()
    }

    /// Author ids of this node's profiles: those used since it started,
    /// and those of its config (default profile, agents, runners, review).
    /// The node groups them as one author: their signals on each other's
    /// posts count zero (ADR 0019 §2.4).
    pub fn local_authors(&self) -> HashSet<String> {
        let mut local: HashSet<String> = self
            .local_authors
            .lock()
            .map(|l| l.keys().cloned().collect())
            .unwrap_or_default();
        local.insert(self.author_id());
        local.extend(configured_authors(&self.root, &self.config));
        local
    }

    /// The reputation table of network `net`, as this node sees it (ADR
    /// 0019 §2): cached, rebuilt when the store changed and the table is
    /// older than `reputation.ttl`.
    pub fn reputation(&self, net: &str) -> Result<Arc<crate::reputation::Table>> {
        self.reputation.get(
            &self.store,
            net,
            &self.local_authors(),
            &crate::reputation::PowPolicy::of(&self.config),
            &self.config.reputation,
        )
    }

    /// Whether `author` is the key of one of this node's agents (a runner
    /// profile, ADR 0021): a local author that is not the human.
    pub fn is_local_agent_author(&self, author: &str) -> bool {
        self.config.runner_profiles().into_iter().any(|profile| {
            is_valid_profile(profile) && author_id(&self.root.profile_key(profile)) == author
        })
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

    /// When this node started (Unix seconds).
    pub fn started_at(&self) -> u64 {
        self.started_at
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Our address, as a ticket other nodes can bootstrap from. In
    /// Internet mode, waits (bounded) for a home relay first.
    pub async fn ticket(&self) -> EndpointTicket {
        if self.config.mode == NetworkMode::Internet
            && self.config.infra.has_relays()
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
        let senders: Vec<GossipSender> = self
            .senders
            .lock()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default();
        for sender in senders {
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
            // Held by a temporary tag until pinned under our own name: no
            // automatic tag, so that dropping the post frees the blob. The
            // collection keeps it meanwhile: the store creates that tag
            // before it records the blob, and our callback reads temporary
            // tags before the mark reads tags (see `blobs_to_keep`).
            let added = self.blobs.add_path(&path).temp_tag().await?;
            let hash = added.hash();
            self.pin(hash).await?;
            // A collection deleting the same content (a dropped post's file
            // published again) can leave the entry without its file: import
            // it again, from the file we still have.
            let mut tries: u32 = 0;
            while self.blob_unreadable_settled(hash).await? {
                tries = tries.saturating_add(1);
                if tries > 3 {
                    bail!("{} could not be stored, try again", path.display());
                }
                let again = self.blobs.add_path(&path).temp_tag().await?;
                if !self.blob_unreadable_settled(hash).await? {
                    self.blob_restored(hash);
                }
                drop(again);
            }
            drop(added);
            refs.push(BlobRef {
                hash: hash.to_hex().to_string(),
                name,
                size: meta.len(),
                url: None,
            });
        }
        Ok(refs)
    }

    /// Keep blob `hash` from the collection while it is brought in, until
    /// the returned hold is dropped (after [`Node::pin`]). Taken before
    /// the first byte is written.
    async fn hold_blob(&self, hash: Hash) -> BlobHold {
        if let Ok(mut held) = self.blob_keep.held.lock() {
            let count = held.entry(hash).or_insert(0);
            *count = count.saturating_add(1);
        }
        let tag = match self.blobs.tags().temp_tag(hash).await {
            Ok(tag) => Some(tag),
            Err(e) => {
                // Still held in memory: only a run whose callback came
                // before this point could miss it.
                debug!(%hash, "no temporary tag for a blob in flight: {e:#}");
                None
            }
        };
        BlobHold {
            keep: Arc::clone(&self.blob_keep),
            hash,
            _tag: tag,
        }
    }

    /// Whether blob `hash` is recorded complete but cannot be read: its
    /// file is gone (a race in the blob store's collection, a disk fault).
    /// Notes it for [`Node::repair_blob`], with one WARN per blob.
    ///
    /// The file is checked on disk: a store that still has the blob open
    /// reads it from the deleted file until it reopens it ("poisoned
    /// storage" from then on).
    async fn blob_unreadable(&self, hash: Hash) -> bool {
        let Ok(BlobStatus::Complete { size }) = self.blobs.status(hash).await else {
            return false;
        };
        if size == 0 {
            return false;
        }
        // Above this size the store keeps the data in a file, not inline.
        let inlined = iroh_blobs::store::fs::options::InlineOptions::default().max_data_inlined;
        let file = self.blob_data_file(hash);
        let gone = size > inlined && !tokio::fs::try_exists(&file).await.unwrap_or(true);
        if !gone
            && self
                .blobs
                .export_ranges(hash, 0..1u64)
                .concatenate()
                .await
                .is_ok()
        {
            return false;
        }
        if self.blob_keep.note_unreadable(hash) {
            warn!(%hash, "blob recorded but unreadable (its file is gone): fetching it again from peers");
        }
        true
    }

    /// [`Node::blob_unreadable`] once the blob store committed what it
    /// had pending. A collection's sweep removes the files of what it
    /// deletes when it commits, which can come after an import of the
    /// same content returned (the import's entry then has no file): its
    /// `sync_db` ends the open transaction first.
    async fn blob_unreadable_settled(&self, hash: Hash) -> Result<bool> {
        self.blobs.sync_db().await?;
        Ok(self.blob_unreadable(hash).await)
    }

    /// Blob `hash`, found unreadable, reads again.
    fn blob_restored(&self, hash: Hash) {
        if let Ok(mut u) = self.blob_keep.unreadable.lock() {
            u.remove(&hash);
        }
        info!(%hash, "unreadable blob restored");
    }

    /// Restore every blob found unreadable, from the peers likely to have
    /// it; the ones none had stay noted for the next pass.
    async fn repair_unreadable(&self) {
        self.forget_unkept_unreadable();
        let hashes: Vec<Hash> = self
            .blob_keep
            .unreadable
            .lock()
            .map(|u| u.iter().copied().collect())
            .unwrap_or_default();
        for hash in hashes {
            let providers = self.providers_for_hash(&hash.to_hex());
            self.repair_blob(hash, None, providers).await;
        }
    }

    /// Forget the unreadable blobs no kept post lists any more (their post
    /// was dropped or retracted since): nothing to repair.
    fn forget_unkept_unreadable(&self) {
        let Ok(kept) = self.store.referenced_blobs() else {
            return;
        };
        let kept: HashSet<Hash> = kept.iter().filter_map(|h| h.parse().ok()).collect();
        if let Ok(mut unreadable) = self.blob_keep.unreadable.lock() {
            unreadable.retain(|hash| kept.contains(hash));
        }
    }

    /// Blobs found recorded but unreadable and not repaired yet, among
    /// those a kept post still lists.
    pub fn unreadable_blobs(&self) -> usize {
        self.forget_unkept_unreadable();
        self.blob_keep.unreadable.lock().map_or(0, |u| u.len())
    }

    /// Put an unreadable blob back (see [`Node::blob_unreadable`]): fetch it
    /// from `providers` into a scratch file, BLAKE3-verified, then import
    /// it again. The blob store has no way to reset an entry, but an
    /// import writes the file and the entry anew, even over a complete
    /// entry whose file is gone. With `size`, only that many bytes are
    /// accepted (B10).
    async fn repair_blob(&self, hash: Hash, size: Option<u64>, providers: Vec<EndpointId>) -> bool {
        let _hold = self.hold_blob(hash).await;
        let me = self.endpoint_id();
        let candidates: Vec<EndpointId> = providers
            .into_iter()
            .filter(|p| *p != me && !self.provider_backing_off(p))
            .take(MAX_PROVIDER_ATTEMPTS)
            .collect();
        if candidates.is_empty() {
            debug!(%hash, "no provider to repair the blob from");
            return false;
        }
        static REPAIRS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = REPAIRS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.config.data_dir.join("blobs-repair").join(format!(
            "{}.{}.{n}",
            hash.to_hex(),
            std::process::id()
        ));
        let repaired = self.repair_into(hash, size, candidates, &dir).await;
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(dir = %dir.display(), "scratch file of a repair not removed: {e:#}");
        }
        match repaired {
            Ok(true) => {
                self.blob_restored(hash);
                true
            }
            Ok(false) => false,
            Err(e) => {
                warn!(%hash, "unreadable blob not restored: {e:#}");
                false
            }
        }
    }

    /// [`Node::repair_blob`] into `dir`, from the first of `candidates`
    /// that has the blob. Fetched without a blob store: the downloader
    /// would trust the main store's entry, and a scratch store would leak
    /// one thread per repair when closed, for as long as the node runs
    /// (`docs/upstream/iroh-blobs-runtime-drop-issue.md`).
    async fn repair_into(
        &self,
        hash: Hash,
        size: Option<u64>,
        candidates: Vec<EndpointId>,
        dir: &Path,
    ) -> Result<bool> {
        tokio::fs::create_dir_all(dir).await?;
        let file = dir.join("content");
        let request = match size {
            Some(size) => GetRequest::builder()
                .root(ChunkRanges::chunks(..size.div_ceil(1024).max(1)))
                .build(hash),
            None => GetRequest::builder().root(ChunkRanges::all()).build(hash),
        };
        let mut fetched = false;
        for provider in candidates {
            let attempt = self.fetch_verified(provider, request.clone(), size, &file);
            match tokio::time::timeout(Duration::from_secs(60), attempt).await {
                Ok(Ok(true)) => {
                    self.provider_ok(provider);
                    fetched = true;
                    break;
                }
                // Another size than the one asked for.
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    debug!(%hash, %provider, "repair fetch failed: {e:#}");
                    self.provider_failed(provider);
                }
                Err(_) => {
                    debug!(%hash, %provider, "repair fetch timed out");
                    self.provider_failed(provider);
                }
            }
        }
        if !fetched {
            return Ok(false);
        }
        let added = match self.blobs.add_path(&file).temp_tag().await {
            Ok(added) => added,
            Err(e) => return self.restore_in_place(hash, &file, e.into()).await,
        };
        anyhow::ensure!(added.hash() == hash, "restored content has another hash");
        // Its file holds it again, whether or not the store's entry in
        // memory is still the poisoned one ([`Node::read_blob_file`]);
        // an inlined blob has no file, the store reads it.
        Ok(self.read_blob_file(hash).await.is_some() || !self.blob_unreadable(hash).await)
    }

    /// [`Node::repair_into`] when the store refuses the import (`error`):
    /// iroh-blobs refuses an entry for good, "entity is dead", once one of
    /// its threads panicked on it (anything that observes a poisoned
    /// entry: the downloader, a peer's observe request). Only reopening
    /// the store clears it. The fetched content, checked again against
    /// the hash, then goes where the store's database expects the blob's
    /// file: reads use it meanwhile ([`Node::read_blob_file`]), and the
    /// store reads it again after the node restarts. Never for a blob the
    /// store does not record complete.
    async fn restore_in_place(
        &self,
        hash: Hash,
        fetched: &Path,
        error: anyhow::Error,
    ) -> Result<bool> {
        let inlined = iroh_blobs::store::fs::options::InlineOptions::default().max_data_inlined;
        let size = match self.blobs.status(hash).await {
            Ok(BlobStatus::Complete { size }) if size > inlined => size,
            _ => return Err(error.context("the blob store refused the restored blob")),
        };
        let content = fetched.to_path_buf();
        let intact = tokio::task::spawn_blocking(move || {
            std::fs::read(&content)
                .is_ok_and(|bytes| bytes.len() as u64 == size && Hash::new(&bytes) == hash)
        })
        .await?;
        anyhow::ensure!(intact, "restored content does not match the blob");
        tokio::fs::rename(fetched, self.blob_data_file(hash)).await?;
        warn!(
            %hash,
            "the blob store refuses this blob until the node restarts ({error:#}): \
             restored in its file, read from there meanwhile"
        );
        Ok(self.read_blob_file(hash).await.is_some())
    }

    /// Fetch `request` (one blob) from `provider` into `file`, each chunk
    /// verified against the blob's BLAKE3 hash as it arrives. False, with
    /// nothing read past the header, when `size` is given and the
    /// provider's blob has another size; with the size right, `request`
    /// covers the whole blob, its last chunk included, which proves it.
    async fn fetch_verified(
        &self,
        provider: EndpointId,
        request: GetRequest,
        size: Option<u64>,
        file: &Path,
    ) -> Result<bool> {
        use bao_tree::io::BaoContentItem;
        use iroh_blobs::get::fsm;
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        // Given up after a few seconds, as the downloader did (after 1 s),
        // not after iroh's own 30 s: the next provider may have it.
        let connection = tokio::time::timeout(
            REPAIR_CONNECT_LIMIT,
            self.endpoint().connect(provider, iroh_blobs::ALPN),
        )
        .await
        .map_err(|_| anyhow::anyhow!("no connection within {REPAIR_CONNECT_LIMIT:?}"))??;
        let connected = fsm::start(connection, request, Default::default())
            .next()
            .await?;
        let fsm::ConnectedNext::StartRoot(root) = connected.next().await? else {
            bail!("the provider answered with no blob");
        };
        let (mut content, got) = root.next().next().await?;
        if size.is_some_and(|size| size != got) {
            return Ok(false);
        }
        let mut out = tokio::io::BufWriter::new(tokio::fs::File::create(file).await?);
        let mut at = 0u64;
        let end = loop {
            match content.next().await {
                fsm::BlobContentNext::More((next, item)) => {
                    if let BaoContentItem::Leaf(leaf) = item? {
                        if leaf.offset != at {
                            out.seek(std::io::SeekFrom::Start(leaf.offset)).await?;
                        }
                        out.write_all(&leaf.data).await?;
                        at = leaf.offset.saturating_add(leaf.data.len() as u64);
                    }
                    content = next;
                }
                fsm::BlobContentNext::Done(end) => break end,
            }
        };
        out.flush().await?;
        if let fsm::EndBlobNext::Closing(closing) = end.next() {
            closing.next().await?;
        }
        Ok(true)
    }

    /// Blob collections begun since the node started, aborted ones
    /// included (0 with `storage.gc = false`). Runs follow one another:
    /// once this grew by 2, a whole run started and ended in between.
    pub fn blob_collections(&self) -> u64 {
        self.blob_keep
            .runs
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether blob collection stopped, at `now` (Unix seconds):
    /// `storage.gc` is on and no run got past reading what to keep for two
    /// intervals and ten minutes. The blob store ends its collection at the
    /// first error of a run, and a store whose database failed fails every
    /// read; either way only a restart resumes it, and nothing is deleted
    /// until then. The `storage` health check raises it (one WARN).
    pub fn blob_gc_stopped(&self, now: u64) -> bool {
        if !self.config.blob_gc {
            return false;
        }
        let last = (self
            .blob_keep
            .last_kept
            .load(std::sync::atomic::Ordering::Relaxed)
            / 1000)
            .max(self.started_at);
        let limit = self
            .config
            .blob_gc_interval
            .as_secs()
            .saturating_mul(2)
            .saturating_add(600);
        now.saturating_sub(last) > limit
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
        let Some(sender) = self.sender(&draft.net) else {
            bail!("this node has not joined network {:?}", draft.net);
        };
        if draft.net == PUBLIC_NET
            && let Some(halt) = self.halt_in_force()
        {
            bail!(
                "the public network is halted by guardians: {} (halt {})",
                halt.reason,
                halt.id
            );
        }
        if draft
            .kind
            .as_ref()
            .is_some_and(|k| crate::retract::is_retract(k.as_str()))
        {
            crate::retract::check_policy(
                draft.thread.as_deref(),
                draft.parent.as_deref(),
                &draft.refs,
            )?;
        }
        draft.blobs.extend(self.import(attachments).await?);
        let key = key.clone();
        // Mine what receivers will require for this time, author and size.
        let ts = now_secs();
        let base = match self.config.pow_bits {
            Some(bits) => bits,
            None => self.policy_pow_bits(&hex::encode(key.verifying_key().as_bytes()), ts, true)?,
        };
        let pow_bits = crate::event::required_pow_bits(base, draft.estimated_size(&key)?);
        let event = tokio::task::spawn_blocking(move || draft.sign(&key, ts, pow_bits)).await??;
        if matches!(self.store.insert(&event, ts)?, Inserted::New(_)) {
            self.on_stored(&AnyEvent::Known(event.clone())).await;
        }
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

    /// Retract posts of `key` (ADR 0020 §5.1): one `x.wearetheswarm.retract`
    /// per network and per 32 posts, signed with `key`, published and
    /// applied here (the bodies are dropped at once). Every post must be on
    /// this node, by `key` or a key it rotated from, and not a key
    /// succession nor a halt event. Checked before anything is published.
    pub async fn retract_as(
        self: &Arc<Self>,
        key: &SigningKey,
        ids: &[String],
        reason: Option<&str>,
    ) -> Result<Vec<Event>> {
        let plan = self.retraction_plan(&author_id(key), ids)?;
        // Checked before anything goes out.
        crate::retract::reason_text(reason)?;
        let mut published = Vec::new();
        for (net, refs) in plan {
            for chunk in refs.chunks(crate::retract::MAX_REFS) {
                let draft = crate::retract::draft(&net, chunk.to_vec(), reason)?;
                published.push(self.publish_as(key, draft, &[]).await?);
            }
        }
        Ok(published)
    }

    /// The posts `ids` grouped by network, once each is checked: held here
    /// (full or tombstone), by `author` or a key it rotated from, and
    /// retractable. The errors name the post.
    pub fn retraction_plan(
        &self,
        author: &str,
        ids: &[String],
    ) -> Result<Vec<(String, Vec<String>)>> {
        if ids.is_empty() {
            bail!("name at least one post to retract");
        }
        let mut plan: Vec<(String, Vec<String>)> = Vec::new();
        for id in ids {
            crate::event::parse_hex::<32>(id, "post id")?;
            let Some((_, _, wire)) = self.store.get_raw(id)? else {
                bail!("post {id} is not on this node");
            };
            let field = |k: &str| wire.get(k).and_then(serde_json::Value::as_str);
            let (Some(net), Some(post_author), Some(kind)) =
                (field("net"), field("author"), field("kind"))
            else {
                bail!("post {id} is malformed here");
            };
            if crate::retract::unretractable(kind) {
                bail!("post {id} is a {kind} event: key successions and halts cannot be retracted");
            }
            if !self.store.rotated_into(author, post_author)? {
                bail!(
                    "post {id} is not yours (its author is {post_author}): retract only your own posts"
                );
            }
            match plan.iter_mut().find(|(n, _)| n == net) {
                Some((_, refs)) if refs.contains(id) => {}
                Some((_, refs)) => refs.push(id.clone()),
                None => plan.push((net.to_string(), vec![id.clone()])),
            }
        }
        Ok(plan)
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
                // No more requests at once than the peer fetches blobs at
                // once: it would skip the others.
                let slot = match self.pushes.acquire(peer, PUSH_WAIT).await {
                    Ok(slot) => slot,
                    Err(why) => {
                        warn!(peer = %peer.fmt_short(), ?why, "replication skipped: too many pushes to this peer");
                        return (peer, None);
                    }
                };
                let result = tokio::time::timeout(
                    PUSH_TIMEOUT,
                    proto::call_solving(self.endpoint(), peer, &request),
                )
                .await;
                drop(slot);
                (peer, Some(result))
            }
        });
        let mut confirmed: HashMap<String, Vec<EndpointId>> = HashMap::new();
        for (peer, result) in n0_future::join_all(calls).await {
            let Some(result) = result else { continue };
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
        // Bounded: a collector that never answers held `wats report send`
        // (and a CI test) forever.
        let call = proto::call_solving_within(
            self.endpoint(),
            collector,
            &request,
            Some(REPORT_CONNECT_LIMIT),
        );
        let answer = tokio::time::timeout(REPORT_CALL_LIMIT, call)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "{} did not answer within {} s",
                    collector.fmt_short(),
                    REPORT_CALL_LIMIT.as_secs()
                )
            })??;
        match answer {
            proto::Response::Accepted => Ok(()),
            proto::Response::Unsupported => {
                bail!("{} does not collect crash reports", collector.fmt_short())
            }
            proto::Response::Refused { reason } => bail!("refused: {reason}"),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// Seal a payload to `inbox` and publish it on the public network as v2
    /// `submit` events, one per part, under a fresh one-off author (ADR
    /// 0015 §5). Each part is mined to what v2 receivers require of a
    /// submission: the public schedule (or `pow_bits`), the size bits and
    /// [`SUBMIT_EXTRA_POW_BITS`], no new-author bits. Returns the sealed
    /// parts.
    pub async fn send_to_inbox(
        &self,
        inbox: &crate::inbox::InboxPublic,
        kind: &str,
        payload: Vec<u8>,
    ) -> Result<crate::inbox::Sealed> {
        let ts = now_secs();
        let base = self
            .config
            .pow_bits
            .unwrap_or_else(|| crate::event::scheduled_pow_bits(&self.config.pow_schedule, ts));
        let (inbox, kind) = (inbox.clone(), kind.to_string());
        let sealed = tokio::task::spawn_blocking(move || {
            crate::inbox::seal_payload(&inbox, &kind, &payload, ts, |size| {
                crate::event::required_pow_bits(base, size).saturating_add(SUBMIT_EXTRA_POW_BITS)
            })
        })
        .await??;
        for event in &sealed.events {
            self.publish_signed(event.clone()).await?;
        }
        Ok(sealed)
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

    /// The run a token belongs to, while it runs. The persistent token of
    /// OpenClaw's `wats-agent` MCP entry (`openclaw/runner.token`, ADR 0017
    /// §3) is valid as long as its file exists: OpenClaw starts that MCP
    /// server itself, outside any run.
    pub fn runner_run(&self, token: &str) -> Option<RunnerRun> {
        if let Some(run) = self
            .runner_tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(token).cloned())
        {
            return Some(run);
        }
        crate::openclaw::is_persistent_runner_token(&self.config.data_dir, token).then(|| {
            RunnerRun {
                id: "openclaw".into(),
                events: Vec::new(),
                profile: self
                    .config
                    .runners
                    .iter()
                    .find_map(|r| r.strip_prefix("openclaw as "))
                    .unwrap_or_default()
                    .to_string(),
                file_roots: Vec::new(),
                via: crate::via::Via::of_run("openclaw", None),
            }
        })
    }

    /// Let the run of `token` share files from `dir` too (a Codex run's
    /// image directory, once its thread id is known), until it ends.
    pub fn add_runner_file_root(&self, token: &str, dir: PathBuf) {
        if let Ok(mut tokens) = self.runner_tokens.lock()
            && let Some(run) = tokens.get_mut(token)
            && !run.file_roots.contains(&dir)
        {
            run.file_roots.push(dir);
        }
    }

    /// The directories the run `id` may share files from besides its
    /// session's own, while it runs.
    pub fn runner_file_roots(&self, id: &str) -> Vec<PathBuf> {
        self.runner_tokens
            .lock()
            .ok()
            .and_then(|tokens| {
                tokens
                    .values()
                    .find(|run| run.id == id)
                    .map(|run| run.file_roots.clone())
            })
            .unwrap_or_default()
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
        // The store's free pages are free space (ADR 0025 §7): the file
        // does not shrink, its freed pages are reused.
        let bytes = tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0)
            .saturating_sub(self.store.free_bytes());
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

    /// Read a blob from the local store, if complete. One the store fails
    /// to read is read another way ([`Node::read_held`]).
    pub async fn blob_bytes(&self, hex: &str) -> Result<Option<bytes::Bytes>> {
        let hash: Hash = hex.parse()?;
        if !self.blobs.has(hash).await? {
            return Ok(None);
        }
        self.read_held(hash, None, self.providers_for_hash(hex))
            .await
            .map(Some)
    }

    /// Blob `hash`, complete in the store, read whole. When the store
    /// fails to read it ("poisoned storage", a file gone or damaged): from
    /// its data file if that holds the blob, or else fetched again from
    /// `providers` and imported anew ([`Node::repair_blob`]), then read.
    /// With `size`, only that many bytes are fetched (B10). An error only
    /// when none of these works.
    async fn read_held(
        &self,
        hash: Hash,
        size: Option<u64>,
        providers: Vec<EndpointId>,
    ) -> Result<bytes::Bytes> {
        let error = match self.store_read(hash).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => e,
        };
        if let Some(bytes) = self.read_blob_file(hash).await {
            debug!(%hash, "the blob store failed to read a blob ({error:#}): read from its file");
            return Ok(bytes);
        }
        if self.blob_keep.note_unreadable(hash) {
            warn!(%hash, "blob recorded but unreadable ({error:#}): fetching it again from peers");
        }
        if self.repair_blob(hash, size, providers).await {
            match self.store_read(hash).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    if let Some(bytes) = self.read_blob_file(hash).await {
                        debug!(%hash, "the blob store does not read the restored blob yet ({e:#}): read from its file");
                        return Ok(bytes);
                    }
                }
            }
        }
        Err(error.context("blob unreadable, and no peer could restore it"))
    }

    /// Blob `hash` read whole through the store, only if it gives all of
    /// it: an entry the store refuses ("entity is dead") can first read as
    /// no bytes at all.
    async fn store_read(&self, hash: Hash) -> Result<bytes::Bytes> {
        let bytes = self.blobs.get_bytes(hash).await?;
        match self.blobs.status(hash).await? {
            BlobStatus::Complete { size } if size == bytes.len() as u64 => Ok(bytes),
            status => bail!(
                "the blob store read {} bytes of a blob it records as {status:?}",
                bytes.len()
            ),
        }
    }

    /// The data file of blob `hash` in the blob store, when it is not
    /// inlined in its database.
    fn blob_data_file(&self, hash: Hash) -> PathBuf {
        self.config
            .data_dir
            .join("blobs/data")
            .join(format!("{}.data", hash.to_hex()))
    }

    /// Blob `hash` read from its data file, if the file holds exactly the
    /// blob (BLAKE3-checked): what a read falls back to while the store
    /// fails to read a blob whose file is intact. The store keeps an entry
    /// whose file failed to open as "poisoned" in memory, and an import of
    /// the same blob does not replace it: it reads the blob again only once
    /// nothing uses the entry and it reopens it.
    async fn read_blob_file(&self, hash: Hash) -> Option<bytes::Bytes> {
        let file = self.blob_data_file(hash);
        tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&file).ok()?;
            (Hash::new(&bytes) == hash).then(|| bytes::Bytes::from(bytes))
        })
        .await
        .ok()
        .flatten()
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

    /// Heard from `id` on `net` (ADR 0026): `nodes_24h` of `swarm`.
    fn sighted(&self, net: &str, id: EndpointId) {
        if id == self.endpoint_id() {
            return;
        }
        if let Ok(mut swarm) = self.swarm.lock() {
            swarm.note(net, id, now_secs());
        }
    }

    /// The `swarm` value of `status` (ADR 0026), per network joined: the
    /// gossip neighbors now, and the distinct nodes heard from on it in
    /// the last 24 hours (with the neighbors).
    pub fn swarm_json(&self) -> serde_json::Value {
        let now = now_secs();
        let nets = self.joined_nets();
        let neighbors = self.neighbors.lock().map(|n| n.clone()).unwrap_or_default();
        let swarm = self.swarm.lock();
        nets.into_iter()
            .map(|net| {
                let up = neighbors.get(&net).cloned().unwrap_or_default();
                let seen = swarm.as_ref().map_or(up.len(), |s| s.count(&net, now, &up));
                (net, crate::swarm::net_json(up.len(), seen))
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    /// Persist the sightings of the last 24 hours (ADR 0026).
    pub async fn save_swarm(&self) {
        let now = now_secs();
        let Some(sightings) = self.swarm.lock().ok().map(|mut s| {
            s.prune(now);
            s.clone()
        }) else {
            return;
        };
        let dir = self.config.data_dir.clone();
        match tokio::task::spawn_blocking(move || sightings.save(&dir, now)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("swarm sightings not saved: {e:#}"),
            Err(e) => warn!("swarm sightings not saved: {e}"),
        }
    }

    fn seen(&self, peer: EndpointId) {
        self.durability.peer_seen(peer);
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
        self.providers_for_hash(&blob.hash)
    }

    /// [`Node::providers_for`] a blob known by its hash only.
    fn providers_for_hash(&self, hex: &str) -> Vec<EndpointId> {
        let mut out: Vec<EndpointId> = self
            .hints
            .lock()
            .ok()
            .and_then(|h| h.get(hex).cloned())
            .unwrap_or_default();
        if let Ok(hash) = crate::event::parse_hex::<32>(hex, "hash") {
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
    pub async fn fetch_and_export(&self, post: &str, blob: &BlobRef) -> Result<Option<PathBuf>> {
        if blob.size > self.config.max_blob_bytes {
            bail!(
                "blob is larger than this node's {} byte limit",
                self.config.max_blob_bytes
            );
        }
        let hash: Hash = blob.hash.parse()?;
        if !self.blobs.has(hash).await? || self.blob_unreadable(hash).await {
            // A read waits for its file: a provider that failed once (a
            // connection reset, a proof in progress that refuses the blob
            // meanwhile, ADR 0029) is asked again, after a pause, instead of
            // giving up on the first miss. The retries do not skip the
            // providers this read marked as failing.
            let mut fetched = false;
            for attempt in 0..READ_FETCH_TRIES {
                if attempt > 0 {
                    tokio::time::sleep(READ_FETCH_PAUSE.saturating_mul(attempt)).await;
                }
                let providers = self.providers_for(blob);
                if providers.is_empty() {
                    break;
                }
                if self.fetch_blob_from(blob, providers, attempt > 0).await {
                    fetched = true;
                    break;
                }
                debug!(hash = %blob.hash, attempt, "blob not fetched on read yet");
            }
            if !fetched {
                return Ok(None);
            }
        }
        self.export_blob(post, blob).await
    }

    /// Copy a complete local blob to `<data dir>/inbox/<post>/<name>`
    /// (`post`: the id of the post that lists it, its first 16 characters),
    /// read-only, and return the path: a post's files sit together (0.4.12;
    /// one directory per blob hash before). A name the post's directory
    /// already holds with other content gets `-1`, `-2`… before its
    /// extension; the same content is found again. Received files are
    /// untrusted data: they are exported for reading, never executed (SPEC
    /// §2.11, §2.15).
    pub async fn export_blob(&self, post: &str, blob: &BlobRef) -> Result<Option<PathBuf>> {
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
        anyhow::ensure!(
            post.len() >= 16 && post.bytes().all(|b| b.is_ascii_hexdigit()),
            "not a post id: {post:?}"
        );
        let inbox = std::path::absolute(self.config.data_dir.join("inbox"))?;
        let dir = inbox.join(&post[..16]);
        real_dir(&inbox).await?;
        real_dir(&dir).await?;
        let mut path = dir.join(&blob.name);
        for n in 1.. {
            match tokio::fs::symlink_metadata(&path).await {
                Ok(meta) if meta.file_type().is_file() => {
                    // The same file again, or another one under that name.
                    if meta.len() == blob.size
                        && tokio::fs::read(&path)
                            .await
                            .is_ok_and(|bytes| Hash::new(&bytes) == hash)
                    {
                        return Ok(Some(path));
                    }
                    path = dir.join(numbered(&blob.name, n));
                }
                Ok(_) => bail!("{} exists and is not a regular file", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e.into()),
            }
            anyhow::ensure!(
                n < 1000,
                "too many files named {} in {}",
                blob.name,
                dir.display()
            );
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
            let whole = match tokio::io::copy(&mut reader, &mut file).await {
                Ok(copied) if copied == blob.size => true,
                Ok(copied) => {
                    debug!(%hash, copied, "blob export short: reading it another way");
                    false
                }
                Err(e) => {
                    debug!(%hash, "blob export failed ({e:#}): reading it another way");
                    false
                }
            };
            if !whole {
                // A blob the store holds but fails to read: read another
                // way, or fetched again, and written from the start.
                let bytes = self
                    .read_held(hash, Some(blob.size), self.providers_for(blob))
                    .await?;
                use tokio::io::{AsyncSeekExt, AsyncWriteExt};
                file.set_len(0).await?;
                file.seek(std::io::SeekFrom::Start(0)).await?;
                file.write_all(&bytes).await?;
            }
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

    /// A fresh subscription to `net`'s topic, through this node's current
    /// neighbors and known peers, after its receiver ended: the new sender
    /// replaces the old one. `None` (logged) when it fails, or when the
    /// network was left meanwhile.
    async fn resubscribe(&self, net: &str) -> Option<GossipReceiver> {
        let mut peers = self.neighbors_of(net);
        for p in self.known_peers() {
            if !peers.contains(&p) {
                peers.push(p);
            }
        }
        match self.gossip.subscribe(topic_for(net), peers).await {
            Ok(topic) => {
                let (sender, receiver) = topic.split();
                let mut senders = self.senders.lock().ok()?;
                if !senders.contains_key(net) {
                    return None;
                }
                senders.insert(net.to_string(), sender);
                warn!(%net, "gossip receive loop ended: subscribed to the network again");
                Some(receiver)
            }
            Err(e) => {
                warn!(%net, "gossip receive loop ended, subscribing again failed: {e:#}");
                None
            }
        }
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
                    self.sighted(&net, msg.delivered_from);
                    self.heard(&net, msg.delivered_from);
                    if links::is_ping(&msg.content) {
                        self.ping_heard();
                        continue;
                    }
                    // Everything on this network waits on it: a handler that
                    // hangs makes the node deaf, so it says so.
                    let handled = self.handle_message(&net, &msg.content, msg.delivered_from);
                    tokio::pin!(handled);
                    let result = tokio::select! {
                        result = &mut handled => result,
                        () = tokio::time::sleep(SLOW_HANDLING) => {
                            warn!(
                                %net,
                                from = %msg.delivered_from.fmt_short(),
                                "a gossip message takes over {} s to handle: nothing else is \
                                 heard on this network meanwhile",
                                SLOW_HANDLING.as_secs()
                            );
                            handled.await
                        }
                    };
                    if let Err(e) = result {
                        debug!(from = %msg.delivered_from, "dropped message: {e:#}");
                    }
                }
                Some(Ok(GossipEvent::NeighborUp(id))) => {
                    debug!(%net, peer = %id, "neighbor up");
                    // The first neighbor: this node was offline on `net`.
                    let mut first = false;
                    if let Ok(mut n) = self.neighbors.lock() {
                        let set = n.entry(net.clone()).or_default();
                        first = set.is_empty();
                        set.insert(id);
                    }
                    if first && !self.config.history.legacy {
                        self.note_offline(&net);
                    }
                    self.reconnect_kick.notify_one();
                    self.seen(id);
                    self.sighted(&net, id);
                    // Heard: a grace before the link check judges it.
                    self.heard(&net, id);
                    let node = Arc::clone(&self);
                    let net = net.clone();
                    tokio::spawn(async move {
                        if node.peer_info(&id).is_none()
                            && let Err(e) = node.hello(id).await
                        {
                            debug!(peer = %id.fmt_short(), "hello failed: {e:#}");
                        }
                        // The history task walks what is missing, newest
                        // first (ADR 0025); a 0.4.0 node pulled from every
                        // new neighbor, ascending.
                        if !node.config.history.legacy {
                            node.kick_history(&net, &[id], first);
                            return;
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
                    warn!(%net, "gossip receive loop ended on an error: {e:#}");
                    return;
                }
                None => {
                    warn!(%net, "gossip receive loop ended: the topic's stream ended");
                    return;
                }
            }
        }
    }

    async fn handle_message(&self, net: &str, bytes: &[u8], from: EndpointId) -> Result<()> {
        // Untrusted: parse under the strict profile (ADR 0010, A3).
        let envelope: Envelope = crate::strict_json::from_slice(bytes)?;
        if envelope.ev < 1 {
            bail!("bad envelope version {}", envelope.ev);
        }
        let Some((event, new)) = self.accept_value(envelope.event, Origin::Pushed).await? else {
            return Ok(());
        };
        if new {
            self.note_arrivals(false, 1);
        }
        if event.net() != net {
            bail!("event for net {:?} on topic {net:?}", event.net());
        }
        // The node that broadcast a new, valid event: the first provider
        // of its envelope (ADR 0026). A claim, one per new event.
        if new && let Some(origin) = envelope.providers.first().and_then(|p| p.parse().ok()) {
            self.sighted(net, origin);
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

    /// Drop the stored v2 `submit` events older than `submit.retention`
    /// (ADR 0015 §5). Returns how many were dropped.
    pub fn drop_expired_submits(&self) -> Result<usize> {
        let Some(retention) = self.config.submit_retention else {
            return Ok(0);
        };
        let before = now_secs().saturating_sub(retention.as_secs());
        let dropped = self.store.delete_submits_before(before)?;
        if dropped > 0 {
            info!(dropped, "dropped submissions older than submit.retention");
        }
        Ok(dropped)
    }

    /// Delete the `evicted` drop records older than 30 days (ADR 0020 §2).
    pub fn expire_drop_records(&self) -> Result<usize> {
        let before = now_secs().saturating_sub(crate::store::EVICTED_RECORD_SECS);
        let expired = self.store.expire_evicted_records(before)?;
        if expired > 0 {
            debug!(expired, "expired evicted drop records");
        }
        Ok(expired)
    }

    /// Drop the body of event `id` here (ADR 0020 §2): the store keeps its
    /// tombstone and a drop record, and the blobs no kept event lists are
    /// untagged, for the next collection to delete them.
    ///
    /// For storage pressure (`evicted`), last-copy protection first (ADR
    /// 0029 §4): a replica stays (the body goes, the blob is kept and
    /// recorded, so that a later retraction, erasure, label or `wats drop`
    /// releases it); any other copy is released only once other nodes
    /// proved they hold it, else the eviction is refused with
    /// [`durability::KeptLastCopy`]. Every other reason deletes at once,
    /// replicas kept by an earlier eviction included: `wats drop` is a
    /// human's compliance tool, and retractions and labels mean the content
    /// must not be kept.
    pub async fn drop_body(
        &self,
        id: &str,
        reason: DropReason,
        trigger: Option<&str>,
        detail: Option<&str>,
    ) -> Result<DropOutcome> {
        let mut kept: Vec<(String, usize)> = Vec::new();
        let mut planned: Vec<String> = Vec::new();
        let mut ks: HashMap<String, usize> = HashMap::new();
        let before = if reason == DropReason::Evicted {
            self.store.get(id)?.map(|s| s.event)
        } else {
            None
        };
        if let Some(event) = &before {
            let plan = self.plan_eviction(id, event).await?;
            if !plan.stranded.is_empty() {
                return Err(durability::KeptLastCopy {
                    id: id.to_string(),
                    blobs: plan.stranded,
                }
                .into());
            }
            planned.extend(plan.released);
            planned.extend(plan.kept.iter().map(|(h, _)| h.clone()));
            kept = plan.kept;
            ks = plan.ks;
        }
        // The kept replicas are recorded in the drop's own transaction
        // (N8): no crash leaves a kept tag without its record.
        let mut outcome =
            self.store
                .drop_body_keeping(id, reason, trigger, detail, now_secs(), &kept)?;
        if !kept.is_empty() {
            info!(%id, blobs = kept.len(), "body evicted, its replicas kept");
        }
        if let Some(event) = &before {
            // Freed now but not planned: another event that listed them
            // went meanwhile (S7).
            let late: Vec<String> = outcome
                .freed_blobs
                .iter()
                .filter(|h| !planned.contains(h))
                .cloned()
                .collect();
            if !late.is_empty() {
                let late_kept = self.cover_late(event, &late, &ks).await;
                if !late_kept.is_empty() {
                    outcome
                        .freed_blobs
                        .retain(|h| !late_kept.iter().any(|(k, _)| k == h));
                    // An error here leaves the blob tagged, never deleted;
                    // the rest is still untagged below.
                    if let Err(e) = self.store.record_evicted_replicas(id, &late_kept) {
                        warn!(%id, "kept blobs of an evicted body not recorded: {e:#}");
                    }
                }
            }
        }
        self.untag_freed(&outcome.freed_blobs).await;
        // A freed blob no longer needs repairing: no kept post lists it.
        if let Ok(mut unreadable) = self.blob_keep.unreadable.lock() {
            for hash in &outcome.freed_blobs {
                if let Ok(hash) = hash.parse::<Hash>() {
                    unreadable.remove(&hash);
                }
            }
        }
        info!(%id, %reason, held = outcome.held, body = outcome.body_dropped,
              blobs = outcome.freed_blobs.len(), "body dropped");
        Ok(outcome)
    }

    /// Whether this node keeps blob `hash` under a tag.
    pub async fn blob_pinned(&self, hash: &str) -> Result<bool> {
        let hash: Hash = hash.parse()?;
        let mut tags = self.blobs.tags().list().await?;
        while let Some(tag) = tags.next().await {
            if tag?.hash == hash {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The base difficulty of the network policy for an event of `author`
    /// at `ts` (before the size bits): the schedule at `ts`, plus the
    /// new-author bits when `pushed` (ADR 0010, PoW policy).
    fn policy_pow_bits(&self, author: &str, ts: u64, pushed: bool) -> Result<u32> {
        let mut bits = crate::event::scheduled_pow_bits(&self.config.pow_schedule, ts);
        if pushed {
            let known_for = self
                .store
                .author_first_seen(author)?
                .map(|first| now_secs().saturating_sub(first));
            bits = bits.saturating_add(crate::event::new_author_extra_bits(known_for));
        }
        Ok(bits)
    }

    /// Verify an untrusted wire event, apply node policy, store it.
    /// Events of versions this node does not understand are verified
    /// against the invariant core and stored opaque (ADR 0007). Returns the
    /// event and whether it is new; `None` if the policy rejects it.
    async fn accept_value(
        &self,
        value: serde_json::Value,
        origin: Origin,
    ) -> Result<Option<(AnyEvent, bool)>> {
        let event = AnyEvent::from_value(value)?;
        if !self.joined(event.net()) {
            bail!("not a member of network {:?}", event.net());
        }
        // A retraction's rules are node policy (ADR 0020 §5.1).
        if let AnyEvent::Known(e) = &event
            && crate::retract::is_retract(e.kind.as_str())
        {
            crate::retract::check_policy(e.thread.as_deref(), e.parent.as_deref(), &e.refs)?;
        }
        let halt_kind =
            matches!(&event, AnyEvent::Known(e) if crate::halt::is_halt_kind(e.kind.as_str()));
        if !halt_kind && self.halt_serving.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("the public network is halted: this node accepts halt events only");
        }
        // Proof of work grows with size (ADR 0010, limits): node policy.
        let size = match &event {
            AnyEvent::Known(e) => e.canonical_size()?,
            AnyEvent::Opaque(r) => serde_json_canonicalizer::to_vec(&r.wire)?.len(),
        };
        // Opaque events of a version this node does not understand (B4):
        // relayed only a little ahead of what we understand, with more
        // proof of work, within a share of storage.max, and up to a larger
        // size than understood versions, so that a post-quantum version
        // with bigger keys and signatures still travels.
        let unknown_version = match &event {
            AnyEvent::Opaque(r) => !understood(r.v),
            AnyEvent::Known(_) => false,
        };
        let cap = if unknown_version {
            OPAQUE_RELAY_MAX_BYTES
        } else {
            crate::event::MAX_EVENT_BYTES
        };
        if size > cap {
            bail!("event is {size} bytes canonicalized, above the relay limit of {cap}");
        }
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
        // A v2 submission (ADR 0015 §5): the public network's schedule and
        // size bits, no new-author bits (one-off keys by design), plus
        // SUBMIT_EXTRA_POW_BITS; kept for submit.retention only.
        let submit = matches!(&event, AnyEvent::Opaque(r)
            if r.v == crate::v2::VERSION && r.kind == crate::v2::KIND_SUBMIT);
        if submit
            && let Some(retention) = self.config.submit_retention
            && event.ts().saturating_add(retention.as_secs()) < now_secs()
        {
            bail!("submission older than submit.retention: not kept");
        }
        if let Some(work) = event.pow_work() {
            let base = match self.config.min_pow_bits {
                Some(bits) => bits,
                None => self.policy_pow_bits(
                    event.author(),
                    event.ts(),
                    origin == Origin::Pushed && !submit,
                )?,
            };
            let mut required = crate::event::required_pow_bits(base, size);
            if unknown_version {
                required = required.saturating_add(OPAQUE_EXTRA_POW_BITS);
            }
            if submit {
                required = required.saturating_add(SUBMIT_EXTRA_POW_BITS);
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
        // Tombstones of understood versions are not opaque content: they
        // stay out of the quota (ADR 0025 §7).
        if matches!(&event, AnyEvent::Opaque(r) if crate::store::counts_as_opaque(r))
            && let Some(quota) = self.opaque_quota()
            && self.store.opaque_bytes().saturating_add(size as u64) > quota
            && self.store.get_raw(event.id())?.is_none()
        {
            self.log_storage("refusing opaque events (their quota is full)");
            bail!("opaque event quota reached");
        }
        let inserted = match &event {
            AnyEvent::Known(e) => {
                self.store
                    .insert_with(e, now_secs(), origin == Origin::Fetched)?
            }
            AnyEvent::Opaque(r) => {
                debug!(id = %r.id, v = r.v, "storing opaque event");
                self.store.insert_raw(r, now_secs())?
            }
        };
        // A body this node dropped (ADR 0020): what was kept is its
        // tombstone, so callers see that, and fetch no blob, hint no
        // provider and relay no body.
        if inserted == Inserted::Dropped {
            debug!(id = %event.id(), "body dropped here: refused, tombstone kept");
            let tombstone = match &event {
                AnyEvent::Known(e) => crate::wire::tombstone(&serde_json::to_value(e)?)?,
                AnyEvent::Opaque(r) => crate::wire::tombstone(&r.to_value())?,
            };
            return Ok(Some((AnyEvent::Opaque(tombstone), false)));
        }
        let new = inserted != Inserted::Duplicate;
        if new
            && let AnyEvent::Known(e) = &event
            && crate::halt::is_halt_kind(e.kind.as_str())
            && self.config.halt.guardians.contains(&e.author)
        {
            self.evaluate_halt();
        }
        if new {
            self.on_stored(&event).await;
        }
        Ok(Some((event, new)))
    }

    /// What a new event changes beyond the store (ADR 0020 §5.1): a
    /// retraction is applied; a `rotate` applies again the retractions of
    /// its new key and of that key's successors, whose posts by the old key
    /// could not be matched until now.
    async fn on_stored(&self, event: &AnyEvent) {
        if let Some(retraction) = crate::retract::Retraction::from_any(event) {
            self.apply_retraction(&retraction).await;
            return;
        }
        if let AnyEvent::Known(label) = event
            && self.is_erasure_label(label)
        {
            self.apply_erasure(label).await;
            return;
        }
        let AnyEvent::Known(rotate) = event else {
            return;
        };
        if rotate.kind != Kind::Rotate {
            return;
        }
        let Some(new_key) = rotate.data.get("new_key").and_then(|k| k.as_str()) else {
            return;
        };
        let retractions = self
            .store
            .rotation_successors(new_key)
            .and_then(|keys| self.store.retractions_by(&keys));
        match retractions {
            Ok(retractions) => {
                for retraction in retractions {
                    self.apply_retraction(&retraction).await;
                }
            }
            Err(e) => warn!(rotate = %rotate.id, "retractions not re-evaluated: {e:#}"),
        }
    }

    /// Whether `event` is an erasure label from a labeler this node
    /// subscribes to: a `react` with `data.label = "erasure"`.
    fn is_erasure_label(&self, event: &Event) -> bool {
        event.kind == Kind::React
            && event.data.get("label").and_then(|l| l.as_str()) == Some("erasure")
            && self.config.erasure_labelers.contains(&event.author)
    }

    /// Apply an erasure label (ADR 0020 §5.2): drop the labelled post's
    /// body (its parent), reason `erasure`, the label as trigger. A post
    /// not held yet is remembered: only its tombstone will be stored. Key
    /// successions and halt events are never dropped.
    async fn apply_erasure(&self, label: &Event) {
        let Some(target) = label.parent.as_deref() else {
            debug!(label = %label.id, "erasure label without a parent: ignored");
            return;
        };
        if let Ok(Some(stored)) = self.store.get(target)
            && crate::retract::unretractable(stored.event.kind.as_str())
        {
            warn!(label = %label.id, %target, "erasure label on a key succession or a halt: ignored");
            return;
        }
        match self
            .drop_body(target, DropReason::Erasure, Some(&label.id), None)
            .await
        {
            Ok(_) => {
                info!(label = %label.id, %target, labeler = %label.author, "erasure label applied")
            }
            Err(e) => warn!(label = %label.id, %target, "erasure label not applied: {e:#}"),
        }
    }

    /// Apply the erasure labels already held from the subscribed labelers
    /// (a labeler added since they arrived).
    async fn apply_held_erasures(&self) {
        for labeler in &self.config.erasure_labelers {
            let filter = crate::store::Filter {
                kinds: vec![Kind::React],
                author: Some(labeler.clone()),
                ..crate::store::Filter::default()
            };
            let Ok(found) = self.store.search(None, &filter, 10_000) else {
                continue;
            };
            for stored in found {
                // Not applied yet, or only an eviction (whose kept
                // replicas the erasure releases, ADR 0029 §5).
                if self.is_erasure_label(&stored.event)
                    && self
                        .store
                        .drop_record(stored.event.parent.as_deref().unwrap_or_default())
                        .ok()
                        .flatten()
                        .is_none_or(|record| record.reason == DropReason::Evicted)
                {
                    self.apply_erasure(&stored.event).await;
                }
            }
        }
    }

    /// Apply a retraction held here: drop the bodies of the referenced
    /// posts by its author (or by a key it rotated from) and untag their
    /// blobs, remember the posts not held yet, ignore the others.
    async fn apply_retraction(&self, retraction: &crate::retract::Retraction) {
        let applied = match self.store.apply_retraction(retraction, now_secs()) {
            Ok(applied) => applied,
            Err(e) => {
                warn!(retraction = %retraction.id, "retraction not applied: {e:#}");
                return;
            }
        };
        for (id, applied) in applied {
            match applied {
                crate::store::Applied::Dropped(outcome) => {
                    self.untag_freed(&outcome.freed_blobs).await;
                    info!(%id, retraction = %retraction.id, body = outcome.body_dropped,
                          blobs = outcome.freed_blobs.len(), "retracted by its author: body dropped");
                }
                crate::store::Applied::Remembered => debug!(
                    %id, retraction = %retraction.id,
                    "retracted post not held: its body is refused if it arrives by its author"
                ),
                crate::store::Applied::NotTheAuthor => debug!(
                    %id, retraction = %retraction.id,
                    "retraction of a post by another author (or a rotation link not held): ignored"
                ),
                crate::store::Applied::Unretractable => debug!(
                    %id, retraction = %retraction.id,
                    "retraction of a rotate or halt event: ignored"
                ),
                crate::store::Applied::AlreadyDropped => {}
            }
        }
    }

    /// Store and broadcast an event that is already signed (any version),
    /// e.g. a repost or an event produced by another tool.
    pub async fn publish_signed(&self, value: serde_json::Value) -> Result<AnyEvent> {
        let halt_kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(crate::halt::is_halt_kind);
        if !halt_kind
            && value.get("net").and_then(serde_json::Value::as_str) == Some(PUBLIC_NET)
            && let Some(halt) = self.halt_in_force()
        {
            bail!(
                "the public network is halted by guardians: {} (halt {})",
                halt.reason,
                halt.id
            );
        }
        let Some((event, _)) = self.accept_value(value.clone(), Origin::Pushed).await? else {
            bail!("rejected by node policy");
        };
        // A node never pushes a body it dropped (ADR 0020 §3).
        if let AnyEvent::Opaque(raw) = &event
            && raw.tombstoned
            && value.get(crate::wire::BODY_HASH_KEY).is_none()
        {
            bail!("the body of {} was dropped here: not published", raw.id);
        }
        let Some(sender) = self.sender(event.net()) else {
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
            info: self.our_info(),
        };
        let mut info = match proto::call_solving(self.endpoint(), peer, &request).await? {
            Response::Hello { info } => info,
            other => bail!("unexpected hello response: {other:?}"),
        };
        self.challenger.met(peer);
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

    /// Load challenges issued and solutions accepted by this node so far.
    pub fn challenge_stats(&self) -> (u64, u64) {
        self.challenger.stats()
    }

    /// Serve a `/wats/1` request from `peer`, with the solution to a load
    /// challenge it may carry.
    pub(crate) async fn handle_request(
        &self,
        peer: EndpointId,
        request: Request,
        solution: Option<crate::challenge::Solution>,
    ) -> Response {
        self.seen(peer);
        if let Request::Sync { net, .. } | Request::Get { net, .. } = &request
            && self.joined(net)
        {
            self.sighted(net, peer);
        }
        let kind = match &request {
            Request::Hello { .. } => Some(crate::limits::Kind::Hello),
            Request::Sync { .. } => Some(crate::limits::Kind::Sync),
            Request::Get { include, .. } if include == "thread" => {
                Some(crate::limits::Kind::GetThread)
            }
            Request::Get { .. } => Some(crate::limits::Kind::Get),
            Request::Replicate { .. } => Some(crate::limits::Kind::Replicate),
            Request::CrashReport { .. } => Some(crate::limits::Kind::Report),
            Request::Unsupported => None,
        };
        let _in_flight = InFlight::enter(&self.in_flight);
        if let Some(kind) = kind
            && let Some(response) = self.admit(peer, kind, &request, solution)
        {
            return response;
        }
        match request {
            Request::Hello { mut info } => {
                crate::grease::strip(&mut info.features);
                if let Ok(mut infos) = self.peer_info.lock() {
                    infos.insert(peer, info);
                }
                self.challenger.met(peer);
                Response::Hello {
                    info: self.our_info(),
                }
            }
            Request::Unsupported => Response::Unsupported,
            Request::Sync {
                net,
                after_ts,
                after_id,
                before_ts,
                before_id,
                kinds,
                authors,
                partial,
                limit,
            } => {
                if !self.joined(&net) {
                    return Response::Refused {
                        reason: format!("not a member of network {net:?}"),
                    };
                }
                if self.config.history.legacy {
                    return self.serve_sync_legacy(&net, after_ts, &after_id, before_ts, limit);
                }
                let before = before_ts.map(|ts| (ts, before_id.unwrap_or_default()));
                self.serve_sync(
                    &net,
                    history::SyncAsk {
                        after: (after_ts, after_id),
                        before,
                        kinds,
                        authors,
                        partial,
                        limit,
                    },
                )
            }
            Request::Get { net, ids, include } => {
                if self.config.history.legacy {
                    return Response::Unsupported;
                }
                if !self.joined(&net) {
                    return Response::Refused {
                        reason: format!("not a member of network {net:?}"),
                    };
                }
                self.serve_get(&net, &ids, &include)
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
                self.durability.note_served();
                self.serve_replicate(peer, event, hashes).await
            }
        }
    }

    /// Serve `replicate` (ADR 0005, ADR 0029): accept the event, then
    /// hold each blob asked for that it lists. A blob already held under a
    /// tag is confirmed at once (no fetch slot, whatever the storage
    /// state); another is fetched from the requester. A replica is recorded
    /// when this node is among the 2k closest of its view. A refusal for
    /// good (storage full, quota, replica share) is said as such when no
    /// blob was taken, so repair moves on.
    async fn serve_replicate(
        &self,
        peer: EndpointId,
        event: serde_json::Value,
        hashes: Vec<String>,
    ) -> Response {
        let tuning = &self.config.durability;
        if tuning.lie_replicate {
            return Response::Replicated { hashes };
        }
        // A body evicted here (ADR 0029 S6): its blobs held under a tag
        // are confirmed without taking the body back.
        if let Some(id) = event.get("id").and_then(serde_json::Value::as_str)
            && matches!(
                self.store.drop_record(id),
                Ok(Some(record)) if record.reason == DropReason::Evicted
            )
        {
            let mut stored = Vec::new();
            let blobs = event
                .get("blobs")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for blob in blobs {
                let Ok(blob) = serde_json::from_value::<BlobRef>(blob) else {
                    continue;
                };
                let Ok(hash) = blob.hash.parse::<Hash>() else {
                    continue;
                };
                if hashes.contains(&blob.hash)
                    && self.blob_tagged(&hash).await
                    && self.holds_blob(&blob).await
                {
                    stored.push(blob.hash.clone());
                }
            }
            return Response::Replicated { hashes: stored };
        }
        let event = match self.accept_value(event, Origin::Pushed).await {
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
        let legacy = tuning.legacy_replicate;
        let mut stored = Vec::new();
        let mut refusal: Option<&str> = None;
        // One budget for every blob of the request, and the pusher asked
        // past its backoff only until it fails one blob: a pusher that
        // serves nothing holds the request, and its fetch slots, for one
        // budget at most, and the rest goes the plain way (skipped while
        // it backs off). One WARN per request for what was not fetched.
        let started = tokio::time::Instant::now();
        let budget = tuning.replicate_budget;
        let mut pusher_failed = false;
        let mut not_fetched: Vec<&str> = Vec::new();
        for blob in event.blobs.iter().filter(|b| hashes.contains(&b.hash)) {
            if blob.url.is_some() || blob.size > self.config.max_blob_bytes {
                continue;
            }
            self.add_hints(&blob.hash, &[peer]);
            let Ok(hash) = blob.hash.parse::<Hash>() else {
                continue;
            };
            let replica = !legacy && self.takes_as_replica(&hash, &event.net);
            // Already held under a tag: confirmed at once. The tag existed
            // before this request, so every blob collection keeps the blob:
            // one past its mark read it then, a later one reads it now (a
            // blob held but untagged takes the fetch path below).
            if !legacy && self.blob_tagged(&hash).await && self.holds_blob(blob).await {
                if let Err(e) = self.pin(hash).await {
                    warn!(hash = %blob.hash, "blob not pinned: {e:#}");
                    continue;
                }
                if replica {
                    self.record_replica_intent(hash, blob.size).await;
                }
                stored.push(blob.hash.clone());
                continue;
            }
            if !legacy {
                if self.storage_state() != StorageState::Ok {
                    refusal = Some("storage full: not taking replicas");
                    continue;
                }
                if let Some(quota) = self.config.blob_quota
                    && self.blob_usage().await.saturating_add(blob.size) > quota
                {
                    refusal = Some("blob quota reached: not taking replicas");
                    continue;
                }
                if replica && !self.replica_fits(blob.size) {
                    refusal = Some("replica share of storage.max reached: not taking replicas");
                    continue;
                }
            }
            let left = budget.saturating_sub(started.elapsed());
            if !legacy && left.is_zero() {
                debug!(hash = %blob.hash, pusher = %peer.fmt_short(), "replicate: budget spent, blob not fetched");
                not_fetched.push(&blob.hash);
                continue;
            }
            // At most a few fetches at once for one peer, a few more
            // waiting their turn.
            let wait = if legacy {
                crate::limits::BLOB_FETCH_WAIT
            } else {
                crate::limits::BLOB_FETCH_WAIT.min(left)
            };
            let Some(_permit) = self.limiter.blob_fetch(peer, wait).await else {
                continue;
            };
            // 0.4.2 (tests: `legacy_replicate`) asked the pusher once,
            // and not while it backed off.
            let fetched = if legacy {
                self.fetch_blob(blob, vec![peer]).await
            } else {
                let left = budget.saturating_sub(started.elapsed());
                let fetch = async {
                    if pusher_failed {
                        self.fetch_blob(blob, vec![peer]).await
                    } else {
                        self.fetch_pushed(blob, peer, left).await
                    }
                };
                let fetched = tokio::time::timeout(left, fetch).await.unwrap_or(false);
                if !fetched {
                    pusher_failed = true;
                    not_fetched.push(&blob.hash);
                }
                fetched
            };
            if fetched {
                if replica {
                    self.record_replica_intent(hash, blob.size).await;
                }
                stored.push(blob.hash.clone());
            }
        }
        if let Some(first) = not_fetched.first() {
            warn!(
                pusher = %peer.fmt_short(),
                count = not_fetched.len(),
                first = %first,
                "pushed blobs not fetched from their pusher: replicas not taken"
            );
        }
        // A retraction or an erasure that landed while the blobs came in
        // (S8): what it would have freed is released again.
        if !stored.is_empty() && !legacy && !self.event_still_full(&event.id) {
            for hex in &stored {
                // Only what nothing keeps: no kept event lists it, no
                // replica kept through an eviction holds it (N13).
                if let Ok(hash) = hex.parse::<Hash>()
                    && !self.store.blob_kept(hex).unwrap_or(true)
                    && let Err(e) = self.untag_blob(hash).await
                {
                    warn!(hash = %hex, "blob of a dropped body not untagged: {e:#}");
                }
            }
            return Response::Replicated { hashes: vec![] };
        }
        match refusal {
            Some(reason) if stored.is_empty() => Response::Refused {
                reason: reason.to_string(),
            },
            _ => Response::Replicated { hashes: stored },
        }
    }

    /// Whether event `id` is held in full with no drop record.
    fn event_still_full(&self, id: &str) -> bool {
        matches!(self.store.get(id), Ok(Some(_))) && matches!(self.store.drop_record(id), Ok(None))
    }

    /// Rate limits and load challenge: `None` lets the request through,
    /// otherwise the response to send instead.
    fn admit(
        &self,
        peer: EndpointId,
        kind: crate::limits::Kind,
        request: &Request,
        solution: Option<crate::challenge::Solution>,
    ) -> Option<Response> {
        let limits = self.limiter.limits();
        let in_flight = self.in_flight.load(std::sync::atomic::Ordering::Relaxed);
        let overloaded = in_flight > limits.overload_in_flight;
        // Only peers that said they can solve one are challenged; a hello
        // says it itself.
        let solves = limits.challenge
            && match request {
                Request::Hello { info } => {
                    info.features.iter().any(|f| f == crate::challenge::FEATURE)
                }
                _ => self
                    .peer_info(&peer)
                    .is_some_and(|i| i.features.iter().any(|f| f == crate::challenge::FEATURE)),
            };
        // A valid solution passes the per-peer limit and the overload, but
        // not the global ceiling on solved requests.
        if solves && let Some(solution) = solution {
            match self
                .challenger
                .verify(&solution, &peer, &self.endpoint_id(), now_secs())
            {
                Ok(()) if self.limiter.allow_solved() => {
                    self.challenger.note(overloaded);
                    return None;
                }
                Ok(()) => debug!(peer = %peer.fmt_short(), "solved requests over the ceiling"),
                Err(why) => debug!(peer = %peer.fmt_short(), ?why, "challenge solution rejected"),
            }
        }
        let limited = !self.limiter.allow(peer, kind);
        let effort = self.challenger.note(overloaded || (limited && solves));
        if solves && (limited || (overloaded && !self.challenger.reputable(&peer))) {
            let issued = self.challenger.issue(&peer, effort, now_secs());
            return Some(Response::Challenge {
                alg: issued.alg,
                seed: issued.seed,
                effort: issued.effort,
                expires: issued.expires,
            });
        }
        if limited {
            return Some(Response::Refused {
                reason: format!("rate limited ({}): retry later", kind.as_str()),
            });
        }
        None
    }

    /// Serve `sync` as 0.4.0 did (tests: `history.legacy`): ascending
    /// only, the halt kinds only while serving a halt, no `gap`, no `seen`;
    /// a descending request is malformed there.
    fn serve_sync_legacy(
        &self,
        net: &str,
        after_ts: u64,
        after_id: &str,
        before_ts: Option<u64>,
        limit: u32,
    ) -> Response {
        if before_ts.is_some() {
            return Response::Refused {
                reason: "malformed request: missing field `after_ts`".into(),
            };
        }
        let limit = limit.clamp(1, proto::MAX_SYNC_PAGE);
        let page = if self.halt_serving.load(std::sync::atomic::Ordering::Relaxed) {
            self.store
                .halt_page_after(net, after_ts, after_id, limit as usize)
        } else {
            self.store
                .page_after(net, after_ts, after_id, limit as usize)
        };
        match page {
            Ok(page) => Response::SyncPage {
                more: page.len() == limit as usize,
                events: page,
                gap: None,
                seen: Vec::new(),
            },
            Err(e) => Response::Refused {
                reason: format!("{e:#}"),
            },
        }
    }

    /// Catch up on a network from a peer, ascending: page through its
    /// events after our sync watermark (ADR 0005). The cheap check of each
    /// new neighbor once no history range is pending (ADR 0025 §5), and
    /// what a 0.4.0 node does on every new neighbor. Returns how many new
    /// events were stored.
    pub async fn sync_from(&self, peer: EndpointId, net: &str) -> Result<usize> {
        let watermark = self.store.sync_watermark(net)?;
        self.sync_from_with(peer, net, watermark, None).await
    }

    /// [`Node::sync_from`] from `from` (a watermark), stopping once the
    /// store holds `stop_at` bytes. The watermark moves only when the peer
    /// had nothing more and no history range is pending.
    pub(crate) async fn sync_from_with(
        &self,
        peer: EndpointId,
        net: &str,
        from: u64,
        stop_at: Option<u64>,
    ) -> Result<usize> {
        let started = now_secs();
        let (mut after_ts, mut after_id) =
            (from.saturating_sub(MAX_CLOCK_SKEW_SECS), String::new());
        let mut new: usize = 0;
        let mut ended = false;
        for _ in 0..10_000 {
            if stop_at.is_some_and(|stop| self.store.store_bytes() >= stop) {
                break;
            }
            // What the peer holds, a range still pending included: a check,
            // not a claim of complete history.
            let request =
                Request::sync_after(net, after_ts, &after_id, proto::MAX_SYNC_PAGE).partial();
            let (events, more, seen) =
                match proto::call_solving(self.endpoint(), peer, &request).await? {
                    Response::SyncPage {
                        events, more, seen, ..
                    } => (events, more, seen),
                    other => bail!("unexpected sync response: {other:?}"),
                };
            let seen = (seen.len() == events.len()).then_some(seen);
            let mut reports: Vec<(String, u64)> = Vec::new();
            let mut last = None;
            for (i, value) in events.into_iter().enumerate() {
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
                match self.accept_value(value, Origin::Pulled).await {
                    Ok(Some((event, stored))) => {
                        if stored {
                            new = new.saturating_add(1);
                        }
                        // Only for what this node holds, by its verified
                        // author.
                        if let Some(at) = seen.as_ref().and_then(|s| s.get(i)) {
                            reports.push((event.author().to_string(), *at));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => debug!(%id, "sync: rejected event: {e:#}"),
                }
                (after_ts, after_id) = (ts, id.clone());
                last = Some(id);
            }
            self.record_seen(peer, &reports);
            if last.is_none() || !more {
                ended = true;
                break;
            }
        }
        // The watermark moves only once no history range is pending (ADR
        // 0025 §5), and when the peer had nothing more.
        let pending = self
            .store
            .sync_ranges(net)?
            .iter()
            .any(|r| r.state == crate::store::RangeState::Pending);
        if ended && !pending {
            self.store.set_sync_watermark(net, started)?;
        }
        Ok(new)
    }

    /// Download a blob from providers. BLAKE3 verified streaming makes the
    /// content match the hash; the content itself stays untrusted data and
    /// is never executed (SPEC §2.15).
    async fn fetch_blob(&self, blob: &BlobRef, providers: Vec<EndpointId>) -> bool {
        self.fetch_blob_from(blob, providers, false).await
    }

    /// Fetch a blob `pusher` asked this node to hold (`replicate`), from
    /// that peer. It just reached this node, so its backoff as a provider
    /// (B7, for providers others name) does not skip it: one failed fetch
    /// from it (a connection that timed out under load, a refusal while a
    /// possession proof of that blob runs there) would otherwise drop every
    /// replica it pushes for the next 30 s, doubling up to an hour, without
    /// telling it. A failed try is tried again after a pause, as a read is,
    /// within [`PUSH_FETCH_BUDGET`] and `budget` for all tries. Logs at
    /// debug only: the request logs what it did not fetch, once.
    async fn fetch_pushed(&self, blob: &BlobRef, pusher: EndpointId, budget: Duration) -> bool {
        let started = tokio::time::Instant::now();
        let budget = budget.min(PUSH_FETCH_BUDGET);
        for attempt in 0..PUSH_FETCH_TRIES {
            if attempt > 0 {
                tokio::time::sleep(READ_FETCH_PAUSE.saturating_mul(attempt)).await;
            }
            let left = budget.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, self.fetch_blob_from(blob, vec![pusher], true)).await {
                Ok(true) => return true,
                Ok(false) => {
                    debug!(hash = %blob.hash, pusher = %pusher.fmt_short(), attempt, "pushed blob not fetched yet");
                }
                Err(_) => break,
            }
        }
        debug!(hash = %blob.hash, pusher = %pusher.fmt_short(), "pushed blob not fetched from its pusher");
        false
    }

    /// [`Node::fetch_blob`]; with `again`, the providers backing off are
    /// asked too (a read retrying the provider its first try marked).
    async fn fetch_blob_from(
        &self,
        blob: &BlobRef,
        providers: Vec<EndpointId>,
        again: bool,
    ) -> bool {
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
            .filter(|p| again || !self.provider_backing_off(p))
            .take(MAX_PROVIDER_ATTEMPTS)
            .collect();
        if candidates.is_empty() {
            debug!(hash = %blob.hash, "no provider to fetch from (all backing off)");
            return false;
        }
        // Kept from the collection from before the first byte until pinned:
        // a partial blob is not, by itself.
        let _hold = self.hold_blob(hash).await;
        // Recorded but unreadable: fetched again into a scratch file (the
        // downloader would trust the entry).
        if self.blob_unreadable(hash).await {
            if !self
                .repair_blob(hash, Some(blob.size), candidates.clone())
                .await
            {
                return false;
            }
            if let Err(e) = self.pin(hash).await {
                warn!(hash = %blob.hash, "blob not pinned: {e:#}");
                return false;
            }
            let _ = self.fetched.send(blob.clone());
            return true;
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
            // Direct addresses only where a path to the peer is in use: one
            // it was never reached on (an ephemeral NAT port, a bridge
            // address) times out at every start (0.4.12). Relays always.
            // Nothing left (no path in use, no relay: loopback): all of them.
            let all: Vec<_> = info.into_addrs().collect();
            let kept: Vec<TransportAddr> = all
                .iter()
                .filter(|a| {
                    !matches!(a.addr(), TransportAddr::Ip(_))
                        || matches!(a.usage(), iroh::endpoint::TransportAddrUsage::Active)
                })
                .map(|a| a.addr().clone())
                .collect();
            let addrs = match kept.is_empty() {
                true => all.iter().map(|a| a.addr().clone()).collect(),
                false => kept,
            };
            let addr = EndpointAddr::from_parts(id, addrs);
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

    /// Broadcast on gossip events that anti-entropy pulled (each with the
    /// peer that served it, as the provider of its blobs): gossip had not
    /// delivered them, so the rest of the path gets them live, not a hop
    /// per round.
    pub(crate) async fn relay_pulled(&self, net: &str, pulled: &[(serde_json::Value, EndpointId)]) {
        let Some(sender) = self.sender(net) else {
            return;
        };
        for (value, from) in pulled {
            let envelope = Envelope::new(value.clone(), *from);
            if let Ok(bytes) = serde_json::to_vec(&envelope)
                && let Err(e) = sender.broadcast(bytes.into()).await
            {
                debug!("relay of a pulled event: {e:#}");
            }
        }
    }

    /// Give gossip a fresh connection to `peer`. iroh-gossip 0.101 keeps a
    /// neighbor whose send loop died on a live connection as active, and
    /// drops every message to it (docs/upstream); the new connection
    /// becomes the active one on both sides, as after a reconnect.
    pub(crate) async fn reset_gossip_link(&self, peer: EndpointId) -> Result<()> {
        let conn = tokio::time::timeout(
            Duration::from_secs(10),
            self.endpoint().connect(peer, iroh_gossip::ALPN),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))??;
        self.gossip.handle_connection(conn).await?;
        Ok(())
    }

    /// The gossip sender of a network this node is on.
    fn sender(&self, net: &str) -> Option<GossipSender> {
        self.senders.lock().ok()?.get(net).cloned()
    }

    /// Whether this node is on `net` (joined, and not left on a halt).
    pub fn joined(&self, net: &str) -> bool {
        self.senders.lock().is_ok_and(|s| s.contains_key(net))
    }

    /// The networks this node is on, in configuration order.
    pub fn joined_nets(&self) -> Vec<String> {
        self.config
            .nets
            .iter()
            .filter(|n| self.joined(n))
            .cloned()
            .collect()
    }

    /// Leave a network: its gossip topic, its sync, and so its events and
    /// the agent wakes they would cause (a halt, ADR 0016 §4).
    pub fn leave_net(&self, net: &str) {
        let sender = self.senders.lock().ok().and_then(|mut s| s.remove(net));
        if let Some(task) = self
            .receive_loops
            .lock()
            .ok()
            .and_then(|mut l| l.remove(net))
        {
            task.abort();
        }
        if let Ok(mut n) = self.neighbors.lock() {
            n.remove(net);
        }
        if let Ok(mut swarm) = self.swarm.lock() {
            swarm.forget(net);
        }
        if sender.is_some()
            && let Ok(mut left) = self.left_nets.lock()
        {
            left.push(net.to_string());
        }
    }

    /// The public network's halt state as last evaluated.
    pub fn halt_view(&self) -> crate::halt::HaltView {
        self.halt_view.borrow().clone()
    }

    /// The halt this node obeys now: in effect, not overridden, and
    /// `halt.accept`.
    pub fn halt_in_force(&self) -> Option<crate::halt::Decision> {
        if !self.config.halt.accept {
            return None;
        }
        self.halt_view.borrow().enforced().cloned()
    }

    /// What this node did about a halt; changes are notified.
    pub fn halt_outcome(&self) -> tokio::sync::watch::Receiver<HaltOutcome> {
        self.halt_outcome.subscribe()
    }

    /// How many peers confirmed this node's push of the halt it relayed.
    pub fn halt_pushed(&self) -> usize {
        self.halt_pushed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Recompute the halt state from the store (ADR 0016 §2), after a halt
    /// event was stored and at startup.
    pub fn evaluate_halt(&self) -> crate::halt::HaltView {
        let guardians = &self.config.halt.guardians;
        let view = if guardians.enabled() {
            match self.store.halt_events(PUBLIC_NET, &guardians.keys) {
                Ok(events) => {
                    let in_effect = crate::halt::evaluate(&events, guardians)
                        .in_effect()
                        .cloned();
                    let overridden = in_effect
                        .as_ref()
                        .is_some_and(|d| crate::halt::is_overridden(&self.config.data_dir, &d.id));
                    crate::halt::HaltView {
                        in_effect,
                        overridden,
                    }
                }
                Err(e) => {
                    warn!("halt state not evaluated: {e:#}");
                    return self.halt_view();
                }
            }
        } else {
            crate::halt::HaltView::default()
        };
        if self.config.halt.serve && self.config.halt.accept {
            self.halt_serving.store(
                view.enforced().is_some(),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        self.halt_view.send_if_modified(|current| {
            let changed = *current != view;
            if changed {
                *current = view.clone();
            }
            changed
        });
        view
    }

    /// Act on halts from now on (ADR 0016 §3): when one is in force, relay
    /// it, write the marker and report [`HaltOutcome::Exit`] (or leave
    /// `public`, or serve halt events only), through
    /// [`Node::halt_outcome`]. The daemon calls it once its MCP sessions
    /// and agent run; a node started only to sync (the startup check) or to
    /// publish does not.
    pub fn enforce_halts(self: &Arc<Self>) {
        let mut views = self.halt_view.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut acted: Option<String> = None;
            let mut logged: Option<String> = None;
            loop {
                let view = views.borrow_and_update().clone();
                let Some(node) = weak.upgrade() else { break };
                match (&view.in_effect, view.enforced()) {
                    (_, Some(halt)) if node.config.halt.accept => {
                        if acted.as_deref() != Some(halt.id.as_str()) {
                            acted = Some(halt.id.clone());
                            node.act_on_halt(halt).await;
                        }
                    }
                    (Some(halt), _) if logged.as_deref() != Some(halt.id.as_str()) => {
                        logged = Some(halt.id.clone());
                        let why = if view.overridden {
                            "overridden here with `wats resume`"
                        } else {
                            "ignored (halt.accept = false)"
                        };
                        warn!(halt = %halt.id, reason = %halt.reason, "public network halted by guardians, {why}");
                    }
                    (None, _) => {
                        acted = None;
                        if *node.halt_outcome.borrow() == HaltOutcome::Serving {
                            info!(
                                "the public network is no longer halted: serving everything again"
                            );
                            node.halt_outcome.send_replace(HaltOutcome::Running);
                        }
                    }
                    _ => {}
                }
                drop(node);
                if views.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    async fn act_on_halt(&self, halt: &crate::halt::Decision) {
        if self.config.halt.serve {
            warn!(halt = %halt.id, reason = %halt.reason,
                "public network halted by guardians: serving halt events only (halt.serve)");
            self.halt_outcome.send_replace(HaltOutcome::Serving);
            return;
        }
        warn!(halt = %halt.id, reason = %halt.reason,
            "public network halted by guardians: relaying the halt before stopping");
        let pushed = self.relay_halt(halt).await;
        info!(halt = %halt.id, peers = pushed, "halt relayed");
        let marker = crate::halt::Marker::new(halt, now_secs());
        if let Err(e) = crate::halt::write_marker(&self.config.data_dir, &marker) {
            warn!("halt marker not written: {e:#}");
        }
        if self.config.nets.iter().any(|n| n != PUBLIC_NET) {
            self.leave_net(PUBLIC_NET);
            warn!(
                nets = ?self.joined_nets(),
                "left the public network on the guardians' halt; the other networks keep running. {}",
                marker.message()
            );
            self.halt_outcome.send_replace(HaltOutcome::LeftPublic);
        } else {
            self.halt_outcome
                .send_replace(HaltOutcome::Exit(halt.clone()));
        }
    }

    /// Re-broadcast a halt and its approvals on gossip and push them to the
    /// known peers, waiting up to [`crate::halt::RELAY_WAIT`]. Returns how
    /// many peers took them all.
    pub async fn relay_halt(&self, halt: &crate::halt::Decision) -> usize {
        let guardians = &self.config.halt.guardians;
        let events = self
            .store
            .halt_events(PUBLIC_NET, &guardians.keys)
            .unwrap_or_default();
        let values: Vec<serde_json::Value> = crate::halt::relay_set(&events, halt, guardians)
            .into_iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect();
        let pushed = self.push_events(&values, crate::halt::RELAY_WAIT).await;
        self.halt_pushed
            .store(pushed, std::sync::atomic::Ordering::Relaxed);
        pushed
    }

    /// Broadcast events this node holds on gossip and push them to every
    /// known peer (a `replicate` with no blobs), for up to `wait`. Returns
    /// how many peers took them all.
    pub async fn push_events(&self, values: &[serde_json::Value], wait: Duration) -> usize {
        for value in values {
            let net = value.get("net").and_then(serde_json::Value::as_str);
            if let Some(sender) = net.and_then(|n| self.sender(n)) {
                let envelope = Envelope::new(value.clone(), self.endpoint_id());
                if let Ok(bytes) = serde_json::to_vec(&envelope)
                    && let Err(e) = sender.broadcast(bytes.into()).await
                {
                    debug!("halt broadcast: {e:#}");
                }
            }
        }
        let pushes = self.known_peers().into_iter().map(|peer| async move {
            for value in values {
                let request = Request::Replicate {
                    event: value.clone(),
                    hashes: Vec::new(),
                };
                match proto::call_solving(self.endpoint(), peer, &request).await {
                    Ok(Response::Replicated { .. }) => {}
                    other => {
                        debug!(peer = %peer.fmt_short(), "push refused: {other:?}");
                        return false;
                    }
                }
            }
            true
        });
        let mut pending: n0_future::FuturesUnordered<_> = pushes.collect();
        let mut ok: usize = 0;
        let _ = tokio::time::timeout(wait, async {
            while let Some(done) = pending.next().await {
                if done {
                    ok = ok.saturating_add(1);
                }
            }
        })
        .await;
        ok
    }

    /// At startup with the halt marker present (ADR 0016 §3): sync from
    /// peers for up to `wait`, storing only, then decide. Returns `true`
    /// when the halt still holds (the marker is kept, updated to the halt
    /// in force), `false` when it was lifted (the marker is removed).
    pub async fn check_halt_marker(&self, wait: Duration) -> bool {
        let dir = &self.config.data_dir;
        let Some(marker) = crate::halt::read_marker(dir) else {
            return false;
        };
        if !self.config.halt.guardians.enabled() {
            info!("no guardian set in this release: the halt marker is cleared");
            let _ = crate::halt::remove_marker(dir);
            return false;
        }
        info!(halt = %marker.halt, "halted by guardians: checking for an unhalt for up to {} s", wait.as_secs());
        let started = tokio::time::Instant::now();
        let lifted = |node: &Self| {
            node.evaluate_halt().enforced().is_none()
                && node
                    .store
                    .halt_events(PUBLIC_NET, &node.config.halt.guardians.keys)
                    .is_ok_and(|events| {
                        crate::halt::evaluate(&events, &node.config.halt.guardians)
                            .latest
                            .is_some_and(|d| !d.halt && d.ts >= marker.since)
                    })
        };
        while started.elapsed() < wait && !lifted(self) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match self.evaluate_halt().enforced() {
            Some(halt) => {
                if halt.id != marker.halt {
                    let _ =
                        crate::halt::write_marker(dir, &crate::halt::Marker::new(halt, now_secs()));
                }
                true
            }
            None => {
                info!(halt = %marker.halt, "the halt is lifted: the marker is removed");
                if let Err(e) = crate::halt::remove_marker(dir) {
                    warn!("halt marker not removed: {e:#}");
                }
                false
            }
        }
    }

    /// The halt part of `status` (ADR 0016 §5).
    pub fn halt_status(&self) -> serde_json::Value {
        let halt = &self.config.halt;
        let mut status = serde_json::Map::new();
        status.insert("accept".into(), halt.accept.into());
        if halt.serve {
            status.insert("serve".into(), true.into());
        }
        let view = self.halt_view();
        if let Some(decision) = &view.in_effect {
            status.insert("in_effect".into(), decision.to_json());
            if view.overridden {
                status.insert("overridden".into(), true.into());
            } else if !halt.accept {
                status.insert("ignored".into(), true.into());
            }
        }
        if let Ok(left) = self.left_nets.lock()
            && !left.is_empty()
        {
            status.insert("left".into(), serde_json::json!(*left));
        }
        if let Some(marker) = crate::halt::read_marker(&self.config.data_dir) {
            status.insert("marker".into(), marker.to_json());
        }
        serde_json::Value::Object(status)
    }

    /// Why MCP tools refuse to work, when a halt stops this node: a halt in
    /// force on a node of the public network alone, or serving halts.
    pub fn halt_mcp_message(&self) -> Option<String> {
        let halt = self.halt_in_force()?;
        let stops = self.config.halt.serve || self.config.nets.iter().all(|n| n == PUBLIC_NET);
        stops.then(|| crate::halt::mcp_message(&crate::halt::Marker::new(&halt, now_secs())))
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.save_peer_cache().await;
        self.save_swarm().await;
        // The receive loops first: gossip's shutdown ends their streams,
        // which they would take for a gossip failure.
        if let Ok(mut loops) = self.receive_loops.lock() {
            for (_, task) in loops.drain() {
                task.abort();
            }
        }
        self.gossip.shutdown().await.ok();
        self.router.shutdown().await?;
        // The router shuts the blob protocol down, which also closes the store.
        self.blobs.shutdown().await.ok();
        // Background tasks may still hold the node; release the data
        // directory explicitly, after its registry entry (the next holder
        // writes its own).
        if let Some(lock) = self.lock.lock().ok().and_then(|mut l| l.take()) {
            if let Some(registry) = &self.config.registry {
                crate::background::unregister(registry, &self.config.data_dir);
            }
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
    /// `who`'s `this_machine` (ADR 0044): each node's agents file, with
    /// the node's name; a missing or broken file is skipped.
    #[test]
    fn machine_agents_reads_each_nodes_file() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("wats");
        std::fs::create_dir_all(&data).unwrap();
        crate::naming::write(&data, "bubbly_gnat").unwrap();
        let machine = crate::naming::Machine {
            registry: None,
            default_dir: Some(data.clone()),
        };
        assert!(super::machine_agents(&machine).is_empty());
        std::fs::write(data.join(super::AGENTS_FILE), "{not json").unwrap();
        assert!(super::machine_agents(&machine).is_empty());
        std::fs::write(
            data.join(super::AGENTS_FILE),
            r#"{"agents":[{"name":"vibe","profile":"alice-vibe","author":"f543","runtime":"vibe"}]}"#,
        )
        .unwrap();
        let found = super::machine_agents(&machine);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "bubbly_gnat");
        assert_eq!(found[0].2["author"], "f543");
    }

    #[test]
    fn numbered_names_keep_their_extension() {
        assert_eq!(super::numbered("chart.png", 1), "chart-1.png");
        assert_eq!(super::numbered("a.tar.gz", 2), "a.tar-2.gz");
        assert_eq!(super::numbered("notes", 3), "notes-3");
    }

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

/// The blob collection (`storage.gc`): what it deletes and what it keeps.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod blob_gc_tests {
    use super::*;
    use crate::event::Kind;

    /// Above the 16 KiB the blob store inlines in its database: files.
    const SIZE: usize = 40 * 1024;

    /// The longest a test here may run; each takes a few seconds at most.
    const TEST_LIMIT: Duration = Duration::from_secs(60);

    /// Run `test` on a runtime of its own, failing it after [`TEST_LIMIT`]
    /// instead of hanging CI. The runtime is not waited for afterwards:
    /// a task stuck in the blob store would hang the test again. What the
    /// test itself logs is shown when it fails, to tell where it stopped.
    fn run(test: impl Future<Output = ()> + Send + 'static) {
        use tracing::instrument::WithSubscriber;
        let logs = tracing_subscriber::fmt()
            .with_env_filter("wats=debug")
            .with_ansi(false)
            .with_test_writer()
            .finish();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let test = test.with_subscriber(logs);
        let outcome =
            runtime.block_on(async { tokio::time::timeout(TEST_LIMIT, tokio::spawn(test)).await });
        runtime.shutdown_background();
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => match e.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(e) => panic!("test task ended: {e}"),
            },
            Err(_) => panic!("timed out after {TEST_LIMIT:?}: the node or its blob store hung"),
        }
    }

    async fn start(dir: &Path, gc: bool) -> Arc<Node> {
        let mut config = NodeConfig::new(dir);
        config.mode = NetworkMode::Loopback;
        config.infra = crate::infra::Infra::NONE;
        config.pow_bits = Some(8);
        config.min_pow_bits = Some(8);
        config.blob_gc = gc;
        config.blob_gc_interval = Duration::from_millis(10);
        Node::start(config).await.unwrap()
    }

    /// Wait until a whole collection ran after this call.
    async fn collected(node: &Node) {
        let from = node.blob_collections();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while node.blob_collections() < from + 2 {
            assert!(tokio::time::Instant::now() < deadline, "no collection ran");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn content(seed: u8) -> Vec<u8> {
        (0..SIZE).map(|i| (i % 251) as u8 ^ seed).collect()
    }

    /// A post with one file of `content(seed)`: a new post each time.
    async fn publish(node: &Arc<Node>, dir: &Path, seed: u8) -> Event {
        static POSTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = POSTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("file-{seed}.bin"));
        std::fs::write(&path, content(seed)).unwrap();
        node.publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some(format!("post {n}")),
                text: format!("with a file, {seed}"),
                ..Draft::default()
            },
            &[Attachment { path, name: None }],
        )
        .await
        .unwrap()
    }

    fn hash_of(event: &Event) -> Hash {
        event.blobs[0].hash.parse().unwrap()
    }

    /// The blob is complete in the store and its file is on disk.
    async fn present(node: &Node, hash: Hash) -> bool {
        let file = node
            .config
            .data_dir
            .join("blobs/data")
            .join(format!("{}.data", hash.to_hex()));
        let complete = matches!(
            node.blobs.status(hash).await.unwrap(),
            BlobStatus::Complete { .. }
        );
        assert_eq!(complete, file.exists(), "store and disk disagree");
        complete
    }

    /// A blob in the store under no tag, kept by nothing.
    async fn loose(node: &Node, seed: u8) -> Hash {
        let tag = node
            .blobs
            .add_bytes(content(seed))
            .temp_tag()
            .await
            .unwrap();
        tag.hash()
    }

    #[test]
    fn a_dropped_body_s_blob_is_deleted() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), true).await;
            let event = publish(&node, dir.path(), 1).await;
            let hash = hash_of(&event);
            collected(&node).await;
            assert!(present(&node, hash).await, "kept while the post is");
            let outcome = node
                .drop_body(&event.id, DropReason::Own, None, None)
                .await
                .unwrap();
            assert_eq!(outcome.freed_blobs, [hash.to_hex().to_string()]);
            collected(&node).await;
            assert!(!present(&node, hash).await, "deleted with the body");
            node.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_blob_a_kept_event_lists_survives_untagged() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), true).await;
            let event = publish(&node, dir.path(), 2).await;
            let hash = hash_of(&event);
            // A run that read what to keep before the post was stored would
            // delete the blob once untagged: let every such run end first. A
            // crash leaves no run in flight.
            collected(&node).await;
            // As after a crash between storing a fetched event and its tag.
            unpin_blob(&node.blobs, hash).await.unwrap();
            assert!(!node.blob_pinned(&hash.to_hex()).await.unwrap());
            collected(&node).await;
            assert!(present(&node, hash).await);
            node.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_blob_shared_by_a_dropped_and_a_kept_event_survives() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), true).await;
            let dropped = publish(&node, dir.path(), 3).await;
            let kept = publish(&node, dir.path(), 3).await;
            let hash = hash_of(&dropped);
            assert_eq!(hash, hash_of(&kept));
            let outcome = node
                .drop_body(&dropped.id, DropReason::Own, None, None)
                .await
                .unwrap();
            assert!(outcome.body_dropped && outcome.freed_blobs.is_empty());
            // Even untagged: the kept post lists it (once no run that started
            // before it was stored is left).
            collected(&node).await;
            unpin_blob(&node.blobs, hash).await.unwrap();
            collected(&node).await;
            assert!(present(&node, hash).await);
            node.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_pinned_blob_survives_and_a_loose_one_does_not() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), true).await;
            let pinned = loose(&node, 4).await;
            node.pin(pinned).await.unwrap();
            let unpinned = loose(&node, 5).await;
            assert!(present(&node, unpinned).await);
            collected(&node).await;
            assert!(present(&node, pinned).await);
            assert!(!present(&node, unpinned).await);
            node.shutdown().await.unwrap();
        });
    }

    /// An import in progress (`publish` between the import and the tag)
    /// and a fetch in progress (between its first byte and its tag) are
    /// kept, and collected once nothing keeps them.
    #[test]
    fn blobs_in_flight_survive() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), true).await;
            // What `import` holds until it pins.
            let import = node.blobs.add_bytes(content(6)).temp_tag().await.unwrap();
            let imported = import.hash();
            // What `fetch_blob` holds until it pins.
            let fetched = loose(&node, 7).await;
            let hold = node.hold_blob(fetched).await;
            let twice = node.hold_blob(fetched).await;
            collected(&node).await;
            collected(&node).await;
            assert!(present(&node, imported).await, "import in progress kept");
            assert!(present(&node, fetched).await, "fetch in progress kept");
            drop(import);
            drop(hold);
            collected(&node).await;
            assert!(
                !present(&node, imported).await,
                "import abandoned: collected"
            );
            assert!(present(&node, fetched).await, "still held once");
            drop(twice);
            collected(&node).await;
            assert!(!present(&node, fetched).await, "fetch abandoned: collected");
            node.shutdown().await.unwrap();
        });
    }

    /// The same file published again while a collection deletes its
    /// previous copy (a dropped post's): the blob store can record it with
    /// no file, or remove the file just after the import returned; the
    /// import waits for that, notices and imports it again.
    #[test]
    fn a_file_published_again_during_its_collection_is_readable() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let mut config = NodeConfig::new(dir.path().join("n"));
            config.mode = NetworkMode::Loopback;
            config.infra = crate::infra::Infra::NONE;
            config.pow_bits = Some(8);
            config.min_pow_bits = Some(8);
            config.blob_gc_interval = Duration::from_millis(1);
            let node = Node::start(config).await.unwrap();
            for round in 0..80 {
                let event = publish(&node, dir.path(), 10).await;
                let bytes = node.blob_bytes(&event.blobs[0].hash).await.unwrap();
                assert_eq!(bytes.as_deref(), Some(&content(10)[..]), "round {round}");
                node.drop_body(&event.id, DropReason::Own, None, None)
                    .await
                    .unwrap();
            }
            assert_eq!(node.unreadable_blobs(), 0);
            node.shutdown().await.unwrap();
        });
    }

    /// A blob store that fails ends its collection for good: the node
    /// says so once the runs are overdue.
    #[test]
    fn a_stopped_collection_is_reported() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), true).await;
            collected(&node).await;
            assert!(!node.blob_gc_stopped(now_secs()));
            let last = || {
                node.blob_keep
                    .last_kept
                    .load(std::sync::atomic::Ordering::Relaxed)
            };
            assert!(last() > 0);
            // Every read of the store fails from now on.
            node.blobs.shutdown().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stopped_at = last();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(last(), stopped_at, "no run reads what to keep any more");
            // Ten minutes and two intervals (of 10 ms) after the last run.
            let last_run = stopped_at / 1000;
            assert!(!node.blob_gc_stopped(last_run + 600));
            assert!(node.blob_gc_stopped(last_run + 601));
            let off = start(&dir.path().join("off"), false).await;
            assert!(!off.blob_gc_stopped(last_run + 100_000));
            off.shutdown().await.unwrap();
            node.shutdown().await.unwrap();
        });
    }

    /// A blob store that cannot be opened (here, unreadable) stops the
    /// start with an error that names it, instead of a hang.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_blob_store_fails_the_start() {
        run(async {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join("n");
            start(&data, true).await.shutdown().await.unwrap();
            let blobs = data.join("blobs");
            let mode = |m| std::fs::set_permissions(&blobs, std::fs::Permissions::from_mode(m));
            mode(0o000).unwrap();
            if std::fs::read_dir(&blobs).is_ok() {
                // Root reads it anyway: nothing to test.
                mode(0o755).unwrap();
                return;
            }
            let mut config = NodeConfig::new(&data);
            config.mode = NetworkMode::Loopback;
            config.infra = crate::infra::Infra::NONE;
            let started = tokio::time::timeout(Duration::from_secs(40), Node::start(config)).await;
            mode(0o755).unwrap();
            let Ok(started) = started else {
                panic!("the start hung on an unreadable blob store");
            };
            let Err(e) = started else {
                panic!("started on an unreadable blob store");
            };
            let error = format!("{e:#}");
            assert!(
                error.contains(&format!(
                    "cannot open the blob store in {}",
                    blobs.display()
                )),
                "{error}"
            );
        });
    }

    /// A blob store that never opens (iroh-blobs hangs when it fails to)
    /// is given up after the limit, with the same error.
    #[test]
    fn a_blob_store_that_never_opens_is_given_up() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let never = std::future::pending::<Result<FsStore>>();
            let e = open_blob_store_within(dir.path(), Duration::from_millis(50), never)
                .await
                .err()
                .unwrap();
            let error = format!("{e:#}");
            assert!(error.contains("cannot open the blob store in"), "{error}");
            assert!(error.contains("did not open within 50ms"), "{error}");
        });
    }

    #[test]
    fn storage_gc_false_deletes_nothing() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), false).await;
            let event = publish(&node, dir.path(), 8).await;
            let hash = hash_of(&event);
            let unpinned = loose(&node, 9).await;
            node.drop_body(&event.id, DropReason::Own, None, None)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(node.blob_collections(), 0);
            assert!(present(&node, hash).await);
            assert!(present(&node, unpinned).await);
            node.shutdown().await.unwrap();
        });
    }

    /// A blob fetched from a peer while collections run back to back is
    /// kept from its first byte (a partial blob is not, by itself).
    #[test]
    fn a_blob_fetched_during_collections_survives() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let a = start(&dir.path().join("a"), false).await;
            let mut config = NodeConfig::new(dir.path().join("b"));
            config.mode = NetworkMode::Loopback;
            config.infra = crate::infra::Infra::NONE;
            config.pow_bits = Some(8);
            config.min_pow_bits = Some(8);
            config.blob_gc_interval = Duration::from_millis(1);
            // Another network: B never stores A's post, only fetches its blob.
            config.nets = vec!["gc-test".into()];
            config.peers.push(a.endpoint().addr());
            let b = Node::start(config).await.unwrap();
            // Large enough to arrive over many writes.
            let path = dir.path().join("big.bin");
            let big: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
            std::fs::write(&path, &big).unwrap();
            let event = a
                .publish(
                    Draft {
                        net: PUBLIC_NET.into(),
                        kind: Some(Kind::Note),
                        title: Some("big".into()),
                        text: "a big file".into(),
                        ..Draft::default()
                    },
                    &[Attachment { path, name: None }],
                )
                .await
                .unwrap();
            let blob = event.blobs[0].clone();
            // Not through an event B stores: only the hold keeps it.
            assert!(b.store().get(&event.id).unwrap().is_none());
            assert!(b.fetch_blob(&blob, vec![a.endpoint_id()]).await);
            collected(&b).await;
            assert!(present(&b, hash_of(&event)).await);
            assert_eq!(b.blob_bytes(&blob.hash).await.unwrap().unwrap(), big);
            b.shutdown().await.unwrap();
            a.shutdown().await.unwrap();
        });
    }

    /// Two nodes, B holding (pinned) the blob of a post A published, with
    /// A its neighbor: the post and the blob's path on B.
    async fn held_by_b(dir: &Path) -> (Arc<Node>, Arc<Node>, Event, PathBuf) {
        let config = |name: &str| {
            let mut config = NodeConfig::new(dir.join(name));
            config.mode = NetworkMode::Loopback;
            config.infra = crate::infra::Infra::NONE;
            config.pow_bits = Some(8);
            config.min_pow_bits = Some(8);
            config
        };
        let mut a_config = config("a");
        // No push of the blob to B: only the fetch below brings it, before
        // the test damages it.
        a_config.replication = 0;
        let a = Node::start(a_config).await.unwrap();
        let mut b_config = config("b");
        b_config.peers.push(a.endpoint().addr());
        let b = Node::start(b_config).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !b.neighbors().contains(&a.endpoint_id()) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "A never became B's neighbor"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let event = publish(&a, dir, 7).await;
        assert!(b.fetch_blob(&event.blobs[0], vec![a.endpoint_id()]).await);
        let file = b.blob_data_file(hash_of(&event));
        assert_eq!(std::fs::read(&file).unwrap(), content(7));
        (a, b, event, file)
    }

    /// Read blob `hash` through B's store until it fails, as it does once
    /// it reopens the entry of a damaged file: "poisoned storage" for a
    /// file gone, a read error for a truncated one. Returns the error.
    async fn until_the_store_fails(b: &Node, hash: Hash) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Err(e) = b.blobs.get_bytes(hash).await {
                return format!("{e:#}");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the store still reads the damaged blob"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A file removed while the store has the blob open (#163): the store
    /// fails with "poisoned storage" once it reopens it, and a read on the
    /// node fetches it again from a peer instead of failing.
    #[test]
    fn a_poisoned_blob_reads_again_from_a_peer() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let (a, b, event, file) = held_by_b(dir.path()).await;
            let hash = hash_of(&event);
            std::fs::remove_file(&file).unwrap();
            let error = until_the_store_fails(&b, hash).await;
            assert!(error.contains("poisoned storage"), "{error}");
            let bytes = b.blob_bytes(&event.blobs[0].hash).await.unwrap();
            assert_eq!(bytes.as_deref(), Some(&content(7)[..]));
            assert_eq!(std::fs::read(&file).unwrap(), content(7));
            assert_eq!(b.unreadable_blobs(), 0);
            b.shutdown().await.unwrap();
            a.shutdown().await.unwrap();
        });
    }

    /// A file truncated under the store: the export (MCP `read` with
    /// `fetch_blobs`) gets the blob from a peer, whole, and the file is
    /// whole again.
    #[test]
    fn a_truncated_blob_exports_whole_from_a_peer() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let (a, b, event, file) = held_by_b(dir.path()).await;
            let hash = hash_of(&event);
            std::fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .unwrap()
                .set_len(SIZE as u64 / 2)
                .unwrap();
            until_the_store_fails(&b, hash).await;
            let exported = b
                .fetch_and_export(&event.id, &event.blobs[0])
                .await
                .unwrap();
            let exported = exported.expect("exported");
            assert_eq!(std::fs::read(exported).unwrap(), content(7));
            assert_eq!(std::fs::read(&file).unwrap(), content(7));
            assert_eq!(b.unreadable_blobs(), 0);
            b.shutdown().await.unwrap();
            a.shutdown().await.unwrap();
        });
    }

    /// The store refusing a blob until it reopens ("entity is dead", after
    /// one of its threads panicked observing the poisoned entry, as the
    /// downloader or a peer's observe request would): a read still gets
    /// it, from a peer, restored in its file, and the store reads it again
    /// after a restart.
    #[test]
    fn a_blob_the_store_refuses_is_restored_in_its_file() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let (a, b, event, file) = held_by_b(dir.path()).await;
            let hash = hash_of(&event);
            std::fs::remove_file(&file).unwrap();
            until_the_store_fails(&b, hash).await;
            // Panics a store thread ("poisoned storage should not be used").
            let _ = b.blobs.observe(hash).await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                // Not always an error at first: the store may also answer
                // with no bytes at all.
                let error = format!("{:#}", b.store_read(hash).await.unwrap_err());
                if error.contains("entity is dead") {
                    break;
                }
                assert!(tokio::time::Instant::now() < deadline, "{error}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let bytes = b.blob_bytes(&event.blobs[0].hash).await.unwrap();
            assert_eq!(bytes.as_deref(), Some(&content(7)[..]));
            assert_eq!(std::fs::read(&file).unwrap(), content(7));
            assert_eq!(b.unreadable_blobs(), 0);
            // Still refused by this store: read from the file again.
            assert!(b.blobs.get_bytes(hash).await.is_err());
            let again = b.blob_bytes(&event.blobs[0].hash).await.unwrap();
            assert_eq!(again.as_deref(), Some(&content(7)[..]));
            let data_dir = b.config.data_dir.clone();
            b.shutdown().await.unwrap();
            drop(b);
            a.shutdown().await.unwrap();
            // A new store reads it.
            let store = FsStore::load(data_dir.join("blobs")).await.unwrap();
            assert_eq!(&store.get_bytes(hash).await.unwrap()[..], &content(7)[..]);
            store.shutdown().await.unwrap();
        });
    }

    /// What a read falls back to while the store fails on a blob whose
    /// file is intact (its entry still the poisoned one in memory, which
    /// an import does not replace): the file, only when it holds exactly
    /// the blob.
    #[test]
    fn the_blob_file_is_used_only_when_it_holds_the_blob() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(&dir.path().join("n"), false).await;
            let event = publish(&node, dir.path(), 3).await;
            let hash = hash_of(&event);
            let file = node.blob_data_file(hash);
            assert_eq!(
                node.read_blob_file(hash).await.as_deref(),
                Some(&content(3)[..])
            );
            // Damaged: not read from the file.
            std::fs::write(&file, content(4)).unwrap();
            assert!(node.read_blob_file(hash).await.is_none());
            std::fs::remove_file(&file).unwrap();
            assert!(node.read_blob_file(hash).await.is_none());
            node.shutdown().await.unwrap();
        });
    }
}

/// The swarm as one node sees it (ADR 0026): who broadcast what it got.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod swarm_tests {
    use super::*;
    use crate::event::Kind;

    async fn start(dir: &Path) -> Arc<Node> {
        let mut config = NodeConfig::new(dir);
        config.mode = NetworkMode::Loopback;
        config.infra = crate::infra::Infra::NONE;
        config.pow_bits = Some(8);
        config.min_pow_bits = Some(8);
        Node::start(config).await.unwrap()
    }

    fn envelope(event: &serde_json::Value, providers: &[EndpointId]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "ev": ENVELOPE_VERSION,
            "event": event,
            "providers": providers.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_node_that_broadcast_a_new_event_counts_once() {
        let dir = tempfile::tempdir().unwrap();
        // Two nodes that never meet: b's posts reach a by hand.
        let a = start(&dir.path().join("a")).await;
        let b = start(&dir.path().join("b")).await;
        let seen = |node: &Node| node.swarm_json()["public"]["nodes_24h"].clone();
        assert_eq!(
            a.swarm_json(),
            serde_json::json!({"public": {"neighbors": 0, "nodes_24h": 0}})
        );
        let note = |text: &str| Draft {
            net: PUBLIC_NET.into(),
            kind: Some(Kind::Note),
            title: Some("hello".into()),
            text: text.into(),
            ..Draft::default()
        };
        let post = |text: &'static str| {
            let b = Arc::clone(&b);
            async move { serde_json::to_value(b.publish(note(text), &[]).await.unwrap()).unwrap() }
        };
        let relay = SecretKey::from_bytes(&[9; 32]).public();
        let other = SecretKey::from_bytes(&[10; 32]).public();
        // The first provider is the node that broadcast it; the others
        // are blob hints, not sightings. Nor is `from`: the receive loop
        // counts the neighbor that delivered it.
        let first = post("one").await;
        a.handle_message(
            PUBLIC_NET,
            &envelope(&first, &[b.endpoint_id(), other]),
            relay,
        )
        .await
        .unwrap();
        assert_eq!(seen(&a), 1);
        // The same event again, claimed by another node: not new, not
        // counted.
        a.handle_message(PUBLIC_NET, &envelope(&first, &[other]), relay)
            .await
            .unwrap();
        assert_eq!(seen(&a), 1);
        // An invalid event counts for nothing.
        let mut forged = post("two").await;
        forged["text"] = "changed".into();
        let _ = a
            .handle_message(PUBLIC_NET, &envelope(&forged, &[other]), relay)
            .await;
        assert_eq!(seen(&a), 1);
        // This node's own id is never counted.
        let third = post("three").await;
        a.handle_message(PUBLIC_NET, &envelope(&third, &[a.endpoint_id()]), relay)
            .await
            .unwrap();
        assert_eq!(seen(&a), 1);
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
    }
}
