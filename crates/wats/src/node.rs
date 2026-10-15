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

use crate::event::{BlobRef, Draft, Event, is_safe_blob_name};
use crate::identity::{DEFAULT_PROFILE, RootSecret, author_id};
use crate::proto::{self, Request, Response, WatsProtocol};
use crate::store::{Inserted, Store};

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
    /// Networks this node joins.
    pub nets: Vec<String>,
    /// Bootstrap peers.
    pub peers: Vec<EndpointAddr>,
    /// Author profile used for posts (SPEC §2.2).
    pub profile: String,
    /// Proof-of-work difficulty mined on our own posts.
    pub pow_bits: u32,
    /// Minimum proof-of-work difficulty accepted from others.
    pub min_pow_bits: u32,
    /// Largest blob this node publishes or fetches, in bytes.
    pub max_blob_bytes: u64,
    /// Replication factor k for blobs: how many peers are asked to hold a
    /// blob at publish time (SPEC §2.5, §7).
    pub replication: usize,
}

impl NodeConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            mode: NetworkMode::Internet,
            nets: vec![PUBLIC_NET.to_string()],
            peers: Vec::new(),
            profile: DEFAULT_PROFILE.to_string(),
            pow_bits: 12,
            min_pow_bits: 12,
            max_blob_bytes: 256 * 1024 * 1024,
            replication: 3,
        }
    }
}

/// What travels over gossip.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    event: Event,
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
        let root = RootSecret::load_or_create(&config.data_dir.join("secret.key"))?;
        let author = root.profile_key(&config.profile);
        let secret = SecretKey::from_bytes(&root.transport_key_bytes());

        let lookup = MemoryLookup::new();
        let builder = match config.mode {
            NetworkMode::Internet => Endpoint::builder(presets::N0),
            NetworkMode::Loopback => Endpoint::builder(presets::Minimal)
                .clear_ip_transports()
                .bind_addr("127.0.0.1:0")?,
        };
        let endpoint = builder
            .secret_key(secret)
            .address_lookup(lookup.clone())
            .bind()
            .await?;
        for peer in &config.peers {
            lookup.add_endpoint_info(peer.clone());
        }

        let blobs = FsStore::load(config.data_dir.join("blobs")).await?;
        let store = Arc::new(Store::open(&config.data_dir.join("events.sqlite"))?);
        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        let downloader = blobs.downloader(&endpoint);
        let protocol = WatsProtocol::default();
        let router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(proto::ALPN, protocol.clone())
            .spawn();

        let bootstrap: Vec<EndpointId> = config.peers.iter().map(|p| p.id).collect();
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
            lock: std::sync::Mutex::new(Some(lock)),
        });
        protocol.set_node(&node);
        for (net, receiver) in receivers {
            tokio::spawn(Arc::clone(&node).receive_loop(net, receiver));
        }
        info!(id = %node.endpoint_id(), author = %node.author_id(), "node started");
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
            event: event.clone(),
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
                event: Box::new(event.clone()),
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
        let envelope: Envelope = serde_json::from_slice(bytes)?;
        let event = envelope.event;
        if event.net != net {
            bail!("event for net {:?} on topic {net:?}", event.net);
        }
        if !self.accept_event(&event)? {
            return Ok(());
        }
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
        Ok(())
    }

    /// Node policy for events from other nodes, then store. Returns whether
    /// the event is new.
    fn accept_event(&self, event: &Event) -> Result<bool> {
        event.verify()?;
        if !self.senders.contains_key(&event.net) {
            bail!("not a member of network {:?}", event.net);
        }
        if event.pow_bits() < self.config.min_pow_bits {
            bail!("insufficient proof of work");
        }
        if event.ts > now_secs() + MAX_CLOCK_SKEW_SECS {
            bail!("event timestamp is in the future");
        }
        Ok(self.store.insert(event, now_secs())? != Inserted::Duplicate)
    }

    /// Serve a `/wats/1` request from `peer`.
    pub(crate) async fn handle_request(&self, peer: EndpointId, request: Request) -> Response {
        self.seen(peer);
        match request {
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
                        events: page.into_iter().map(|s| (*s.event).clone()).collect(),
                    },
                    Err(e) => Response::Refused {
                        reason: format!("{e:#}"),
                    },
                }
            }
            Request::Replicate { event, hashes } => {
                if let Err(e) = self.accept_event(&event) {
                    return Response::Refused {
                        reason: format!("{e:#}"),
                    };
                }
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
            for event in &events {
                if event.net != net
                    || (event.ts, event.id.as_str()) <= (after_ts, after_id.as_str())
                {
                    bail!("peer sent an out-of-order sync page");
                }
                match self.accept_event(event) {
                    Ok(true) => new += 1,
                    Ok(false) => {}
                    Err(e) => debug!(id = %event.id, "sync: rejected event: {e:#}"),
                }
            }
            match events.last() {
                Some(last) if more => {
                    after_ts = last.ts;
                    after_id = last.id.clone();
                }
                _ => break,
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
