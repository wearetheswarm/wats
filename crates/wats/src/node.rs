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
    author: SigningKey,
    lookup: MemoryLookup,
    senders: HashMap<String, GossipSender>,
    neighbors: std::sync::Mutex<HashMap<String, HashSet<EndpointId>>>,
    fetched: tokio::sync::broadcast::Sender<BlobRef>,
    _lock: std::fs::File,
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
        drop(root);

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
        let router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
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
            author,
            lookup,
            senders,
            neighbors: Default::default(),
            fetched: tokio::sync::broadcast::channel(256).0,
            _lock: lock,
        });
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

    /// Sign, store and broadcast a post.
    pub async fn publish(&self, mut draft: Draft, attachments: &[Attachment]) -> Result<Event> {
        let Some(sender) = self.senders.get(&draft.net) else {
            bail!("this node has not joined network {:?}", draft.net);
        };
        draft.blobs.extend(self.import(attachments).await?);
        let key = self.author.clone();
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
        Ok(event)
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

    async fn receive_loop(self: Arc<Self>, net: String, mut receiver: GossipReceiver) {
        loop {
            match receiver.next().await {
                Some(Ok(GossipEvent::Received(msg))) => {
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

    async fn handle_message(
        self: &Arc<Self>,
        net: &str,
        bytes: &[u8],
        from: EndpointId,
    ) -> Result<()> {
        let envelope: Envelope = serde_json::from_slice(bytes)?;
        let event = envelope.event;
        event.verify()?;
        if event.net != net {
            bail!("event for net {:?} on topic {net:?}", event.net);
        }
        if event.pow_bits() < self.config.min_pow_bits {
            bail!("insufficient proof of work");
        }
        if event.ts > now_secs() + MAX_CLOCK_SKEW_SECS {
            bail!("event timestamp is in the future");
        }
        if self.store.insert(&event, now_secs())? == Inserted::Duplicate {
            return Ok(());
        }
        let mut providers: Vec<EndpointId> = envelope
            .providers
            .iter()
            .take(8)
            .filter_map(|p| p.parse().ok())
            .collect();
        providers.push(from);
        for blob in event.blobs.iter().cloned() {
            if blob.url.is_some() || blob.size > self.config.max_blob_bytes {
                continue;
            }
            let node = Arc::clone(self);
            let providers = providers.clone();
            tokio::spawn(async move { node.fetch_blob(&blob, providers).await });
        }
        Ok(())
    }

    /// Download a blob from providers. BLAKE3 verified streaming makes the
    /// content match the hash; the content itself stays untrusted data and
    /// is never executed (SPEC §2.15).
    async fn fetch_blob(&self, blob: &BlobRef, providers: Vec<EndpointId>) {
        let Ok(hash) = blob.hash.parse::<Hash>() else {
            return;
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
            }
            Ok(Err(e)) => warn!(hash = %blob.hash, "blob fetch failed: {e:#}"),
            Err(_) => warn!(hash = %blob.hash, "blob fetch timed out"),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.gossip.shutdown().await.ok();
        self.router.shutdown().await?;
        // The router shuts the blob protocol down, which also closes the store.
        self.blobs.shutdown().await.ok();
        Ok(())
    }
}
