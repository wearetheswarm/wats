//! Reconnection: staying connected is the node's job. A node with no
//! connected peer never sits there silently; it dials again until a peer
//! answers, and says what it is doing.
//!
//! While the node has no gossip neighbor, it retries with exponential
//! backoff: [`Backoff::first`], doubled at each attempt, with jitter,
//! capped at [`Backoff::max`]. Each attempt resolves every bootstrap entry
//! again (DNS is never resolved once for good), and dials the configured
//! peers, the peer cache and the bootstrap nodes: a fresh connection to
//! each (10 s at most), handed to gossip, which never dials again a peer
//! whose dial failed (see `Node::redial`). The backoff starts over
//! on a network change (new addresses, the home relay back) and as soon as
//! a peer is back. When peers come back, the node catches up on what it
//! missed (a sync with each, see [`crate::node::Node::sync_from`]).
//!
//! The `connectivity` value of the `health` value of `status` (stable;
//! see `docs/agent.md`):
//!
//! ```json
//! {"state": "offline", "peers": 0, "since": "2026-10-18T18:00:00Z",
//!  "attempt": 3, "next_in_secs": 8}
//! ```
//!
//! `state` is `connecting` (just started, first peers being dialed),
//! `online` (`peers` > 0) or `offline` (no peer; `since` when it went
//! offline, `attempt` the attempts so far, `next_in_secs` until the next
//! one). Every change of state is logged once, and every failed attempt
//! once.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::time::Duration;

use serde_json::{Value, json};
use tracing::{info, warn};

/// How long the node retries, and how fast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Before the first retry once the node lost its peers.
    pub first: Duration,
    /// The longest wait between two attempts.
    pub max: Duration,
    /// After a start, before the first retry: the first dials get this
    /// long to answer.
    pub start_grace: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            first: Duration::from_secs(1),
            max: Duration::from_secs(5 * 60),
            start_grace: Duration::from_secs(15),
        }
    }
}

impl Backoff {
    /// The wait before attempt `attempt` (1 for the first): `first`
    /// doubled `attempt - 1` times, at most `max`; then "equal jitter",
    /// half of it fixed and half random (`jitter` in [0, 1)), so nodes cut
    /// off together do not all dial back at once.
    pub fn delay(&self, attempt: u32, jitter: f64) -> Duration {
        let doublings = attempt.saturating_sub(1).min(30);
        let full = self
            .first
            .saturating_mul(1u32 << doublings)
            .min(self.max)
            .max(Duration::from_millis(1));
        let half = full.mul_f64(0.5);
        half.saturating_add(half.mul_f64(jitter.clamp(0.0, 1.0)))
    }
}

/// A random number in [0, 1) for the jitter.
pub fn jitter() -> f64 {
    match getrandom::u32() {
        Ok(n) => f64::from(n) / (f64::from(u32::MAX) + 1.0),
        Err(_) => 0.5,
    }
}

/// Where the node stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Started, the first dials in flight.
    Connecting {
        since: u64,
    },
    Online {
        peers: usize,
        since: u64,
    },
    /// No peer since `since`; `attempt` attempts so far, the next at
    /// `next_at` (unix seconds). `dialable`: there is somewhere to dial.
    Offline {
        since: u64,
        attempt: u32,
        next_at: u64,
        dialable: bool,
    },
}

/// A change worth one log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Lost every peer (or none answered after a start).
    WentOffline { next_in: Duration },
    /// An attempt found no peer.
    Retrying { attempt: u32, next_in: Duration },
    /// A peer is back: `offline_for` (seconds) since the node went offline.
    BackOnline { peers: usize, offline_for: u64 },
}

impl Transition {
    pub fn log(&self) {
        match self {
            Transition::WentOffline { next_in } => warn!(
                "connectivity: offline, no connected peer; reconnecting (next attempt in {})",
                format_wait(*next_in)
            ),
            Transition::Retrying { attempt, next_in } => info!(
                "connectivity: reconnecting (attempt {attempt} found no peer, next in {})",
                format_wait(*next_in)
            ),
            Transition::BackOnline { peers, offline_for } => info!(
                "connectivity: back online ({peers} {}) after {} offline; catching up",
                if *peers == 1 { "peer" } else { "peers" },
                format_span(*offline_for)
            ),
        }
    }
}

/// "8s", "2m30s".
pub fn format_wait(wait: Duration) -> String {
    let secs = wait.as_secs_f64().ceil() as u64;
    match secs {
        0..60 => format!("{secs}s"),
        _ if secs.is_multiple_of(60) => format!("{}m", secs / 60),
        _ => format!("{}m{:02}s", secs / 60, secs % 60),
    }
}

fn format_span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The node's connectivity, as the reconnect loop sees it.
#[derive(Debug, Clone)]
pub struct Connectivity {
    pub state: State,
}

impl Connectivity {
    pub fn new(now: u64) -> Self {
        Connectivity {
            state: State::Connecting { since: now },
        }
    }

    /// The node has `peers` neighbors: online, or back online.
    pub fn online(&mut self, peers: usize, now: u64) -> Option<Transition> {
        let transition = match &self.state {
            State::Online { .. } => None,
            State::Connecting { .. } => {
                info!(
                    "connectivity: online ({peers} {})",
                    if peers == 1 { "peer" } else { "peers" }
                );
                None
            }
            State::Offline {
                since, dialable, ..
            } => dialable.then(|| Transition::BackOnline {
                peers,
                offline_for: now.saturating_sub(*since),
            }),
        };
        let since = match &self.state {
            State::Online { since, .. } => *since,
            _ => now,
        };
        self.state = State::Online { peers, since };
        transition
    }

    /// No peer; attempt `attempt` comes in `next_in`. `dialable`: the
    /// node has somewhere to dial (otherwise it waits quietly, e.g. the
    /// first node of a private network).
    pub fn offline(
        &mut self,
        attempt: u32,
        next_in: Duration,
        dialable: bool,
        now: u64,
    ) -> Option<Transition> {
        let next_at = now.saturating_add(next_in.as_secs_f64().ceil() as u64);
        let (since, transition) = match &self.state {
            State::Offline { since, .. } => (
                *since,
                (dialable && attempt > 1).then_some(Transition::Retrying {
                    attempt: attempt.saturating_sub(1),
                    next_in,
                }),
            ),
            State::Connecting { since } => (
                *since,
                dialable.then_some(Transition::WentOffline { next_in }),
            ),
            State::Online { .. } => (now, dialable.then_some(Transition::WentOffline { next_in })),
        };
        self.state = State::Offline {
            since,
            attempt: attempt.saturating_sub(1),
            next_at,
            dialable,
        };
        transition
    }

    /// The `connectivity` value (see the module documentation).
    pub fn json(&self, now: u64) -> Value {
        let ts = crate::update::format_timestamp;
        match &self.state {
            State::Connecting { since } => {
                json!({"state": "connecting", "peers": 0, "since": ts(*since)})
            }
            State::Online { peers, since } => {
                json!({"state": "online", "peers": peers, "since": ts(*since)})
            }
            State::Offline {
                since,
                attempt,
                next_at,
                dialable,
            } => {
                if *dialable {
                    json!({"state": "offline", "peers": 0, "since": ts(*since),
                           "attempt": attempt, "next_in_secs": next_at.saturating_sub(now)})
                } else {
                    json!({"state": "offline", "peers": 0, "since": ts(*since),
                           "detail": "no known peer to dial"})
                }
            }
        }
    }
}

/// One phrase for humans: "online (3 peers)", "offline since …,
/// reconnecting (attempt 3, next in 8s)".
pub fn summary(connectivity: &Value) -> String {
    let peers = connectivity["peers"].as_u64().unwrap_or(0);
    let since = connectivity["since"].as_str().unwrap_or("?");
    match connectivity["state"].as_str().unwrap_or("unknown") {
        "online" => format!(
            "online ({peers} {})",
            if peers == 1 { "peer" } else { "peers" }
        ),
        "connecting" => "connecting".to_string(),
        "offline" => match (
            connectivity["attempt"].as_u64(),
            connectivity["next_in_secs"].as_u64(),
        ) {
            (Some(attempt), Some(next)) => format!(
                "offline since {since}, reconnecting (attempt {}, next in {})",
                attempt.saturating_add(1),
                format_wait(Duration::from_secs(next))
            ),
            _ => format!(
                "offline since {since}, {}",
                connectivity["detail"]
                    .as_str()
                    .unwrap_or("no known peer to dial")
            ),
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_with_jitter_up_to_the_cap() {
        let b = Backoff::default();
        // Without jitter: half of 1, 2, 4, … s.
        assert_eq!(b.delay(1, 0.0), Duration::from_millis(500));
        assert_eq!(b.delay(2, 0.0), Duration::from_secs(1));
        assert_eq!(b.delay(4, 0.0), Duration::from_secs(4));
        // With the most jitter: the full doubling.
        assert_eq!(b.delay(4, 1.0), Duration::from_secs(8));
        // Capped at 5 minutes, whatever the attempt.
        assert_eq!(b.delay(20, 1.0), Duration::from_secs(300));
        assert_eq!(b.delay(u32::MAX, 0.999), b.delay(40, 0.999));
        assert!(b.delay(u32::MAX, 0.0) >= Duration::from_secs(150));
        for _ in 0..100 {
            let j = jitter();
            assert!((0.0..1.0).contains(&j));
        }
    }

    #[test]
    fn logs_going_offline_each_retry_and_coming_back() {
        let mut c = Connectivity::new(100);
        assert_eq!(c.json(100)["state"], "connecting");
        // No peer answered the first dials.
        let second = Duration::from_secs(2);
        assert_eq!(
            c.offline(1, second, true, 115),
            Some(Transition::WentOffline { next_in: second })
        );
        let json = c.json(116);
        assert_eq!(json["state"], "offline");
        assert_eq!(json["since"], crate::update::format_timestamp(100));
        assert_eq!(json["attempt"], 0);
        assert_eq!(json["next_in_secs"], 1);
        assert_eq!(
            summary(&json),
            format!(
                "offline since {}, reconnecting (attempt 1, next in 1s)",
                crate::update::format_timestamp(100)
            )
        );
        let four = Duration::from_secs(4);
        assert_eq!(
            c.offline(2, four, true, 117),
            Some(Transition::Retrying {
                attempt: 1,
                next_in: four
            })
        );
        assert_eq!(c.json(117)["attempt"], 1);
        assert_eq!(
            c.online(2, 160),
            Some(Transition::BackOnline {
                peers: 2,
                offline_for: 60
            })
        );
        assert_eq!(summary(&c.json(160)), "online (2 peers)");
        // A change in the number of peers is no transition.
        assert_eq!(c.online(3, 170), None);
        assert_eq!(c.json(170)["since"], crate::update::format_timestamp(160));
        // Lost them: offline from now.
        assert!(matches!(
            c.offline(1, second, true, 200),
            Some(Transition::WentOffline { .. })
        ));
        assert_eq!(c.json(200)["since"], crate::update::format_timestamp(200));
    }

    #[test]
    fn nowhere_to_dial_is_quiet() {
        let mut c = Connectivity::new(0);
        assert_eq!(c.offline(1, Duration::from_secs(1), false, 15), None);
        assert_eq!(c.offline(2, Duration::from_secs(2), false, 16), None);
        let json = c.json(16);
        assert_eq!(json["detail"], "no known peer to dial");
        assert!(json.get("attempt").is_none());
        assert_eq!(c.online(1, 20), None);
    }

    #[test]
    fn waits_read_well() {
        assert_eq!(format_wait(Duration::from_millis(400)), "1s");
        assert_eq!(format_wait(Duration::from_secs(8)), "8s");
        assert_eq!(format_wait(Duration::from_secs(120)), "2m");
        assert_eq!(format_wait(Duration::from_secs(150)), "2m30s");
    }
}
