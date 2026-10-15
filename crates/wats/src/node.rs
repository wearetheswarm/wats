//! A wats node (SPEC §2.1, §2.5): Iroh endpoint, gossip per network, blob
//! store, and the local event store.
//!
//! Events travel over gossip inside an [`Envelope`] that adds transport
//! hints (which nodes can serve the blobs). The envelope is not signed and
//! not stored; only the event is.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_blobs::api::downloader::Downloader;
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
use crate::wire::AnyEvent;

/// The public network id (ring zero, SPEC §2.4). Provisional, ADR 0004.
pub const PUBLIC_NET: &str = "public";

/// Gossip messages are capped by Iroh; events are at most 48 KiB, so leave
/// room for the envelope. Every node must use the same value.
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
            replication: 3,
            bootstrap: Vec::new(),
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

/// Total size of the regular files under `dir`.
fn dir_size(dir: &Path) -> u64 {
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
    /// What peers said in `hello`.
    peer_info: std::sync::Mutex<HashMap<EndpointId, NodeInfo>>,
    /// Cached blob store size: (when measured, bytes).
    usage: std::sync::Mutex<Option<(std::time::Instant, u64)>>,
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
        let lock = lock_data_dir(&config.data_dir)?;
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
        let mut peers = config.peers.clone();
        for entry in &config.bootstrap {
            match entry.resolve().await {
                Ok(addr) if addr.id == endpoint.id() => {}
                Ok(addr) => {
                    info!(peer = %addr.id, host = %entry.host, "bootstrap peer");
                    peers.push(addr);
                }
                Err(e) => warn!(%entry, "bootstrap peer unavailable: {e:#}"),
            }
        }
        for peer in &peers {
            lookup.add_endpoint_info(peer.clone());
        }

        let blobs = FsStore::load(config.data_dir.join("blobs")).await?;
        let store = Arc::new(Store::open(&config.data_dir.join("events.sqlite"))?);
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
            peer_info: Default::default(),
            usage: Default::default(),
            lock: std::sync::Mutex::new(Some(lock)),
        });
        protocol.set_node(&node);
        for (net, receiver) in receivers {
            tokio::spawn(Arc::clone(&node).receive_loop(net, receiver));
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
        Ok(self.root.profile_key(profile))
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
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.neighbor_count(net) > 0 {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
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
        let pow_bits = self.config.pow_bits;
        let ts = now_secs();
        let event = tokio::task::spawn_blocking(move || draft.sign(&key, ts, pow_bits)).await??;
        self.store.insert(&event, ts)?;
        let envelope = Envelope {
            ev: ENVELOPE_VERSION,
            event: serde_json::to_value(&event)?,
            providers: vec![self.endpoint_id().to_string()],
        };
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
                self.config.replication * 2,
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
        if !self.blobs.has(hash).await? {
            return Ok(None);
        }
        let dir = std::path::absolute(self.config.data_dir.join("inbox").join(hash.to_hex()))?;
        let path = dir.join(&blob.name);
        if tokio::fs::try_exists(&path).await? {
            return Ok(Some(path));
        }
        tokio::fs::create_dir_all(&dir).await?;
        let tmp = dir.join(format!(".{}.part", hash.to_hex()));
        self.blobs.export(hash, &tmp).await?;
        let mut perms = tokio::fs::metadata(&tmp).await?.permissions();
        perms.set_readonly(true);
        tokio::fs::set_permissions(&tmp, perms).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(Some(path))
    }

    async fn receive_loop(self: Arc<Self>, net: String, mut receiver: GossipReceiver) {
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
                .take(8)
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
        if event.pow_bits() < self.config.min_pow_bits {
            bail!("insufficient proof of work");
        }
        if event.ts() > now_secs() + MAX_CLOCK_SKEW_SECS {
            bail!("event timestamp is in the future");
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
        let envelope = Envelope {
            ev: ENVELOPE_VERSION,
            event: value,
            providers: vec![self.endpoint_id().to_string()],
        };
        sender
            .broadcast(serde_json::to_vec(&envelope)?.into())
            .await?;
        Ok(event)
    }

    /// Exchange `hello` with a peer and remember what it speaks.
    pub async fn hello(&self, peer: EndpointId) -> Result<NodeInfo> {
        let request = Request::Hello {
            info: NodeInfo::ours(),
        };
        let info = match proto::call(self.endpoint(), peer, &request).await? {
            Response::Hello { info } => info,
            other => bail!("unexpected hello response: {other:?}"),
        };
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
        match request {
            Request::Hello { info } => {
                if let Ok(mut infos) = self.peer_info.lock() {
                    infos.insert(peer, info);
                }
                Response::Hello {
                    info: NodeInfo::ours(),
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
        let mut new = 0;
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
                    Ok(Some((_, true))) => new += 1,
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
        let result = async {
            self.downloader.download(hash, providers).await?;
            self.pin(hash).await?;
            anyhow::Ok(())
        };
        match tokio::time::timeout(Duration::from_secs(120), result).await {
            Ok(Ok(())) => {
                info!(hash = %blob.hash, name = %blob.name, "blob fetched");
                let _ = self.fetched.send(blob.clone());
                true
            }
            Ok(Err(e)) => {
                warn!(hash = %blob.hash, "blob fetch failed: {e:#}");
                false
            }
            Err(_) => {
                warn!(hash = %blob.hash, "blob fetch timed out");
                false
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
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
