//! Gossip links that stay alive (0.4.11, ADR 0025 §5 amended).
//!
//! iroh-gossip sees a link as up while its QUIC connection lives, even
//! when nothing it sends arrives. wats watches what it hears:
//!
//! - **Ping.** Every [`super::NodeConfig::gossip_ping`], a node sends a
//!   tiny message to its gossip neighbors only (not relayed). Every
//!   message a neighbor delivers counts as hearing from it, a ping or an
//!   event. A neighbor that says it pings ([`proto::FEATURE_GOSSIP_PING`])
//!   and that this node has not heard for [`SILENT_ROUNDS`] rounds gets a
//!   fresh gossip connection. Nodes before 0.4.11 drop the ping as an
//!   invalid envelope, and are never judged by it.
//! - **A fresh connection** ([`Node::reset_link`]) becomes the active
//!   one on both sides, as after a reconnect. At most every
//!   [`RESET_EVERY`] per neighbor; anti-entropy uses it too, for a
//!   neighbor that held events gossip never brought.
//! - **Diversity.** When every neighbor is on this node's own networks
//!   (private, loopback or link-local addresses, as seeds on one Docker
//!   bridge), and a known peer is not, the node joins that peer through
//!   gossip, so that one link leaves the island. Bootstrap nodes first.
//!   At most every [`DIVERSITY_EVERY`] per network.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use iroh::endpoint::TransportAddrUsage;
use iroh::{EndpointId, TransportAddr};
use serde::Deserialize;
use tracing::{debug, info};

use super::Node;
use crate::proto;

/// Rounds without hearing from a neighbor before its link is taken for
/// dead.
pub const SILENT_ROUNDS: u32 = 3;
/// A neighbor's gossip link is reset at most this often.
pub const RESET_EVERY: Duration = Duration::from_secs(600);
/// The diversity check joins an outside peer at most this often.
pub const DIVERSITY_EVERY: Duration = Duration::from_secs(600);

/// The links' state.
#[derive(Debug, Default)]
pub struct Links {
    /// When each neighbor last delivered a gossip message, per network.
    heard: Mutex<HashMap<(String, EndpointId), Instant>>,
    /// The last reset of each neighbor's link.
    resets: Mutex<HashMap<EndpointId, Instant>>,
    /// The last diversity join, per network.
    joined: Mutex<HashMap<String, Instant>>,
    /// Links reset since the node started.
    reset_count: std::sync::atomic::AtomicU64,
    /// New events of the last hour: when, and whether anti-entropy (not
    /// gossip) brought them (0.4.12, for `status`).
    arrivals: Mutex<std::collections::VecDeque<(Instant, bool)>>,
    /// Link resets of the last hour: when, the peer, and why.
    reset_log: Mutex<std::collections::VecDeque<(Instant, EndpointId, String)>>,
}

/// The window of the `status` counters.
pub const WINDOW: Duration = Duration::from_secs(3600);
/// Events anti-entropy brings in [`WINDOW`] before `status` says that
/// gossip is degraded: a few late ones are normal (a node that just came
/// back, a neighbor's restart).
pub const LATE_FLAG: usize = 5;
/// The arrivals kept, at most (a busy hour counts up to here).
const ARRIVALS_MAX: usize = 100_000;

/// `status`'s gossip line: `gossip: 120 new events by gossip, 3 late by
/// anti-entropy (last hour) · 1 link reset: 9f5d9e24df (nothing heard…)`.
pub fn summary(gossip: &serde_json::Value) -> String {
    use serde_json::Value;
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let n = |k: &str| {
        gossip
            .get("last_hour")
            .and_then(|h| h.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let mut line = format!(
        "{} new events by gossip, {} late by anti-entropy (last hour)",
        n("by_gossip"),
        n("by_anti_entropy")
    );
    if let Some(resets) = gossip
        .get("resets")
        .and_then(Value::as_array)
        .filter(|r| !r.is_empty())
    {
        let peers: Vec<String> = resets
            .iter()
            .map(|r| {
                format!(
                    "{} ({})",
                    str_of(r, "peer")
                        .and_then(|p| p.get(..10).map(str::to_string))
                        .unwrap_or_else(|| "?".into()),
                    str_of(r, "why").unwrap_or_default()
                )
            })
            .collect();
        line.push_str(&format!(
            " · {} link reset(s): {}",
            resets.len(),
            peers.join(", ")
        ));
    }
    if let Some(flag) = str_of(gossip, "degraded") {
        line = format!("degraded: {flag} · {line}");
    }
    line
}

/// The peer to join when every neighbor is near (`Some(true)`): the first
/// candidate that is not near and not a neighbor already. `None` when a
/// neighbor is not near, unknown, or there are fewer than two.
pub fn outside_peer(
    me: EndpointId,
    neighbors: &[(EndpointId, Option<bool>)],
    candidates: &[(EndpointId, Option<bool>)],
) -> Option<EndpointId> {
    if neighbors.len() < 2 || neighbors.iter().any(|(_, near)| *near != Some(true)) {
        return None;
    }
    candidates
        .iter()
        .find(|(peer, near)| {
            *peer != me && *near != Some(true) && !neighbors.iter().any(|(n, _)| n == peer)
        })
        .map(|(peer, _)| *peer)
}

/// The ping, as it travels: no envelope (`ev`, `event`), so a node that
/// does not know it drops it.
#[derive(Deserialize)]
struct Ping {
    #[allow(dead_code)]
    wats_ping: u32,
}

/// A ping's bytes. The nonce keeps two pings apart: gossip drops a
/// message it has already seen.
pub fn ping_bytes() -> Vec<u8> {
    let mut nonce = [0u8; 8];
    if getrandom::fill(&mut nonce).is_err() {
        nonce = super::now_secs().to_le_bytes();
    }
    // Within I-JSON's safe integers, as the strict parser wants.
    let n = u64::from_le_bytes(nonce) & ((1 << 53) - 1);
    format!("{{\"wats_ping\":1,\"n\":{n}}}").into_bytes()
}

/// Whether `bytes` is a ping.
pub fn is_ping(bytes: &[u8]) -> bool {
    bytes.len() < 128 && crate::strict_json::from_slice::<Ping>(bytes).is_ok()
}

/// An address on this node's own networks: loopback, private (RFC 1918,
/// unique local) or link-local.
pub fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

impl Node {
    /// Gossip links reset since the node started ([`Node::reset_link`]).
    pub fn link_resets(&self) -> u64 {
        self.links
            .reset_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A new event stored: by gossip, or `late`, by anti-entropy.
    pub(crate) fn note_arrivals(&self, late: bool, n: usize) {
        if let Ok(mut arrivals) = self.links.arrivals.lock() {
            let now = Instant::now();
            for _ in 0..n {
                arrivals.push_back((now, late));
            }
            while arrivals.len() > ARRIVALS_MAX
                || arrivals.front().is_some_and(|(t, _)| t.elapsed() > WINDOW)
            {
                arrivals.pop_front();
            }
        }
    }

    /// For `status` (0.4.12): what arrived how in the last hour, the links
    /// reset, and a `degraded` flag when anti-entropy brought more than
    /// [`LATE_FLAG`] events: gossip lost them on the way.
    pub fn gossip_json(&self) -> serde_json::Value {
        let (by_gossip, late) = self
            .links
            .arrivals
            .lock()
            .map(|a| {
                let recent = a.iter().filter(|(t, _)| t.elapsed() <= WINDOW);
                recent.fold((0usize, 0usize), |(g, l), (_, late)| match late {
                    true => (g, l.saturating_add(1)),
                    false => (g.saturating_add(1), l),
                })
            })
            .unwrap_or_default();
        let resets: Vec<serde_json::Value> = self
            .links
            .reset_log
            .lock()
            .map(|r| {
                r.iter()
                    .filter(|(t, _, _)| t.elapsed() <= WINDOW)
                    .map(|(t, peer, why)| {
                        serde_json::json!({"peer": peer.to_string(), "why": why,
                                           "secs_ago": t.elapsed().as_secs()})
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut out = serde_json::json!({
            "last_hour": {"by_gossip": by_gossip, "by_anti_entropy": late},
            "resets": resets,
        });
        if late > LATE_FLAG
            && let Some(map) = out.as_object_mut()
        {
            map.insert(
                "degraded".into(),
                format!("{late} events arrived late (anti-entropy brought what gossip had not)")
                    .into(),
            );
        }
        out
    }

    /// `peer` delivered a gossip message on `net`.
    pub(super) fn heard(&self, net: &str, peer: EndpointId) {
        if let Ok(mut heard) = self.links.heard.lock() {
            heard.insert((net.to_string(), peer), Instant::now());
        }
    }

    /// Give `peer` a fresh gossip connection, at most every
    /// [`RESET_EVERY`]. Returns whether it did.
    pub(crate) async fn reset_link(&self, net: &str, peer: EndpointId, why: &str) -> bool {
        let due = self.links.resets.lock().is_ok_and(|mut resets| {
            let now = Instant::now();
            let due = resets
                .get(&peer)
                .is_none_or(|t| now.duration_since(*t) >= RESET_EVERY);
            if due {
                resets.insert(peer, now);
            }
            due
        });
        if !due {
            return false;
        }
        match self.reset_gossip_link(peer).await {
            Ok(()) => {
                self.links
                    .reset_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut log) = self.links.reset_log.lock() {
                    log.push_back((Instant::now(), peer, why.to_string()));
                    while log.len() > 64
                        || log.front().is_some_and(|(t, _, _)| t.elapsed() > WINDOW)
                    {
                        log.pop_front();
                    }
                }
                info!(%net, peer = %peer.fmt_short(), "gossip link looked dead ({why}): reconnected");
                // A grace before it is judged again.
                self.heard(net, peer);
                true
            }
            Err(e) => {
                debug!(peer = %peer.fmt_short(), "gossip link reset: {e:#}");
                false
            }
        }
    }

    pub(super) fn spawn_links(self: &Arc<Self>, net: &str) {
        if self.config.gossip_ping.is_zero() {
            return;
        }
        let weak = Arc::downgrade(self);
        let name = format!("gossip links {net}");
        let net = net.to_string();
        tokio::spawn(async move {
            crate::supervise::supervise(&name, move || links_loop(weak.clone(), net.clone())).await;
        });
    }

    /// One round: ping the neighbors, reset the silent ones, and look at
    /// the diversity of the neighbors.
    async fn links_round(&self, net: &str) {
        let neighbors = self.neighbors_of(net);
        if neighbors.is_empty() {
            return;
        }
        let pinging: Vec<EndpointId> = neighbors
            .iter()
            .copied()
            .filter(|p| self.has_feature(p, proto::FEATURE_GOSSIP_PING))
            .collect();
        if let Some(sender) = self.sender(net)
            && let Err(e) = sender.broadcast_neighbors(ping_bytes().into()).await
        {
            debug!(%net, "gossip ping: {e:#}");
        }
        let silent_after = self.config.gossip_ping.saturating_mul(SILENT_ROUNDS);
        let silent: Vec<EndpointId> = self
            .links
            .heard
            .lock()
            .map(|heard| {
                pinging
                    .iter()
                    .copied()
                    .filter(|p| {
                        heard
                            .get(&(net.to_string(), *p))
                            .is_none_or(|t| t.elapsed() > silent_after)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for peer in silent {
            let why = format!("nothing heard from it for {} s", silent_after.as_secs());
            self.reset_link(net, peer, &why).await;
        }
        self.keep_diverse(net, &neighbors).await;
    }

    /// Whether `peer` is on this node's own networks: by the addresses in
    /// use when there are some, else by every address known (all local:
    /// near; a public one, or a relay only: not). `None`: nothing known.
    async fn near(&self, peer: EndpointId) -> Option<bool> {
        let info = self.endpoint().remote_info(peer).await?;
        let all: Vec<(TransportAddr, bool)> = info
            .addrs()
            .map(|a| {
                (
                    a.addr().clone(),
                    matches!(a.usage(), TransportAddrUsage::Active),
                )
            })
            .collect();
        let local = |a: &TransportAddr| matches!(a, TransportAddr::Ip(ip) if is_local_ip(ip.ip()));
        let active: Vec<&TransportAddr> =
            all.iter().filter(|(_, on)| *on).map(|(a, _)| a).collect();
        if !active.is_empty() {
            return Some(active.into_iter().all(local));
        }
        if all.is_empty() {
            return None;
        }
        let ips: Vec<&TransportAddr> = all
            .iter()
            .map(|(a, _)| a)
            .filter(|a| matches!(a, TransportAddr::Ip(_)))
            .collect();
        Some(!ips.is_empty() && ips.into_iter().all(local))
    }

    /// Every neighbor on this node's own networks: join a known peer that
    /// is not, a bootstrap node first ([`outside_peer`]).
    async fn keep_diverse(&self, net: &str, neighbors: &[EndpointId]) {
        if neighbors.len() < 2 {
            return;
        }
        let due = self.links.joined.lock().is_ok_and(|joined| {
            joined
                .get(net)
                .is_none_or(|t| t.elapsed() >= DIVERSITY_EVERY)
        });
        if !due {
            return;
        }
        let mut near = Vec::new();
        for peer in neighbors {
            let n = self.near(*peer).await;
            if n != Some(true) {
                return;
            }
            near.push((*peer, n));
        }
        let mut candidates = Vec::new();
        for peer in self.seed_ids.iter().copied().chain(self.known_peers()) {
            if !candidates.iter().any(|(c, _)| *c == peer) {
                candidates.push((peer, self.near(peer).await));
            }
        }
        let Some(peer) = outside_peer(self.endpoint_id(), &near, &candidates) else {
            return;
        };
        let Some(sender) = self.sender(net) else {
            return;
        };
        if let Ok(mut joined) = self.links.joined.lock() {
            joined.insert(net.to_string(), Instant::now());
        }
        match sender.join_peers(vec![peer]).await {
            Ok(()) => info!(
                %net,
                peer = %peer.fmt_short(),
                "every gossip neighbor is on this node's own networks: joining one outside them"
            ),
            Err(e) => debug!(%net, "diversity join: {e:#}"),
        }
    }
}

async fn links_loop(weak: Weak<Node>, net: String) {
    loop {
        let every = {
            let Some(node) = weak.upgrade() else { return };
            node.config.gossip_ping
        };
        tokio::time::sleep(every).await;
        let Some(node) = weak.upgrade() else { return };
        if node.endpoint().is_closed() {
            return;
        }
        node.links_round(&net).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn pings_are_not_envelopes_and_differ() {
        let (a, b) = (ping_bytes(), ping_bytes());
        assert!(is_ping(&a) && is_ping(&b));
        assert_ne!(a, b, "a nonce each");
        assert!(!is_ping(br#"{"ev":1,"event":{},"providers":[]}"#));
        assert!(!is_ping(b"not json"));
    }

    #[test]
    fn an_island_joins_the_first_peer_outside_it() {
        let id = |n: u8| iroh::SecretKey::from_bytes(&[n; 32]).public();
        let (me, a, b, router, other) = (id(1), id(2), id(3), id(4), id(5));
        let island = [(a, Some(true)), (b, Some(true))];
        let candidates = [(a, Some(true)), (router, Some(false)), (other, None)];
        assert_eq!(outside_peer(me, &island, &candidates), Some(router));
        // One neighbor outside already, or unknown: nothing to do.
        assert_eq!(
            outside_peer(me, &[(a, Some(true)), (router, Some(false))], &candidates),
            None
        );
        assert_eq!(
            outside_peer(me, &[(a, Some(true)), (b, None)], &candidates),
            None
        );
        // A single neighbor: not an island yet.
        assert_eq!(outside_peer(me, &[(a, Some(true))], &candidates), None);
        // Only near candidates, or ourselves.
        assert_eq!(
            outside_peer(me, &island, &[(me, Some(false)), (b, Some(true))]),
            None
        );
    }

    #[test]
    fn the_status_line_says_how_events_arrived() {
        let quiet = serde_json::json!({"last_hour": {"by_gossip": 120, "by_anti_entropy": 0}, "resets": []});
        assert_eq!(
            summary(&quiet),
            "120 new events by gossip, 0 late by anti-entropy (last hour)"
        );
        let bad = serde_json::json!({
            "last_hour": {"by_gossip": 3, "by_anti_entropy": 9},
            "resets": [{"peer": "9f5d9e24df0123", "why": "nothing heard from it for 180 s"}],
            "degraded": "9 events arrived late (anti-entropy brought what gossip had not)",
        });
        assert_eq!(
            summary(&bad),
            "degraded: 9 events arrived late (anti-entropy brought what gossip had not) · 3 new \
             events by gossip, 9 late by anti-entropy (last hour) · 1 link reset(s): 9f5d9e24df \
             (nothing heard from it for 180 s)"
        );
    }

    #[test]
    fn local_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.31.250.4",
            "192.168.1.9",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_local_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["159.173.133.31", "8.8.8.8", "2001:db8::1", "100.64.0.1"] {
            assert!(!is_local_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
