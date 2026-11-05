//! The node's memory watch: every [`EVERY`], this process's resident memory
//! and how late the tick woke. A crossing of the next level up (256 MiB,
//! 512 MiB, 1 GiB… doubling) logs one WARN with the node's load counters,
//! so that a memory spike leaves in the log what the node was doing when
//! it began; a fall to under half the level logs it once more. A tick
//! that wakes [`STALL`] late means the runtime was starved (a blocking
//! call, or a machine swapping): that too is logged, rate-limited.
//!
//! Nothing is kept but the last level and the time of the last stall
//! line: the watch itself costs one small read every 10 s.

use std::time::Duration;

/// How often the watch reads the resident size.
pub const EVERY: Duration = Duration::from_secs(10);
/// The first level reported: a node at rest sits well under it.
pub const FLOOR: u64 = 256 << 20;
/// A tick this late is a starved runtime.
pub const STALL: Duration = Duration::from_secs(5);
/// At most one stall line per this long.
pub const STALL_LOG_EVERY: Duration = Duration::from_secs(60);

/// What a reading changes in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Quiet,
    /// Resident memory crossed `level` going up.
    Up {
        level: u64,
    },
    /// Resident memory fell under half of the level last reported.
    Down {
        level: u64,
    },
}

/// The level last reported (0: none, under [`FLOOR`]).
#[derive(Debug, Default)]
pub struct Watch {
    level: u64,
}

/// The highest of FLOOR, 2×FLOOR, 4×FLOOR… at or under `rss` (0 under
/// FLOOR).
pub fn level_of(rss: u64) -> u64 {
    if rss < FLOOR {
        return 0;
    }
    let mut level = FLOOR;
    while let Some(next) = level.checked_mul(2) {
        if next > rss {
            break;
        }
        level = next;
    }
    level
}

impl Watch {
    pub fn step(&mut self, rss: u64) -> Step {
        let level = level_of(rss);
        if level > self.level {
            self.level = level;
            return Step::Up { level };
        }
        if self.level > 0 && rss < self.level / 2 {
            self.level = level;
            return Step::Down { level };
        }
        Step::Quiet
    }
}

/// Bytes as MiB, for the log.
pub fn mib(bytes: u64) -> u64 {
    bytes >> 20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_double_from_the_floor() {
        assert_eq!(level_of(0), 0);
        assert_eq!(level_of(FLOOR - 1), 0);
        assert_eq!(level_of(FLOOR), FLOOR);
        assert_eq!(level_of(2 * FLOOR - 1), FLOOR);
        assert_eq!(level_of(2 * FLOOR), 2 * FLOOR);
        assert_eq!(level_of(5 << 30), 4 << 30);
        assert_eq!(level_of(u64::MAX), 1 << 63);
    }

    #[test]
    fn a_spike_is_logged_once_per_level_then_once_on_the_way_down() {
        let mut watch = Watch::default();
        assert_eq!(watch.step(60 << 20), Step::Quiet);
        assert_eq!(watch.step(300 << 20), Step::Up { level: 256 << 20 });
        assert_eq!(watch.step(400 << 20), Step::Quiet);
        // Straight to 2 GiB: the level reached, not each one on the way.
        assert_eq!(watch.step(2100 << 20), Step::Up { level: 2 << 30 });
        assert_eq!(watch.step(1500 << 20), Step::Quiet);
        // Freed: once under half the level.
        assert_eq!(watch.step(190 << 20), Step::Down { level: 0 });
        assert_eq!(watch.step(180 << 20), Step::Quiet);
        // A new spike is reported again.
        assert_eq!(watch.step(600 << 20), Step::Up { level: 512 << 20 });
    }

    #[test]
    fn a_slow_climb_is_logged_at_each_doubling() {
        let mut watch = Watch::default();
        let ups = (100..5000u64)
            .step_by(10)
            .filter(|mb| matches!(watch.step(mb << 20), Step::Up { .. }))
            .count();
        // 256, 512, 1024, 2048, 4096 MiB.
        assert_eq!(ups, 5);
    }
}
