# 0052. Quiet across sleep

Status: Accepted (2026-10-26), for the release after 0.4.20. Vincent
approved the scope through the internal session: make the brief view
quiet across a laptop's sleep. The technical choices are left to this
ADR.

## Context

Over one night on Vincent's Mac, `wats attach` showed about 20 lines of
"⚠ connectivity: offline, no connected peer; reconnecting" between 02:37
and 10:45. It also showed "infra health degraded: … our record not
republished" and "relay.wearetheswarm.ai and n0's relays unreachable … no
relay". `pmset -g log` matches every offline line to a Sleep or
DarkWake cycle. The infra was fine all along, but a tester on a laptop
reads this as broken.

The health checks debounce on the wall clock. After a sleep, a problem
seen just before it, or at the wake, has already "lasted" hours. The
brief view (ADR 0051) shows every warning, so it showed each one.

## Decision

### 1. Detect sleep from the clocks

Every 5 s, the node compares the wall clock and the monotonic clock
(`crate::awake::slept`):
- On macOS and Linux, Rust's `Instant` does not count sleep, so the wall
  clock goes further. A gap of 20 s or more is a sleep.
- Where the monotonic clock counts sleep, the 5 s tick comes far too
  late, which also counts as a sleep.

This needs no platform API (`IORegisterForSystemPower` would be macOS
only) and works the same on both. A wall clock set back is not a sleep.
A wall clock set forward by 20 s or more looks like one, and costs at
most a `back online after sleep` line.

### 2. The brief view's connectivity and health lines

These lines no longer come from the `wats::reconnect` and `wats::health`
warnings, which the brief view now leaves out. They are brief lines of
their own, from one state machine (`crate::awake::Notices`):

| Situation | Brief line |
|---|---|
| a sleep, then a peer again | `back online after sleep (6h02m)`, once the node has been awake 60 s with a peer and no change, or 2 min |
| a sleep, then no peer 2 min after waking | `⚠ offline after sleep: no connected peer; reconnecting` |
| awake: no peer for 30 s | `⚠ offline: no connected peer; reconnecting` |
| back after that line | `back online after 3m` |
| a health issue lasting 2 min awake | `⚠ infra health degraded: <detail>` |
| that issue clears | `infra health recovered: <check> ok again` |

Sleeps in a row are one sleep: a DarkWake is shorter than the 60 s it
takes to end one, so a night of DarkWakes gives one line in the
morning. Across a sleep there is no offline and no health line. No line
for each reconnect attempt, and none for a blip shorter than 30 s.

### 3. The full log does not change

`node.log` keeps every line it had: reconnect attempts, health warnings,
recoveries. `wats attach --full` and support logs read as before. The
new lines are brief lines (INFO, target `brief`), like those of ADR
0051.

## Consequences

- A laptop's night reads as one line: `back online after sleep (8h08m)`.
- A real outage still shows: one offline line after 30 s, and health
  issues 2 min later than in the full log.
- The `health` and `connectivity` values of `status` do not change: they
  can still read `degraded` for a few minutes after a wake.
- No wire change.
