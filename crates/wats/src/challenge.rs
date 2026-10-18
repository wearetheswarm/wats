//! Load challenge on `/wats` (ADR 0014, "Load challenge" addendum): when a
//! node is overloaded, or a peer is over its rate limit, the node answers
//! `Challenge { alg, seed, effort, expires }` instead of serving or
//! refusing, and the requester resubmits the same request with a
//! `solution` field.
//!
//! - **Algorithm ids.** A challenge names its algorithm. The only one is
//!   [`ALG_B3`], a BLAKE3 hashcash; a memory-hard puzzle can be added later
//!   under a new id. A requester that does not know the id gives up on that
//!   request, as for `Refused`.
//! - **Stateless seeds.** A seed is `expires (8 bytes, big endian) ||
//!   effort (1) || random (7) || MAC (16)`, the MAC being a BLAKE3 keyed
//!   hash, under a per-process random key, of the first 16 bytes and the
//!   requester id. Nothing is stored per challenge; a bounded set of used
//!   seeds rejects replays (see [`Challenger::verify`]).
//! - **Adaptive effort** (after Tor proposal 327): the effort asked rises
//!   by one bit per interval while the node is under pressure, and decays
//!   by one bit per interval when it is not, between a base and a cap.

// Network input must never panic the node (security review, item D).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// The `hello` feature of nodes that can solve challenges. Nodes never send
/// a challenge to a peer that did not advertise it.
pub const FEATURE: &str = "challenge";
/// BLAKE3 hashcash on a fresh seed.
pub const ALG_B3: &str = "b3";
/// Key derivation context of [`ALG_B3`].
const B3_CONTEXT: &str = "wats challenge v1";
/// How long a seed stays valid.
pub const SEED_TTL: Duration = Duration::from_secs(60);
/// Highest effort a requester agrees to solve (about 16 million hashes).
pub const SOLVE_MAX_BITS: u32 = 24;
/// Highest effort a node can ask (it is carried in one byte of the seed and
/// must stay solvable).
pub const MAX_BITS: u32 = 32;
/// Longest nonce accepted: a decimal u64.
const MAX_NONCE_LEN: usize = 20;
/// Used seeds remembered against replay.
const USED_SEEDS_MAX: usize = 4096;
/// The effort moves by one bit per interval.
pub const EFFORT_INTERVAL: Duration = Duration::from_secs(10);
/// A peer met (`hello`) this long ago is not challenged for global load.
pub const REPUTATION_AFTER: Duration = Duration::from_secs(600);
/// Peers whose first `hello` time is remembered.
const MET_MAX: usize = 10_000;

/// The solution a requester sends along with a request it resubmits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    pub alg: String,
    /// The seed of the challenge, as it was received.
    pub seed: String,
    /// Decimal digits.
    pub nonce: String,
}

/// The [`ALG_B3`] digest: `BLAKE3-derive_key("wats challenge v1", seed ||
/// requester id || responder id || nonce)`.
pub fn b3_digest(
    seed: &[u8; 32],
    requester: &EndpointId,
    responder: &EndpointId,
    nonce: &str,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(B3_CONTEXT);
    hasher.update(seed);
    hasher.update(requester.as_bytes());
    hasher.update(responder.as_bytes());
    hasher.update(nonce.as_bytes());
    *hasher.finalize().as_bytes()
}

fn valid_nonce(nonce: &str) -> bool {
    !nonce.is_empty() && nonce.len() <= MAX_NONCE_LEN && nonce.bytes().all(|b| b.is_ascii_digit())
}

/// Solve a challenge (CPU-bound: run it on a blocking thread). `None` for
/// an unknown algorithm, a malformed seed, or an effort above
/// [`SOLVE_MAX_BITS`].
pub fn solve(
    alg: &str,
    seed: &str,
    effort: u32,
    requester: &EndpointId,
    responder: &EndpointId,
) -> Option<Solution> {
    if alg != ALG_B3 || effort > SOLVE_MAX_BITS {
        return None;
    }
    let bytes = crate::event::parse_hex::<32>(seed, "seed").ok()?;
    let mut prefix = blake3::Hasher::new_derive_key(B3_CONTEXT);
    prefix.update(&bytes);
    prefix.update(requester.as_bytes());
    prefix.update(responder.as_bytes());
    // Expected 2^effort tries; giving up after 2^(effort + 8) is
    // vanishingly rare.
    let tries = 1u64 << effort.saturating_add(8).min(63);
    for n in 0..tries {
        let nonce = n.to_string();
        let mut hasher = prefix.clone();
        hasher.update(nonce.as_bytes());
        if crate::event::leading_zero_bits(hasher.finalize().as_bytes()) >= effort {
            return Some(Solution {
                alg: ALG_B3.into(),
                seed: seed.into(),
                nonce,
            });
        }
    }
    None
}

/// A challenge to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub alg: String,
    pub seed: String,
    pub effort: u32,
    /// Unix seconds.
    pub expires: u64,
}

/// Why a solution was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    UnknownAlg,
    Malformed,
    NotOurs,
    Expired,
    TooLittleWork,
    Replayed,
}

struct Effort {
    bits: u32,
    /// Start of the current interval.
    since: Instant,
    /// Whether the node was under pressure during the current interval.
    pressure: bool,
}

#[derive(Default)]
struct Used {
    /// `(expires, first 16 bytes of the seed)`.
    seeds: BTreeSet<(u64, [u8; 16])>,
    /// Seeds expiring at or before this are refused: the set forgot some.
    watermark: u64,
}

/// Issues and checks challenges for one node.
pub struct Challenger {
    key: [u8; 32],
    base: u32,
    max: u32,
    effort: Mutex<Effort>,
    used: Mutex<Used>,
    /// Peers whose first `hello` we saw, and when.
    met: Mutex<HashMap<EndpointId, Instant>>,
    /// Peers from the persisted peer cache.
    known: HashSet<EndpointId>,
    issued: AtomicU64,
    solved: AtomicU64,
}

impl std::fmt::Debug for Challenger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Challenger")
            .field("base", &self.base)
            .field("max", &self.max)
            .finish_non_exhaustive()
    }
}

impl Challenger {
    /// `base` and `max` are clamped to `1..=MAX_BITS`, `max >= base`.
    /// `known`: peers with an established reputation (the peer cache).
    pub fn new(base: u32, max: u32, known: HashSet<EndpointId>) -> Self {
        let base = base.clamp(1, MAX_BITS);
        let max = max.clamp(base, MAX_BITS);
        let mut key = [0u8; 32];
        // Without randomness, fall back to a key from the clock and the
        // address of a local: seeds stay unforgeable enough for a puzzle.
        if getrandom::fill(&mut key).is_err() {
            let fallback = format!("{:?}{:p}", std::time::SystemTime::now(), &key);
            key = *blake3::hash(fallback.as_bytes()).as_bytes();
        }
        Challenger {
            key,
            base,
            max,
            effort: Mutex::new(Effort {
                bits: base,
                since: Instant::now(),
                pressure: false,
            }),
            used: Default::default(),
            met: Default::default(),
            known,
            issued: AtomicU64::new(0),
            solved: AtomicU64::new(0),
        }
    }

    /// Record whether the node is under pressure now, and return the
    /// current effort.
    pub fn note(&self, pressure: bool) -> u32 {
        self.note_at(pressure, Instant::now())
    }

    fn note_at(&self, pressure: bool, now: Instant) -> u32 {
        let Ok(mut e) = self.effort.lock() else {
            return self.base;
        };
        let interval = EFFORT_INTERVAL.as_secs().max(1);
        let elapsed = now
            .saturating_duration_since(e.since)
            .as_secs()
            .checked_div(interval)
            .unwrap_or(0);
        if elapsed >= 1 {
            // The interval that ended moves the effort one way or the
            // other; each further interval without requests decays it.
            e.bits = if e.pressure {
                e.bits.saturating_add(1).min(self.max)
            } else {
                e.bits.saturating_sub(1).max(self.base)
            };
            let idle = u32::try_from(elapsed.saturating_sub(1)).unwrap_or(u32::MAX);
            e.bits = e.bits.saturating_sub(idle).max(self.base);
            e.since = now;
            e.pressure = false;
        }
        e.pressure |= pressure;
        e.bits
    }

    fn mac(&self, head: &[u8; 16], requester: &EndpointId) -> [u8; 16] {
        let mut hasher = blake3::Hasher::new_keyed(&self.key);
        hasher.update(b"wats challenge seed v1");
        hasher.update(head);
        hasher.update(requester.as_bytes());
        let mut out = [0u8; 16];
        hasher.finalize_xof().fill(&mut out);
        out
    }

    /// A fresh challenge for `requester` at the given effort.
    pub fn issue(&self, requester: &EndpointId, effort: u32, now_secs: u64) -> Issued {
        let effort = effort.clamp(1, MAX_BITS);
        let expires = now_secs.saturating_add(SEED_TTL.as_secs());
        let mut head = [0u8; 16];
        let (time, rest) = head.split_at_mut(8);
        time.copy_from_slice(&expires.to_be_bytes());
        let (bits, random) = rest.split_at_mut(1);
        bits.copy_from_slice(&[u8::try_from(effort).unwrap_or(u8::MAX)]);
        if getrandom::fill(random).is_err() {
            let n = self.issued.load(Ordering::Relaxed).to_be_bytes();
            for (r, b) in random.iter_mut().zip(n.iter().skip(1)) {
                *r = *b;
            }
        }
        let mut seed = [0u8; 32];
        let (a, b) = seed.split_at_mut(16);
        a.copy_from_slice(&head);
        b.copy_from_slice(&self.mac(&head, requester));
        self.issued.fetch_add(1, Ordering::Relaxed);
        Issued {
            alg: ALG_B3.into(),
            seed: hex::encode(seed),
            effort,
            expires,
        }
    }

    /// Check a solution sent by `requester` to us (`responder`). A valid
    /// solution is used up.
    pub fn verify(
        &self,
        solution: &Solution,
        requester: &EndpointId,
        responder: &EndpointId,
        now_secs: u64,
    ) -> Result<(), Rejected> {
        if solution.alg != ALG_B3 {
            return Err(Rejected::UnknownAlg);
        }
        let seed = crate::event::parse_hex::<32>(&solution.seed, "seed")
            .map_err(|_| Rejected::Malformed)?;
        if !valid_nonce(&solution.nonce) {
            return Err(Rejected::Malformed);
        }
        let (head_bytes, mac) = seed.split_at(16);
        let mut head = [0u8; 16];
        head.copy_from_slice(head_bytes);
        let expected = self.mac(&head, requester);
        // Constant time comparison.
        if expected
            .iter()
            .zip(mac)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
        {
            return Err(Rejected::NotOurs);
        }
        let mut time = [0u8; 8];
        time.copy_from_slice(head.get(..8).ok_or(Rejected::Malformed)?);
        let expires = u64::from_be_bytes(time);
        let effort = u32::from(*head.get(8).ok_or(Rejected::Malformed)?);
        if expires < now_secs {
            return Err(Rejected::Expired);
        }
        let digest = b3_digest(&seed, requester, responder, &solution.nonce);
        if crate::event::leading_zero_bits(&digest) < effort {
            return Err(Rejected::TooLittleWork);
        }
        let Ok(mut used) = self.used.lock() else {
            return Err(Rejected::Replayed);
        };
        if expires <= used.watermark || !used.seeds.insert((expires, head)) {
            return Err(Rejected::Replayed);
        }
        // Forget expired seeds; past the cap, forget the oldest and refuse
        // every seed that expires no later (the requester is challenged
        // again).
        while let Some(&(oldest, _)) = used.seeds.first() {
            if oldest >= now_secs && used.seeds.len() <= USED_SEEDS_MAX {
                break;
            }
            used.seeds.pop_first();
            used.watermark = used.watermark.max(oldest);
        }
        self.solved.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Remember that `peer` completed `hello` (the first time counts).
    pub fn met(&self, peer: EndpointId) {
        if let Ok(mut met) = self.met.lock() {
            if met.len() >= MET_MAX && !met.contains_key(&peer) {
                let now = Instant::now();
                met.retain(|_, t| now.saturating_duration_since(*t) >= REPUTATION_AFTER);
                if met.len() >= MET_MAX {
                    return;
                }
            }
            met.entry(peer).or_insert_with(Instant::now);
        }
    }

    /// Whether `peer` has an established reputation: it is in the peer
    /// cache, or it completed `hello` with us [`REPUTATION_AFTER`] ago.
    pub fn reputable(&self, peer: &EndpointId) -> bool {
        self.known.contains(peer)
            || self.met.lock().is_ok_and(|met| {
                met.get(peer)
                    .is_some_and(|t| t.elapsed() >= REPUTATION_AFTER)
            })
    }

    /// Challenges issued and solutions accepted so far.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.issued.load(Ordering::Relaxed),
            self.solved.load(Ordering::Relaxed),
        )
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
    use super::*;

    fn peer(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn challenger() -> Challenger {
        Challenger::new(8, 24, HashSet::new())
    }

    #[test]
    fn solutions_verify_once_and_are_bound_to_both_ids() {
        let c = challenger();
        let (alice, bob, us) = (peer(1), peer(2), peer(9));
        let now = 1_000_000;
        let issued = c.issue(&alice, 8, now);
        assert_eq!(issued.alg, ALG_B3);
        assert_eq!(issued.expires, now + 60);
        let solution = solve(&issued.alg, &issued.seed, issued.effort, &alice, &us).unwrap();
        let seed = crate::event::parse_hex::<32>(&issued.seed, "seed").unwrap();
        assert!(
            crate::event::leading_zero_bits(&b3_digest(&seed, &alice, &us, &solution.nonce)) >= 8
        );
        // Another peer cannot use it, nor can it be used at another node.
        assert_eq!(c.verify(&solution, &bob, &us, now), Err(Rejected::NotOurs));
        assert_ne!(
            b3_digest(&seed, &alice, &us, &solution.nonce),
            b3_digest(&seed, &alice, &bob, &solution.nonce)
        );
        assert_eq!(c.verify(&solution, &alice, &us, now), Ok(()));
        assert_eq!(
            c.verify(&solution, &alice, &us, now),
            Err(Rejected::Replayed)
        );
        assert_eq!(c.stats(), (1, 1));
    }

    #[test]
    fn expired_forged_and_lazy_solutions_are_rejected() {
        let c = challenger();
        let (alice, us) = (peer(1), peer(9));
        let issued = c.issue(&alice, 12, 100);
        let solution = solve(ALG_B3, &issued.seed, 12, &alice, &us).unwrap();
        assert_eq!(
            c.verify(&solution, &alice, &us, 161),
            Err(Rejected::Expired)
        );
        // Lowering the effort byte breaks the MAC.
        let mut seed = crate::event::parse_hex::<32>(&issued.seed, "seed").unwrap();
        seed[8] = 0;
        let forged = Solution {
            seed: hex::encode(seed),
            ..solution.clone()
        };
        assert_eq!(c.verify(&forged, &alice, &us, 100), Err(Rejected::NotOurs));
        // A nonce that does not reach the effort.
        let lazy = (0u64..)
            .map(|n| n.to_string())
            .find(|n| {
                let s = crate::event::parse_hex::<32>(&issued.seed, "seed").unwrap();
                crate::event::leading_zero_bits(&b3_digest(&s, &alice, &us, n)) < 12
            })
            .unwrap();
        let lazy = Solution {
            nonce: lazy,
            ..solution.clone()
        };
        assert_eq!(
            c.verify(&lazy, &alice, &us, 100),
            Err(Rejected::TooLittleWork)
        );
        for nonce in ["", "12a", "123456789012345678901", "-1"] {
            let bad = Solution {
                nonce: nonce.into(),
                ..solution.clone()
            };
            assert_eq!(c.verify(&bad, &alice, &us, 100), Err(Rejected::Malformed));
        }
        assert_eq!(c.verify(&solution, &alice, &us, 160), Ok(()));
        // Another process (another key) does not accept it.
        assert_eq!(
            challenger().verify(&solution, &alice, &us, 100),
            Err(Rejected::NotOurs)
        );
    }

    #[test]
    fn unknown_algorithms_are_given_up_and_refused() {
        let (alice, us) = (peer(1), peer(9));
        let c = challenger();
        let issued = c.issue(&alice, 8, 0);
        assert!(solve("equix", &issued.seed, 8, &alice, &us).is_none());
        assert!(solve("_grease_1a2b", &issued.seed, 8, &alice, &us).is_none());
        assert!(solve(ALG_B3, "zz", 8, &alice, &us).is_none());
        assert!(
            solve(ALG_B3, &issued.seed, SOLVE_MAX_BITS + 1, &alice, &us).is_none(),
            "too much work is refused"
        );
        let mut solution = solve(ALG_B3, &issued.seed, 8, &alice, &us).unwrap();
        solution.alg = "m1".into();
        assert_eq!(
            c.verify(&solution, &alice, &us, 0),
            Err(Rejected::UnknownAlg)
        );
    }

    #[test]
    fn the_replay_set_is_bounded() {
        let c = Challenger::new(1, 1, HashSet::new());
        let (alice, us) = (peer(1), peer(9));
        let first = c.issue(&alice, 1, 0);
        let first = solve(ALG_B3, &first.seed, 1, &alice, &us).unwrap();
        for i in 0..USED_SEEDS_MAX as u64 + 10 {
            let issued = c.issue(&alice, 1, i);
            let s = solve(ALG_B3, &issued.seed, 1, &alice, &us).unwrap();
            assert_eq!(c.verify(&s, &alice, &us, 0), Ok(()));
        }
        assert!(c.used.lock().unwrap().seeds.len() <= USED_SEEDS_MAX);
        // Forgotten, but still refused: it expires before the watermark.
        assert_eq!(c.verify(&first, &alice, &us, 0), Err(Rejected::Replayed));
    }

    #[test]
    fn effort_rises_under_pressure_and_decays() {
        let c = Challenger::new(8, 10, HashSet::new());
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(c.note_at(true, at(1)), 8);
        assert_eq!(c.note_at(true, at(11)), 9);
        assert_eq!(c.note_at(true, at(21)), 10);
        assert_eq!(c.note_at(true, at(31)), 10, "capped");
        assert_eq!(c.note_at(false, at(41)), 10, "the last interval was busy");
        assert_eq!(c.note_at(false, at(51)), 9);
        assert_eq!(c.note_at(false, at(200)), 8, "never below the base");
    }

    #[test]
    fn reputation_comes_from_the_cache_or_time() {
        let (old, new) = (peer(1), peer(2));
        let c = Challenger::new(8, 24, HashSet::from([old]));
        c.met(new);
        assert!(c.reputable(&old));
        assert!(!c.reputable(&new), "met just now");
        if let Some(t) = Instant::now().checked_sub(REPUTATION_AFTER) {
            c.met.lock().unwrap().insert(new, t);
            assert!(c.reputable(&new));
        }
    }
}
