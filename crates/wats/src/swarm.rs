//! The size of the swarm, as one node sees it (ADR 0026).
//!
//! No node knows the whole swarm: gossip gives each node a handful of
//! neighbors, and nothing on the wire counts the others. What a node can
//! say honestly, per network:
//!
//! - `neighbors`: its gossip neighbors now (connected, authenticated);
//! - `nodes_24h`: the distinct nodes it heard from on that network in the
//!   last 24 hours: its neighbors, the nodes that delivered it a gossip
//!   message or asked it for events of the network (each authenticated by
//!   its connection), and the node that broadcast each new, valid event it
//!   got by gossip (the first `providers` entry of the envelope: a claim,
//!   at the price of one new valid event per claim). Nodes that stayed
//!   silent and far away are not in it: it is a lower bound of the swarm
//!   that was there, not its size.
//!
//! The sightings are kept in `<data dir>/swarm.json`, so that a node that
//! starts shows the last 24 hours of its previous runs while it connects.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use iroh::EndpointId;
use serde_json::{Value, json};
use tracing::warn;

const FILE: &str = "swarm.json";
/// The window of `nodes_24h`.
pub const WINDOW_SECS: u64 = 24 * 3600;
/// Nodes remembered per network; past it, the stalest is forgotten.
pub const MAX_PER_NET: usize = 10_000;
/// The file is local, but a corrupt or planted one must not make the node
/// allocate without bound.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// When this node last heard from each node, per network.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sightings {
    nets: HashMap<String, HashMap<EndpointId, u64>>,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

impl Sightings {
    /// The sightings of `nets` in `<data dir>/swarm.json` within the
    /// window, without `me`. A missing or unreadable file is empty.
    pub fn load(data_dir: &Path, nets: &[String], me: Option<EndpointId>, now: u64) -> Self {
        let path = path(data_dir);
        let Ok(meta) = std::fs::metadata(&path) else {
            return Self::default();
        };
        if meta.len() > MAX_FILE_BYTES {
            warn!(path = %path.display(), "swarm sightings too large, ignored");
            return Self::default();
        }
        let stored: BTreeMap<String, BTreeMap<String, u64>> = match std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
        {
            Ok(stored) => stored,
            Err(e) => {
                warn!(path = %path.display(), "swarm sightings unreadable, ignored: {e:#}");
                return Self::default();
            }
        };
        let mut sightings = Self::default();
        for (net, ids) in stored {
            if !nets.contains(&net) {
                continue;
            }
            for (id, at) in ids {
                if let Ok(id) = id.parse::<EndpointId>()
                    && Some(id) != me
                {
                    sightings.note(&net, id, at.min(now));
                }
            }
        }
        sightings.prune(now);
        sightings
    }

    /// Heard from `id` on `net` at `at` (Unix seconds).
    pub fn note(&mut self, net: &str, id: EndpointId, at: u64) {
        let seen = self.nets.entry(net.to_string()).or_default();
        if seen.len() >= MAX_PER_NET
            && !seen.contains_key(&id)
            && let Some(stalest) = seen.iter().min_by_key(|(_, t)| **t).map(|(id, _)| *id)
        {
            seen.remove(&stalest);
        }
        let last = seen.entry(id).or_insert(at);
        *last = (*last).max(at);
    }

    /// Forget the sightings older than the window.
    pub fn prune(&mut self, now: u64) {
        let since = now.saturating_sub(WINDOW_SECS);
        for seen in self.nets.values_mut() {
            seen.retain(|_, at| *at >= since);
        }
        self.nets.retain(|_, seen| !seen.is_empty());
    }

    /// Whether `id` was heard from on `net` within the window: a member of
    /// that network, for replica repair (ADR 0029).
    pub fn seen(&self, net: &str, id: &EndpointId, now: u64) -> bool {
        let since = now.saturating_sub(WINDOW_SECS);
        self.nets
            .get(net)
            .and_then(|seen| seen.get(id))
            .is_some_and(|at| *at >= since)
    }

    /// Forget `net` (left on a halt).
    pub fn forget(&mut self, net: &str) {
        self.nets.remove(net);
    }

    /// The nodes heard from on `net` within the window, with `also` (the
    /// neighbors now, heard from or not lately).
    pub fn count(&self, net: &str, now: u64, also: &HashSet<EndpointId>) -> usize {
        let since = now.saturating_sub(WINDOW_SECS);
        let mut nodes: HashSet<&EndpointId> = also.iter().collect();
        if let Some(seen) = self.nets.get(net) {
            nodes.extend(
                seen.iter()
                    .filter(|(_, at)| **at >= since)
                    .map(|(id, _)| id),
            );
        }
        nodes.len()
    }

    /// Rewrite `<data dir>/swarm.json` with the sightings within the window.
    pub fn save(&self, data_dir: &Path, now: u64) -> anyhow::Result<()> {
        let since = now.saturating_sub(WINDOW_SECS);
        let stored: BTreeMap<&str, BTreeMap<String, u64>> = self
            .nets
            .iter()
            .map(|(net, seen)| {
                let ids = seen
                    .iter()
                    .filter(|(_, at)| **at >= since)
                    .map(|(id, at)| (id.to_string(), *at))
                    .collect();
                (net.as_str(), ids)
            })
            .collect();
        let path = path(data_dir);
        let tmp = path.with_extension("json.tmp");
        crate::background::write_file(&tmp, &serde_json::to_string(&stored)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// The `swarm` value of `status` for one network.
pub fn net_json(neighbors: usize, nodes_24h: usize) -> Value {
    json!({ "neighbors": neighbors, "nodes_24h": nodes_24h })
}

/// What a node that is not running (or not connected yet) knows: no
/// neighbor, and the nodes its previous runs heard from in the last 24
/// hours. One entry per network of `nets`.
pub fn stored_json(data_dir: &Path, nets: &[String], now: u64) -> Value {
    let sightings = Sightings::load(data_dir, nets, None, now);
    nets.iter()
        .map(|net| {
            let count = sightings.count(net, now, &HashSet::new());
            (net.clone(), net_json(0, count))
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// One network's `swarm` value for humans: `3 neighbors, 17 nodes seen in
/// 24 h`. `running`: the node runs (no neighbor yet then reads
/// `connecting…`).
pub fn line(value: &Value, running: bool) -> String {
    let neighbors = value["neighbors"].as_u64().unwrap_or(0);
    let seen = value["nodes_24h"].as_u64().unwrap_or(0);
    let (head, sep) = match (running, neighbors) {
        (false, _) => ("not running".to_string(), " · "),
        (true, 0) => ("connecting…".to_string(), " · "),
        (true, n) => (plural(n, "neighbor", "neighbors"), ", "),
    };
    if seen == 0 {
        head
    } else {
        format!("{head}{sep}{} seen in 24 h", plural(seen, "node", "nodes"))
    }
}

/// `swarm: …` lines, one per network (`swarm (net): …` when there are
/// several), from a `swarm` value of `status`.
pub fn lines(swarm: &Value, running: bool) -> Vec<String> {
    let Some(nets) = swarm.as_object() else {
        return Vec::new();
    };
    nets.iter()
        .map(|(net, value)| {
            let text = line(value, running);
            if nets.len() > 1 {
                format!("swarm ({net}): {text}")
            } else {
                format!("swarm: {text}")
            }
        })
        .collect()
}

/// The start banner of a node as `wats attach` and `wats --detach` show it
/// later: its `swarm:` lines (what the node knew when it started) replaced
/// by `live`, the `swarm` value of the running node, when there is one.
pub fn refresh_banner(banner: &[String], live: Option<&Value>) -> Vec<String> {
    let Some(live) = live.filter(|v| v.as_object().is_some_and(|m| !m.is_empty())) else {
        return banner.to_vec();
    };
    let is_swarm = |l: &String| l.starts_with("swarm: ") || l.starts_with("swarm (");
    let mut out = Vec::with_capacity(banner.len());
    let mut fresh = Some(lines(live, true));
    for line in banner {
        if is_swarm(line) {
            out.extend(fresh.take().into_iter().flatten());
        } else {
            out.push(line.clone());
        }
    }
    out
}

/// The `NODES 24H` cell of `wats ps`: `17`, or `public 17 · team 3`.
pub fn ps_cell(swarm: Option<&Value>) -> String {
    let Some(nets) = swarm.and_then(Value::as_object).filter(|m| !m.is_empty()) else {
        return "-".to_string();
    };
    let count = |v: &Value| v["nodes_24h"].as_u64().unwrap_or(0);
    if nets.len() == 1 {
        return nets.values().map(count).sum::<u64>().to_string();
    }
    nets.iter()
        .map(|(net, v)| format!("{net} {}", count(v)))
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn id(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn nets(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn counts_distinct_nodes_in_the_window_with_the_neighbors() {
        let now = 10 * WINDOW_SECS;
        let mut s = Sightings::default();
        let none = HashSet::new();
        assert_eq!(s.count("public", now, &none), 0);
        s.note("public", id(1), now - 10);
        s.note("public", id(1), now - 5);
        assert_eq!(s.count("public", now, &none), 1, "the same node once");
        s.note("public", id(2), now - WINDOW_SECS);
        s.note("public", id(3), now - WINDOW_SECS - 1);
        s.note("team", id(4), now);
        assert_eq!(s.count("public", now, &none), 2, "24 h, not a second more");
        assert_eq!(s.count("team", now, &none), 1, "per network");
        assert_eq!(s.count("other", now, &none), 0);
        // A neighbor up for days, silent since: still counted.
        let neighbors: HashSet<_> = [id(1), id(3), id(9)].into();
        assert_eq!(s.count("public", now, &neighbors), 4);
        // An older sighting never moves the last one back.
        s.note("public", id(1), 0);
        assert_eq!(s.count("public", now, &none), 2);
        s.prune(now);
        assert_eq!(s.nets["public"].len(), 2);
        s.forget("team");
        assert_eq!(s.count("team", now, &none), 0);
    }

    #[test]
    fn bounded_per_network() {
        let mut s = Sightings::default();
        for i in 0..MAX_PER_NET as u64 + 5 {
            let key = iroh::SecretKey::from_bytes(&blake3::hash(&i.to_le_bytes()).into());
            s.note("public", key.public(), 1_000 + i);
        }
        assert_eq!(s.nets["public"].len(), MAX_PER_NET);
        assert_eq!(
            *s.nets["public"].values().min().unwrap(),
            1_005,
            "the stalest went first"
        );
    }

    #[test]
    fn saved_and_loaded_for_the_networks_joined() {
        let dir = tempfile::tempdir().unwrap();
        let now = 5 * WINDOW_SECS;
        let mut s = Sightings::default();
        s.note("public", id(1), now - 100);
        s.note("public", id(2), now - 2 * WINDOW_SECS);
        s.note("public", id(7), now);
        s.note("team", id(3), now);
        s.save(dir.path(), now).unwrap();
        let loaded = Sightings::load(dir.path(), &nets(&["public"]), Some(id(7)), now);
        assert_eq!(loaded.count("public", now, &HashSet::new()), 1);
        assert_eq!(loaded.count("team", now, &HashSet::new()), 0, "not joined");
        // Before the node starts (banner, `wats status` while stopped).
        let stored = stored_json(dir.path(), &nets(&["public", "team"]), now);
        assert_eq!(
            stored,
            json!({"public": {"neighbors": 0, "nodes_24h": 2},
                   "team": {"neighbors": 0, "nodes_24h": 1}})
        );
        // A day later, all of it is stale.
        let later = stored_json(dir.path(), &nets(&["public"]), now + WINDOW_SECS + 1);
        assert_eq!(later, json!({"public": {"neighbors": 0, "nodes_24h": 0}}));
        // Garbage is ignored, never fatal.
        std::fs::write(dir.path().join(FILE), b"{not json").unwrap();
        assert_eq!(
            Sightings::load(dir.path(), &nets(&["public"]), None, now),
            Sightings::default()
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            stored_json(empty.path(), &nets(&["public"]), now),
            json!({"public": {"neighbors": 0, "nodes_24h": 0}})
        );
    }

    #[test]
    fn lines_for_humans() {
        assert_eq!(line(&net_json(0, 0), true), "connecting…");
        assert_eq!(
            line(&net_json(0, 17), true),
            "connecting… · 17 nodes seen in 24 h"
        );
        assert_eq!(
            line(&net_json(1, 1), true),
            "1 neighbor, 1 node seen in 24 h"
        );
        assert_eq!(
            line(&net_json(3, 17), true),
            "3 neighbors, 17 nodes seen in 24 h"
        );
        assert_eq!(
            line(&net_json(0, 5), false),
            "not running · 5 nodes seen in 24 h"
        );
        let one = json!({"public": net_json(2, 4)});
        assert_eq!(
            lines(&one, true),
            ["swarm: 2 neighbors, 4 nodes seen in 24 h"]
        );
        let two = json!({"public": net_json(2, 4), "team": net_json(0, 0)});
        assert_eq!(
            lines(&two, true),
            [
                "swarm (public): 2 neighbors, 4 nodes seen in 24 h",
                "swarm (team): connecting…"
            ]
        );
        assert_eq!(ps_cell(Some(&one)), "4");
        assert_eq!(ps_cell(Some(&two)), "public 4 · team 0");
        assert_eq!(ps_cell(None), "-");
    }

    #[test]
    fn the_banner_shows_the_live_figures() {
        let banner: Vec<String> = [
            "wats 1.0 · data ~/.wats",
            "swarm (public): connecting…",
            "swarm (team): connecting…",
            "agent: off",
        ]
        .map(String::from)
        .to_vec();
        let live = json!({"public": net_json(3, 9), "team": net_json(1, 1)});
        assert_eq!(
            refresh_banner(&banner, Some(&live)),
            [
                "wats 1.0 · data ~/.wats",
                "swarm (public): 3 neighbors, 9 nodes seen in 24 h",
                "swarm (team): 1 neighbor, 1 node seen in 24 h",
                "agent: off",
            ]
        );
        // An older node answers no `swarm`: the banner as it was.
        assert_eq!(refresh_banner(&banner, None), banner);
        assert_eq!(refresh_banner(&banner, Some(&json!({}))), banner);
    }
}
