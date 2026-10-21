//! Per-peer rate limits on the request/response protocol (`/wats`): token
//! buckets per peer and request type, and a cap on the blobs one peer can
//! make us fetch at once, with a short queue behind it. A first robustness
//! step before strangers run nodes; gossip has its own limits in Iroh.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How long a blob fetch a peer asked for waits for one of its slots
/// (`blob_fetches_per_peer`) before it is skipped. With the 60 s a fetch
/// from that peer may take, the answer still comes within the 150 s a
/// publisher waits for it.
pub const BLOB_FETCH_WAIT: Duration = Duration::from_secs(60);

/// Limits per peer, from `[limits]` in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub hello_per_min: u32,
    /// A sync page is up to 256 events: 600 pages a minute lets a new node
    /// catch up on a million events in about 7 minutes.
    pub sync_per_min: u32,
    pub replicate_per_min: u32,
    pub report_per_min: u32,
    /// `get` requests (ADR 0025 §3): history fetched by id, on demand.
    pub get_per_min: u32,
    /// Blobs one peer can make us fetch at the same time (replication).
    pub blob_fetches_per_peer: usize,
    /// Blob fetches of one peer that wait for a slot (up to
    /// [`BLOB_FETCH_WAIT`]) when all its slots are taken; past these, a
    /// fetch is skipped at once.
    pub blob_fetch_queue: usize,
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
            get_per_min: 120,
            blob_fetches_per_peer: 4,
            blob_fetch_queue: 16,
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
    Get,
    /// `get` of whole threads: a quarter of `get_per_min`.
    GetThread,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Hello => "hello",
            Kind::Sync => "sync",
            Kind::Replicate => "replicate",
            Kind::Report => "crash_report",
            Kind::Get => "get",
            Kind::GetThread => "get_thread",
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
    fetches: PeerSlots,
}

impl RateLimiter {
    pub fn new(limits: Limits) -> Self {
        RateLimiter {
            limits,
            buckets: Default::default(),
            logged: Default::default(),
            solved: Default::default(),
            fetches: PeerSlots::new(limits.blob_fetches_per_peer, limits.blob_fetch_queue),
        }
    }

    fn per_min(&self, kind: Kind) -> u32 {
        match kind {
            Kind::Hello => self.limits.hello_per_min,
            Kind::Sync => self.limits.sync_per_min,
            Kind::Replicate => self.limits.replicate_per_min,
            Kind::Report => self.limits.report_per_min,
            Kind::Get => self.limits.get_per_min,
            Kind::GetThread => match self.limits.get_per_min {
                0 => 0,
                n => (n / 4).max(1),
            },
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

    /// A permit to fetch one blob for `peer`: at once if it has fewer than
    /// `blob_fetches_per_peer` fetches running, else once one ends, if
    /// fewer than `blob_fetch_queue` already wait and within `wait`.
    pub async fn blob_fetch(
        &self,
        peer: EndpointId,
        wait: Duration,
    ) -> Option<OwnedSemaphorePermit> {
        match self.fetches.acquire(peer, wait).await {
            Ok(permit) => Some(permit),
            Err(why) => {
                let (running, queued) = (
                    self.limits.blob_fetches_per_peer,
                    self.limits.blob_fetch_queue,
                );
                match why {
                    NoSlot::Full => tracing::warn!(
                        peer = %peer.fmt_short(),
                        "peer throttled: {running} blob fetches already running for it, {queued} waiting"
                    ),
                    NoSlot::TimedOut => tracing::warn!(
                        peer = %peer.fmt_short(),
                        "peer throttled: a blob fetch waited {}s for one of its {running} slots",
                        wait.as_secs()
                    ),
                }
                None
            }
        }
    }
}

/// Why [`PeerSlots::acquire`] gave no slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSlot {
    /// Every slot taken and the queue full.
    Full,
    /// Queued, but no slot freed in time.
    TimedOut,
}

/// At most `slots` holders at once per peer, and at most `queue` more
/// waiting for a slot, first come first served. Bounded both ways: a
/// peer cannot make us run, nor keep waiting, more than that.
pub struct PeerSlots {
    slots: usize,
    queue: usize,
    peers: Mutex<HashMap<EndpointId, Arc<Gate>>>,
}

struct Gate {
    semaphore: Arc<Semaphore>,
    waiting: AtomicUsize,
}

/// A place in a [`Gate`]'s queue, given back when dropped (the wait ended
/// or was cancelled).
struct Place<'a> {
    waiting: &'a AtomicUsize,
    /// Callers that were waiting already.
    ahead: usize,
}

impl<'a> Place<'a> {
    fn take(waiting: &'a AtomicUsize) -> Self {
        let ahead = waiting.fetch_add(1, Ordering::SeqCst);
        Place { waiting, ahead }
    }
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PeerSlots {
    pub fn new(slots: usize, queue: usize) -> Self {
        PeerSlots {
            slots,
            queue,
            peers: Default::default(),
        }
    }

    fn gate(&self, peer: EndpointId) -> Option<Arc<Gate>> {
        let mut peers = self.peers.lock().ok()?;
        if peers.len() >= 1_000 && !peers.contains_key(&peer) {
            // Forget idle peers: nobody waits for their slots nor holds one.
            peers.retain(|_, gate| {
                Arc::strong_count(gate) > 1 || gate.semaphore.available_permits() < self.slots
            });
        }
        let gate = peers.entry(peer).or_insert_with(|| {
            Arc::new(Gate {
                semaphore: Arc::new(Semaphore::new(self.slots)),
                waiting: AtomicUsize::new(0),
            })
        });
        Some(Arc::clone(gate))
    }

    /// A slot for `peer`, held until the permit is dropped: at once if one
    /// is free, else after waiting at most `wait` in a queue of at most
    /// `queue` callers.
    pub async fn acquire(
        &self,
        peer: EndpointId,
        wait: Duration,
    ) -> Result<OwnedSemaphorePermit, NoSlot> {
        let gate = self.gate(peer).ok_or(NoSlot::Full)?;
        if let Ok(permit) = Arc::clone(&gate.semaphore).try_acquire_owned() {
            return Ok(permit);
        }
        // Taken first, then checked: callers racing for the last place may
        // all be refused, never all let in.
        let place = Place::take(&gate.waiting);
        if place.ahead >= self.queue {
            return Err(NoSlot::Full);
        }
        match tokio::time::timeout(wait, Arc::clone(&gate.semaphore).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(NoSlot::TimedOut),
        }
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
            blob_fetch_queue: 0,
            ..Limits::default()
        });
        let a = peer(1);
        let wait = Duration::from_secs(5);
        let one = limiter.blob_fetch(a, wait).await.unwrap();
        let _two = limiter.blob_fetch(a, wait).await.unwrap();
        assert!(limiter.blob_fetch(a, wait).await.is_none(), "no queue");
        assert!(limiter.blob_fetch(peer(2), wait).await.is_some());
        drop(one);
        assert!(
            limiter.blob_fetch(a, wait).await.is_some(),
            "a finished fetch frees a slot"
        );
    }

    #[tokio::test]
    async fn blob_fetches_queue_behind_the_cap() {
        let slots = Arc::new(PeerSlots::new(1, 2));
        let a = peer(1);
        let long = Duration::from_secs(30);
        let first = slots.acquire(a, long).await.unwrap();
        // Two wait their turn; a third finds the queue full at once.
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let slots = Arc::clone(&slots);
                tokio::spawn(async move { slots.acquire(a, long).await.map(drop) })
            })
            .collect();
        while slots.gate(a).unwrap().waiting.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(slots.acquire(a, long).await.err(), Some(NoSlot::Full));
        assert!(slots.acquire(peer(2), long).await.is_ok(), "per peer");
        // Freeing the slot lets both through, one after the other.
        drop(first);
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap(), Ok(()));
        }
        // A queued caller gives up after its wait; its place is given back.
        let _held = slots.acquire(a, long).await.unwrap();
        let short = Duration::from_millis(50);
        assert_eq!(slots.acquire(a, short).await.err(), Some(NoSlot::TimedOut));
        assert_eq!(slots.gate(a).unwrap().waiting.load(Ordering::SeqCst), 0);
    }
}
