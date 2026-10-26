//! Laptop sleep, and the brief view's connectivity and infra health lines
//! (ADR 0052).
//!
//! A laptop that sleeps loses its peers, its relay and its published
//! record, and the full log says so at every wake, DarkWakes included.
//! The infra is fine; a person watching `wats attach` should not read it
//! as broken. So the brief lines about connectivity and health come from
//! [`Notices`], which knows about sleep:
//! - **Sleep** is a gap between the wall clock and the monotonic clock,
//!   which does not count sleep on macOS or Linux, or a tick far too late
//!   where it does ([`slept`]). Sleeps in a row (DarkWakes) are one sleep
//!   until the node has been awake [`SETTLE`] with a peer.
//! - **Across a sleep**, no offline or health line. Once back with a peer:
//!   `back online after sleep (6h02m)`. Still no peer [`AWAKE_GRACE`]
//!   after waking: one offline line.
//! - **Outside a sleep**, one offline line once offline for
//!   [`OFFLINE_GRACE`], one `back online after 3m` line, none per attempt.
//!   A health issue shows once it lasted [`AWAKE_GRACE`] awake.
//!
//! `node.log` keeps every line it had: the `wats::reconnect` and
//! `wats::health` warnings are only left out of the brief view.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;
use std::sync::Weak;
use std::time::{Duration, Instant, SystemTime};

use tracing::info;

use crate::health::What;
use crate::node::Node;

/// How often the node compares its clocks.
pub const TICK: Duration = Duration::from_secs(5);
/// A gap this long between the clocks is a sleep.
pub const SLEEP_GAP: Duration = Duration::from_secs(20);
/// Offline this long, awake and outside a sleep, before the brief view
/// says so.
pub const OFFLINE_GRACE: Duration = Duration::from_secs(30);
/// Awake this long after a sleep with no peer, or with a health issue,
/// before the brief view says so.
pub const AWAKE_GRACE: Duration = Duration::from_secs(2 * 60);
/// Awake with a peer this long, with no change, ends a sleep: a DarkWake
/// is shorter.
pub const SETTLE: Duration = Duration::from_secs(60);

/// How long the machine slept between two ticks `every` apart, which took
/// `wall` on the wall clock and `mono` on the monotonic one.
pub fn slept(wall: Duration, mono: Duration, every: Duration) -> Option<Duration> {
    // The monotonic clock stops during sleep (macOS, Linux): the wall
    // clock went further.
    let gap = wall.saturating_sub(mono);
    // Where it counts sleep: the tick came far too late.
    let late = mono.saturating_sub(every);
    let slept = gap.max(late);
    (slept >= SLEEP_GAP).then_some(slept)
}

/// "45s", "3m", "6h02m".
pub fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

#[derive(Debug, Clone)]
struct Offline {
    wall: u64,
    mono: Instant,
    shown: bool,
}

#[derive(Debug, Clone)]
struct Sleep {
    /// When the first sleep began (unix seconds).
    from: u64,
    /// The last wake.
    woke: Instant,
    /// The last wake or connectivity change.
    changed: Instant,
    /// When a peer was back last (unix seconds).
    back: Option<u64>,
    /// The node lost its peers during it.
    was_offline: bool,
}

#[derive(Debug, Clone)]
struct Issue {
    detail: String,
    raised: Instant,
    shown: bool,
}

/// What the brief view says about connectivity and health (see the
/// module documentation). Every method takes the times, and returns the
/// lines to log.
#[derive(Debug, Clone)]
pub struct Notices {
    online: bool,
    offline: Option<Offline>,
    sleep: Option<Sleep>,
    woke: Option<Instant>,
    issues: BTreeMap<What, Issue>,
}

impl Default for Notices {
    fn default() -> Self {
        Notices {
            online: true,
            offline: None,
            sleep: None,
            woke: None,
            issues: BTreeMap::new(),
        }
    }
}

impl Notices {
    /// The machine slept `secs` until `wall` / `mono`.
    pub fn slept(&mut self, secs: u64, wall: u64, mono: Instant) {
        let was_offline = !self.online;
        let sleep = self.sleep.get_or_insert(Sleep {
            from: wall.saturating_sub(secs),
            woke: mono,
            changed: mono,
            back: None,
            was_offline,
        });
        sleep.woke = mono;
        sleep.changed = mono;
        sleep.back = None;
        sleep.was_offline |= was_offline;
        self.woke = Some(mono);
    }

    /// The node lost its last peer.
    pub fn went_offline(&mut self, wall: u64, mono: Instant) {
        self.online = false;
        if let Some(sleep) = &mut self.sleep {
            sleep.was_offline = true;
            sleep.changed = mono;
        }
        self.offline.get_or_insert(Offline {
            wall,
            mono,
            shown: false,
        });
    }

    /// A peer is back.
    pub fn back_online(&mut self, wall: u64, mono: Instant) -> Vec<String> {
        self.online = true;
        let offline = self.offline.take();
        if let Some(sleep) = &mut self.sleep {
            sleep.changed = mono;
            sleep.back = Some(wall);
            return Vec::new();
        }
        match offline {
            Some(o) if o.shown => vec![format!(
                "back online after {}",
                span(wall.saturating_sub(o.wall))
            )],
            _ => Vec::new(),
        }
    }

    /// A health issue was raised.
    pub fn raised(&mut self, what: What, detail: &str, mono: Instant) {
        self.issues.insert(
            what,
            Issue {
                detail: detail.to_string(),
                raised: mono,
                shown: false,
            },
        );
    }

    /// A health issue changed its description.
    pub fn changed(&mut self, what: What, detail: &str) -> Vec<String> {
        let quiet = self.sleep.is_some();
        match self.issues.get_mut(&what) {
            Some(issue) => {
                issue.detail = detail.to_string();
                if issue.shown && !quiet {
                    vec![format!("⚠ infra health degraded: {detail}")]
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        }
    }

    /// A health check is fine again.
    pub fn recovered(&mut self, what: What) -> Vec<String> {
        match self.issues.remove(&what) {
            Some(issue) if issue.shown => vec![format!("infra health recovered: {what} ok again")],
            _ => Vec::new(),
        }
    }

    /// The lines due at `wall` / `mono`.
    pub fn tick(&mut self, wall: u64, mono: Instant) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(sleep) = &self.sleep {
            let awake = mono.saturating_duration_since(sleep.woke);
            let calm = mono.saturating_duration_since(sleep.changed);
            if self.online && (calm >= SETTLE || awake >= AWAKE_GRACE) {
                if sleep.was_offline {
                    let back = sleep.back.unwrap_or(wall);
                    lines.push(format!(
                        "back online after sleep ({})",
                        span(back.saturating_sub(sleep.from))
                    ));
                }
                self.sleep = None;
                self.offline = None;
            } else if !self.online && awake >= AWAKE_GRACE {
                lines.push("⚠ offline after sleep: no connected peer; reconnecting".into());
                self.sleep = None;
                self.offline = Some(Offline {
                    wall,
                    mono,
                    shown: true,
                });
            } else {
                return lines;
            }
        }
        if let Some(offline) = &mut self.offline
            && !offline.shown
            && mono.saturating_duration_since(offline.mono) >= OFFLINE_GRACE
        {
            offline.shown = true;
            lines.push("⚠ offline: no connected peer; reconnecting".into());
        }
        let woke = self.woke;
        for issue in self.issues.values_mut() {
            let since = woke.map_or(issue.raised, |w| w.max(issue.raised));
            if !issue.shown && mono.saturating_duration_since(since) >= AWAKE_GRACE {
                issue.shown = true;
                lines.push(format!("⚠ infra health degraded: {}", issue.detail));
            }
        }
        lines
    }
}

/// Logs `lines` as brief lines.
pub fn log(lines: Vec<String>) {
    for line in lines {
        info!(target: crate::brief::TARGET, "{line}");
    }
}

/// Compares the clocks every [`TICK`] and logs the lines due, until the
/// node is gone.
pub async fn watch(node: Weak<Node>) {
    let mut wall = SystemTime::now();
    let mut mono = Instant::now();
    loop {
        tokio::time::sleep(TICK).await;
        let (wall_now, mono_now) = (SystemTime::now(), Instant::now());
        let Some(node) = node.upgrade() else { break };
        let wall_took = wall_now.duration_since(wall).unwrap_or_default();
        let mono_took = mono_now.saturating_duration_since(mono);
        (wall, mono) = (wall_now, mono_now);
        let now = crate::node::now_secs();
        let lines = node.notices(|n| {
            if let Some(secs) = slept(wall_took, mono_took, TICK) {
                n.slept(secs.as_secs(), now, mono_now);
            }
            n.tick(now, mono_now)
        });
        log(lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn a_clock_gap_is_a_sleep() {
        assert_eq!(slept(secs(5), secs(5), TICK), None);
        assert_eq!(slept(secs(6), secs(5), TICK), None);
        // macOS / Linux: the monotonic clock stopped.
        assert_eq!(slept(secs(3605), secs(5), TICK), Some(secs(3600)));
        // A clock that counts sleep: the tick is late.
        assert_eq!(slept(secs(3605), secs(3605), TICK), Some(secs(3600)));
        // The wall clock set back: no sleep.
        assert_eq!(slept(secs(0), secs(5), TICK), None);
    }

    /// A night of DarkWakes: one line, on the real return.
    #[test]
    fn a_night_of_sleep_says_one_line() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + secs(s);
        let mut n = Notices::default();
        let night = 1_000_000;
        // Asleep at `night`; a DarkWake 2 h later, offline then back.
        n.slept(7200, night + 7200, at(10));
        n.went_offline(night + 7205, at(15));
        n.raised(What::Lookup, "dns: our record not republished", at(16));
        assert!(n.tick(night + 7220, at(30)).is_empty());
        assert!(n.back_online(night + 7225, at(35)).is_empty());
        assert!(n.tick(night + 7230, at(40)).is_empty());
        // Asleep again before it settled: 6 h in all, then a wake.
        n.slept(14_000, night + 21_600, at(45));
        n.went_offline(night + 21_610, at(55));
        assert!(n.tick(night + 21_620, at(65)).is_empty());
        assert!(n.back_online(night + 21_630, at(75)).is_empty());
        assert!(n.tick(night + 21_660, at(105)).is_empty());
        assert_eq!(
            n.tick(night + 21_690, at(135)),
            vec!["back online after sleep (6h00m)"]
        );
        // The lookup issue cleared within its grace: never shown.
        assert!(n.recovered(What::Lookup).is_empty());
        assert!(n.tick(night + 21_900, at(345)).is_empty());
    }

    #[test]
    fn still_offline_after_a_sleep_says_so_once() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + secs(s);
        let mut n = Notices::default();
        n.slept(3600, 10_000, at(10));
        n.went_offline(10_005, at(15));
        assert!(n.tick(10_100, at(100)).is_empty());
        assert_eq!(
            n.tick(10_130, at(130)),
            vec!["⚠ offline after sleep: no connected peer; reconnecting"]
        );
        assert!(n.tick(10_200, at(200)).is_empty());
        assert_eq!(n.back_online(10_310, at(310)), vec!["back online after 3m"]);
    }

    #[test]
    fn awake_one_offline_line_and_one_back_line() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + secs(s);
        let mut n = Notices::default();
        // A blip: nothing.
        n.went_offline(100, at(100));
        assert!(n.tick(110, at(110)).is_empty());
        assert!(n.back_online(112, at(112)).is_empty());
        // A real outage.
        n.went_offline(200, at(200));
        assert!(n.tick(220, at(220)).is_empty());
        assert_eq!(
            n.tick(230, at(230)),
            vec!["⚠ offline: no connected peer; reconnecting"]
        );
        assert!(n.tick(300, at(300)).is_empty());
        assert_eq!(n.back_online(380, at(380)), vec!["back online after 3m"]);
    }

    #[test]
    fn a_health_issue_shows_after_its_grace() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + secs(s);
        let mut n = Notices::default();
        n.raised(What::Relay, "relay.wearetheswarm.ai unreachable", at(0));
        assert!(n.tick(60, at(60)).is_empty());
        assert_eq!(
            n.tick(120, at(120)),
            vec!["⚠ infra health degraded: relay.wearetheswarm.ai unreachable"]
        );
        assert_eq!(
            n.changed(What::Relay, "no relay"),
            vec!["⚠ infra health degraded: no relay"]
        );
        assert_eq!(
            n.recovered(What::Relay),
            vec!["infra health recovered: relay ok again"]
        );
        // Raised just before a sleep: the grace counts from the wake.
        n.raised(What::Peers, "no connected peer", at(200));
        n.slept(3600, 5000, at(210));
        assert!(n.tick(5100, at(300)).is_empty());
        // The sleep ended (online, settled); the issue lasts.
        assert!(n.tick(5300, at(320)).is_empty());
        assert_eq!(
            n.tick(5320, at(330)),
            vec!["⚠ infra health degraded: no connected peer"]
        );
    }
}
