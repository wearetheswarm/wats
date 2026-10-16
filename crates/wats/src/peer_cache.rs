//! The peers this node met, persisted with their last known addresses
//! (security review B1). At startup a node dials them first; the bootstrap
//! nodes compiled into the binary are only the fallback for an empty cache,
//! or for a cache whose peers no longer answer. The swarm then keeps
//! working when our bootstrap node is down or blocked.
//!
//! The cache is `<data dir>/peers.json`, rewritten atomically. Only peers
//! that answered `hello` are kept: an address a stranger merely claimed is
//! never persisted.

use std::path::{Path, PathBuf};

use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};
use serde::{Deserialize, Serialize};
use tracing::warn;

const CACHE_FILE: &str = "peers.json";
/// Peers kept, most recently seen first.
pub const MAX_CACHED_PEERS: usize = 64;
/// Addresses kept per peer.
const MAX_ADDRS_PER_PEER: usize = 8;
/// Peers not seen for this long are dropped.
pub const MAX_AGE_SECS: u64 = 30 * 24 * 3600;
/// The cache is local, but a corrupt or planted file must not make the
/// node allocate without bound.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// One peer, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPeer {
    pub id: String,
    /// Direct addresses (`ip:port`).
    #[serde(default)]
    pub addrs: Vec<String>,
    /// Home relays.
    #[serde(default)]
    pub relays: Vec<String>,
    /// Unix time, seconds.
    pub last_seen: u64,
}

impl CachedPeer {
    pub fn from_addr(addr: &EndpointAddr, last_seen: u64) -> Self {
        let mut addrs = Vec::new();
        let mut relays = Vec::new();
        for transport in &addr.addrs {
            match transport {
                TransportAddr::Ip(ip) => addrs.push(ip.to_string()),
                TransportAddr::Relay(url) => relays.push(url.to_string()),
                _ => {}
            }
        }
        addrs.truncate(MAX_ADDRS_PER_PEER);
        relays.truncate(MAX_ADDRS_PER_PEER);
        CachedPeer {
            id: addr.id.to_string(),
            addrs,
            relays,
            last_seen,
        }
    }

    /// The dialable address, or None for an invalid entry.
    pub fn to_addr(&self) -> Option<EndpointAddr> {
        let id: EndpointId = self.id.parse().ok()?;
        let ips = self
            .addrs
            .iter()
            .take(MAX_ADDRS_PER_PEER)
            .filter_map(|a| a.parse().ok())
            .map(TransportAddr::Ip);
        let relays = self
            .relays
            .iter()
            .take(MAX_ADDRS_PER_PEER)
            .filter_map(|r| r.parse::<RelayUrl>().ok())
            .map(TransportAddr::Relay);
        Some(EndpointAddr::from_parts(id, ips.chain(relays)))
    }
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(CACHE_FILE)
}

/// The cached peers, most recent first: entries that are invalid, stale
/// or `me` are skipped. A missing or unreadable cache is empty.
pub fn load(data_dir: &Path, me: EndpointId, now: u64) -> Vec<CachedPeer> {
    let path = path(data_dir);
    let Ok(meta) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    if meta.len() > MAX_FILE_BYTES {
        warn!(path = %path.display(), "peer cache too large, ignored");
        return Vec::new();
    }
    let peers: Vec<CachedPeer> = match std::fs::read(&path)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
    {
        Ok(peers) => peers,
        Err(e) => {
            warn!(path = %path.display(), "peer cache unreadable, ignored: {e:#}");
            return Vec::new();
        }
    };
    tidy(peers, me, now)
}

/// Drop invalid, stale and duplicate entries and `me`; most recent first,
/// at most [`MAX_CACHED_PEERS`].
pub fn tidy(mut peers: Vec<CachedPeer>, me: EndpointId, now: u64) -> Vec<CachedPeer> {
    let me = me.to_string();
    peers.retain(|p| {
        p.id != me
            && p.to_addr().is_some()
            && now.saturating_sub(p.last_seen) <= MAX_AGE_SECS
            && (!p.addrs.is_empty() || !p.relays.is_empty())
    });
    peers.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(a.id.cmp(&b.id)));
    let mut seen = std::collections::HashSet::new();
    peers.retain(|p| seen.insert(p.id.clone()));
    peers.truncate(MAX_CACHED_PEERS);
    peers
}

/// Only the loopback addresses of each peer, for loopback (hermetic)
/// nodes; peers left without any are dropped.
pub fn loopback_only(peers: Vec<CachedPeer>) -> Vec<CachedPeer> {
    peers
        .into_iter()
        .filter_map(|mut p| {
            p.relays.clear();
            p.addrs.retain(|a| {
                a.parse::<std::net::SocketAddr>()
                    .is_ok_and(|a| a.ip().is_loopback())
            });
            (!p.addrs.is_empty()).then_some(p)
        })
        .collect()
}

/// Merge `fresh` entries into the cache on disk and rewrite it.
pub fn save(
    data_dir: &Path,
    me: EndpointId,
    now: u64,
    fresh: Vec<CachedPeer>,
) -> anyhow::Result<()> {
    let mut all = fresh;
    all.extend(load(data_dir, me, now));
    let all = tidy(all, me, now);
    let path = path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&all)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn peer(n: u8, last_seen: u64) -> CachedPeer {
        CachedPeer {
            id: id(n).to_string(),
            addrs: vec![format!("192.0.2.{n}:7842")],
            relays: vec!["https://relay.example.com/".into()],
            last_seen,
        }
    }

    #[test]
    fn round_trips_addresses() {
        let p = peer(1, 10);
        let addr = p.to_addr().unwrap();
        assert_eq!(addr.id, id(1));
        assert_eq!(CachedPeer::from_addr(&addr, 10), p);
    }

    #[test]
    fn tidy_drops_stale_invalid_duplicate_and_self() {
        let now = MAX_AGE_SECS + 100;
        let mut bad = peer(5, now);
        bad.id = "nope".into();
        let mut empty = peer(6, now);
        empty.addrs.clear();
        empty.relays.clear();
        let peers = tidy(
            vec![
                peer(1, 50),
                peer(2, now),
                peer(3, 200),
                peer(2, 150),
                peer(9, now),
                bad,
                empty,
            ],
            id(9),
            now,
        );
        let ids: Vec<_> = peers.iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids, vec![id(2).to_string(), id(3).to_string()]);
        assert_eq!(peers[0].last_seen, now, "the most recent entry wins");
        let many: Vec<_> = (0..100u8).map(|n| peer(n, now)).collect();
        assert_eq!(tidy(many, id(200), now).len(), MAX_CACHED_PEERS);
    }

    #[test]
    fn loopback_keeps_local_addresses_only() {
        let mut local = peer(1, 10);
        local.addrs.push("127.0.0.1:4000".into());
        let peers = loopback_only(vec![local, peer(2, 10)]);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].addrs, vec!["127.0.0.1:4000".to_string()]);
        assert!(peers[0].relays.is_empty());
    }

    #[test]
    fn save_merges_and_load_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let me = id(9);
        assert!(load(dir.path(), me, 100).is_empty());
        save(dir.path(), me, 100, vec![peer(1, 90)]).unwrap();
        save(dir.path(), me, 100, vec![peer(2, 95), peer(1, 99)]).unwrap();
        let peers = load(dir.path(), me, 100);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0], peer(1, 99));
        std::fs::write(path(dir.path()), b"{not json").unwrap();
        assert!(
            load(dir.path(), me, 100).is_empty(),
            "corrupt cache is empty"
        );
    }
}
