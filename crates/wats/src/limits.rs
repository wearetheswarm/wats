//! Per-peer rate limits on the request/response protocol (`/wats`): token
//! buckets per peer and request type, and a cap on the blobs one peer can
//! make us fetch at once. A first robustness step before strangers run
//! nodes; gossip has its own limits in Iroh.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use tokio::sync::Semaphore;

/// Limits per peer, from `[limits]` in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub hello_per_min: u32,
    /// A sync page is up to 256 events: 600 pages a minute lets a new node
    /// catch up on a million events in about 7 minutes.
    pub sync_per_min: u32,
    pub replicate_per_min: u32,
    pub report_per_min: u32,
    /// Blobs one peer can make us fetch at the same time (replication).
    pub blob_fetches_per_peer: usize,
    /// Answer with a proof-of-work challenge (`crate::challenge`) instead
    /// of refusing a peer over its rate limit, and challenge peers without
    /// a reputation while overloaded. Only peers that advertise the
    /// `challenge` feature are ever challenged.
    pub challenge: bool,
    /// Effort asked when the load starts (leading zero bits).
    pub challenge_base_bits: u32,
    /// Highest effort asked while the overload lasts.
    pub challenge_max_bits: u32,
    /// The node is overloaded above this many `/wats` requests in flight.
    pub overload_in_flight: usize,
    /// Requests let through on a solved challenge, all peers together, per
    /// minute: solving puzzles never exceeds this ceiling (0: unlimited).
    pub solved_per_min: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            hello_per_min: 60,
            sync_per_min: 600,
            replicate_per_min: 120,
            report_per_min: 10,
            blob_fetches_per_peer: 4,
            challenge: true,
            challenge_base_bits: 8,
            challenge_max_bits: 24,
            overload_in_flight: 64,
            solved_per_min: 1200,
        }
    }
}

/// The request types that are limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Hello,
    Sync,
    Replicate,
    Report,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Hello => "hello",
            Kind::Sync => "sync",
            Kind::Replicate => "replicate",
            Kind::Report => "crash_report",
        }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Refill `bucket` at `per_min / 60` tokens a second, up to a burst of a
/// quarter of a minute's worth, and take one token if there is one.
fn take(bucket: &mut Bucket, per_min: u32, now: Instant) -> bool {
    let burst = f64::from((per_min / 4).max(1));
    let rate = f64::from(per_min) / 60.0;
    bucket.tokens =
        (bucket.tokens + now.duration_since(bucket.last).as_secs_f64() * rate).min(burst);
    bucket.last = now;
    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        true
    } else {
        false
    }
}

/// Buckets, refilled continuously: `per_min / 60` tokens a second, up to a
/// burst of a quarter of a minute's worth (at least one).
pub struct RateLimiter {
    limits: Limits,
    buckets: Mutex<HashMap<(EndpointId, Kind), Bucket>>,
    logged: Mutex<HashMap<(EndpointId, Kind), Instant>>,
    /// Requests let through on a solved challenge, all peers together.
    solved: Mutex<Option<Bucket>>,
    fetches: Mutex<HashMap<EndpointId, Arc<Semaphore>>>,
}

impl RateLimiter {
    pub fn new(limits: Limits) -> Self {
        RateLimiter {
            limits,
            buckets: Default::default(),
            logged: Default::default(),
            solved: Default::default(),
            fetches: Default::default(),
        }
    }

    fn per_min(&self, kind: Kind) -> u32 {
        match kind {
            Kind::Hello => self.limits.hello_per_min,
            Kind::Sync => self.limits.sync_per_min,
            Kind::Replicate => self.limits.replicate_per_min,
            Kind::Report => self.limits.report_per_min,
        }
    }

    /// Whether `peer` may make a request of this kind now. Logs a throttle
    /// at most once a minute per peer and kind.
    pub fn allow(&self, peer: EndpointId, kind: Kind) -> bool {
        let per_min = self.per_min(kind);
        if per_min == 0 {
            return true; // 0 means unlimited
        }
        let burst = f64::from((per_min / 4).max(1));
        let now = Instant::now();
        let allowed = {
            let Ok(mut buckets) = self.buckets.lock() else {
                return true;
            };
            if buckets.len() > 10_000 {
                // Forget idle peers.
                buckets.retain(|_, b| now.duration_since(b.last) < Duration::from_secs(600));
            }
            let bucket = buckets.entry((peer, kind)).or_insert(Bucket {
                tokens: burst,
                last: now,
            });
            take(bucket, per_min, now)
        };
        if !allowed && let Ok(mut logged) = self.logged.lock() {
            let last = logged.get(&(peer, kind)).copied();
            if last.is_none_or(|t| now.duration_since(t) > Duration::from_secs(60)) {
                logged.insert((peer, kind), now);
                tracing::warn!(
                    peer = %peer.fmt_short(),
                    "peer throttled: more than {per_min} {} requests a minute",
                    kind.as_str()
                );
            }
        }
        allowed
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Whether one more request may go through on a solved challenge,
    /// under the global `solved_per_min` ceiling.
    pub fn allow_solved(&self) -> bool {
        let per_min = self.limits.solved_per_min;
        if per_min == 0 {
            return true;
        }
        let Ok(mut bucket) = self.solved.lock() else {
            return false;
        };
        take(
            bucket.get_or_insert(Bucket {
                tokens: f64::from((per_min / 4).max(1)),
                last: Instant::now(),
            }),
            per_min,
            Instant::now(),
        )
    }

    /// A permit to fetch one blob for `peer`, if it has fewer than
    /// `blob_fetches_per_peer` fetches running.
    pub fn blob_fetch(&self, peer: EndpointId) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = {
            let mut fetches = self.fetches.lock().ok()?;
            Arc::clone(
                fetches
                    .entry(peer)
                    .or_insert_with(|| Arc::new(Semaphore::new(self.limits.blob_fetches_per_peer))),
            )
        };
        let permit = semaphore.try_acquire_owned().ok();
        if permit.is_none() {
            tracing::warn!(
                peer = %peer.fmt_short(),
                "peer throttled: {} blob fetches already running for it",
                self.limits.blob_fetches_per_peer
            );
        }
        permit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn buckets_per_peer_and_kind() {
        let limiter = RateLimiter::new(Limits {
            sync_per_min: 8, // burst 2
            ..Limits::default()
        });
        let (a, b) = (peer(1), peer(2));
        assert!(limiter.allow(a, Kind::Sync));
        assert!(limiter.allow(a, Kind::Sync));
        assert!(!limiter.allow(a, Kind::Sync), "burst spent");
        assert!(
            limiter.allow(b, Kind::Sync),
            "another peer has its own bucket"
        );
        assert!(limiter.allow(a, Kind::Hello), "another kind too");
        let unlimited = RateLimiter::new(Limits {
            sync_per_min: 0,
            ..Limits::default()
        });
        assert!((0..1000).all(|_| unlimited.allow(a, Kind::Sync)));
    }

    #[test]
    fn solved_requests_have_a_global_ceiling() {
        let limiter = RateLimiter::new(Limits {
            solved_per_min: 8, // burst 2
            ..Limits::default()
        });
        assert!(limiter.allow_solved());
        assert!(limiter.allow_solved());
        assert!(!limiter.allow_solved(), "ceiling reached");
    }

    #[tokio::test]
    async fn blob_fetches_are_capped() {
        let limiter = RateLimiter::new(Limits {
            blob_fetches_per_peer: 2,
            ..Limits::default()
        });
        let a = peer(1);
        let one = limiter.blob_fetch(a).unwrap();
        let _two = limiter.blob_fetch(a).unwrap();
        assert!(limiter.blob_fetch(a).is_none());
        assert!(limiter.blob_fetch(peer(2)).is_some());
        drop(one);
        assert!(
            limiter.blob_fetch(a).is_some(),
            "a finished fetch frees a slot"
        );
    }
}
