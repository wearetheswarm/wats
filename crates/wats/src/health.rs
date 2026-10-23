//! Infra health alerts (ADR 0009 addendum): each node watches its own
//! view of the infrastructure and says when it degrades. There is no
//! central monitor; any node notices when our relay, our directory, the
//! bootstrap node or n0's services go down, from where it stands.
//!
//! Five checks:
//! - `relay`: the home relay. Ours unreachable (with `wats+n0`, the node
//!   fell back to n0's relays), none of the custom relays reachable, or no
//!   relay at all. Read from the endpoint's home relay status
//!   ([`iroh::Endpoint::home_relay_status`]).
//! - `lookup`: the address directories this node publishes to. Every few
//!   minutes the node resolves its own record through each one (pkarr
//!   over HTTPS, then DNS): not found, stale (not republished), or a
//!   failing DNS lookup is a failure of that directory. With custom
//!   services (`infra_relays`, `infra_dns`), the checks watch those.
//! - `bootstrap`: none of the configured bootstrap nodes or peers is
//!   reachable, and the peer cache is empty or stale, so the node has no
//!   way back into the swarm.
//! - `peers`: no connected peer (gossip neighbor) for `health.no_peers_after`.
//! - `storage`: blob collection (`storage.gc`) stopped. iroh-blobs ends it
//!   for good at the first error of its database, and only a restart
//!   resumes it ([`crate::node::Node::blob_gc_stopped`], which waits two
//!   intervals and ten minutes before it says so).
//!
//! A check that cannot apply is `off`: no relay or lookup in loopback
//! (hermetic) mode or with `infra = none` (and no relay check without a
//! relay, no lookup check without a directory), no bootstrap check without
//! bootstrap nodes or peers configured, no peer check without anywhere to
//! find peers, no storage check with `storage.gc = false`. A problem becomes an issue only once it lasted its check's
//! debounce; each issue is logged once when raised (WARN), once if its
//! description changes for good (WARN), and once when it clears (INFO).
//!
//! The `health` value of `wats status --json` and the MCP `status` tool
//! (stable; see `docs/agent.md`):
//!
//! ```json
//! {"state": "degraded",
//!  "checks": {"relay": "degraded", "lookup": "ok", "bootstrap": "off", "peers": "unknown",
//!             "storage": "ok"},
//!  "issues": [{"what": "relay",
//!              "detail": "relay.wearetheswarm.ai unreachable since 2026-10-18T18:00:00Z, using n0",
//!              "since": "2026-10-18T18:00:00Z"}]}
//! ```
//!
//! `state` is `ok` or `degraded`; `issues` is present only when degraded.
//! Each check is `ok`, `degraded`, `unknown` (not yet confirmed either
//! way) or `off` (not applicable).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::time::Duration;

use iroh::address_lookup::PkarrRelayClient;
use iroh::{Endpoint, EndpointId};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::infra::{Directory, Infra, Preset};

/// How often the node looks at its checks.
pub const CHECK_EVERY: Duration = Duration::from_secs(10);
/// Default of `health.no_peers_after`.
pub const DEFAULT_NO_PEERS_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long the home relay may be out before it is an issue: longer than
/// the relay fallback's own delay, so a fallback is reported, not a blip.
pub const RELAY_ALERT_AFTER: Duration = Duration::from_secs(30);
/// How often the node resolves its own record through each directory.
pub const LOOKUP_PROBE_EVERY: Duration = Duration::from_secs(5 * 60);
/// The first probe: once the record had time to be published.
pub const LOOKUP_FIRST_PROBE_AFTER: Duration = Duration::from_secs(60);
/// A directory failing for this long is an issue (two probes in a row).
pub const LOOKUP_ALERT_AFTER: Duration = Duration::from_secs(4 * 60);
/// How long one probe of one directory may take.
const LOOKUP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Our record is stale when older than this: Iroh republishes it every
/// 5 minutes.
pub const RECORD_STALE_AFTER: Duration = Duration::from_secs(15 * 60);
/// How long no bootstrap node may be reachable before it is an issue.
pub const BOOTSTRAP_ALERT_AFTER: Duration = Duration::from_secs(2 * 60);
/// The peer cache is stale when its most recent peer was last seen longer
/// ago than this.
pub const PEER_CACHE_STALE_AFTER: Duration = Duration::from_secs(24 * 3600);

/// What a check watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum What {
    Relay,
    Lookup,
    Bootstrap,
    Peers,
    Storage,
}

impl What {
    pub const ALL: [What; 5] = [
        What::Relay,
        What::Lookup,
        What::Bootstrap,
        What::Peers,
        What::Storage,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            What::Relay => "relay",
            What::Lookup => "lookup",
            What::Bootstrap => "bootstrap",
            What::Peers => "peers",
            What::Storage => "storage",
        }
    }
}

impl std::fmt::Display for What {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A problem a check observed: `failed` since when, and what the node
/// does instead (e.g. "using n0").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub failed: String,
    pub effect: Option<String>,
}

impl Problem {
    pub fn new(failed: impl Into<String>, effect: Option<&str>) -> Self {
        Problem {
            failed: failed.into(),
            effect: effect.map(str::to_string),
        }
    }

    /// "relay.wearetheswarm.ai unreachable since …, using n0".
    pub fn describe(&self, since: u64) -> String {
        let mut text = format!(
            "{} since {}",
            self.failed,
            crate::update::format_timestamp(since)
        );
        if let Some(effect) = &self.effect {
            text.push_str(", ");
            text.push_str(effect);
        }
        text
    }
}

/// A change worth one log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// A problem lasted the debounce: a new issue.
    Raised { what: What, detail: String },
    /// An issue's description changed, and the new one lasted.
    Changed { what: What, detail: String },
    /// The check is fine again.
    Recovered { what: What, since: u64, now: u64 },
}

impl Transition {
    /// Logs it: WARN when degraded, INFO when recovered.
    pub fn log(&self) {
        match self {
            Transition::Raised { what, detail } | Transition::Changed { what, detail } => {
                warn!(check = %what, "infra health degraded: {detail}");
            }
            Transition::Recovered { what, since, now } => info!(
                check = %what,
                "infra health recovered: {what} ok again (degraded since {}, for {})",
                crate::update::format_timestamp(*since),
                format_span(now.saturating_sub(*since)),
            ),
        }
    }
}

fn format_span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

#[derive(Debug, Clone)]
struct Bad {
    /// When the check first failed (unix seconds).
    since: u64,
    problem: Problem,
    /// When `problem` took its current description.
    problem_since: u64,
    /// The description last logged, once raised.
    logged: Option<Problem>,
}

#[derive(Debug, Clone)]
struct Check {
    what: What,
    applicable: bool,
    debounce: Duration,
    seen_ok: bool,
    bad: Option<Bad>,
}

impl Check {
    fn observe(&mut self, problem: Option<Problem>, now: u64) -> Option<Transition> {
        if !self.applicable {
            return None;
        }
        let debounce = self.debounce.as_secs();
        let Some(problem) = problem else {
            self.seen_ok = true;
            let bad = self.bad.take()?;
            bad.logged.as_ref()?;
            return Some(Transition::Recovered {
                what: self.what,
                since: bad.since,
                now,
            });
        };
        let bad = self.bad.get_or_insert_with(|| Bad {
            since: now,
            problem: problem.clone(),
            problem_since: now,
            logged: None,
        });
        if bad.problem != problem {
            bad.problem = problem;
            bad.problem_since = now;
        }
        match &bad.logged {
            None if now.saturating_sub(bad.since) >= debounce => {
                bad.logged = Some(bad.problem.clone());
                Some(Transition::Raised {
                    what: self.what,
                    detail: bad.problem.describe(bad.since),
                })
            }
            Some(logged)
                if *logged != bad.problem && now.saturating_sub(bad.problem_since) >= debounce =>
            {
                bad.logged = Some(bad.problem.clone());
                Some(Transition::Changed {
                    what: self.what,
                    detail: bad.problem.describe(bad.since),
                })
            }
            _ => None,
        }
    }

    /// The raised issue: (since, its description as logged).
    fn issue(&self) -> Option<(u64, &Problem)> {
        let bad = self.bad.as_ref()?;
        Some((bad.since, bad.logged.as_ref()?))
    }

    fn state(&self) -> &'static str {
        if !self.applicable {
            "off"
        } else if self.issue().is_some() {
            "degraded"
        } else if self.seen_ok {
            "ok"
        } else {
            "unknown"
        }
    }
}

/// Which checks apply to a node, and their debounce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Relay: Internet mode with relays.
    pub relay: bool,
    /// Lookup: Internet mode with a directory.
    pub lookup: bool,
    /// Bootstrap nodes or peers configured.
    pub bootstrap: bool,
    /// The peer check, with its delay: off without anywhere to find peers
    /// or with `health.no_peers_after = 0`.
    pub no_peers_after: Option<Duration>,
    /// Storage: blob collection on (`storage.gc`). Off unless set.
    pub storage: bool,
}

impl Plan {
    /// `loopback`: hermetic mode, no relay and no lookup whatever `infra`
    /// says. `seeds`: bootstrap nodes and peers configured. `cached`: the
    /// peer cache had peers at startup.
    pub fn new(
        loopback: bool,
        infra: &Infra,
        seeds: usize,
        cached: usize,
        no_peers_after: Option<Duration>,
    ) -> Self {
        Plan {
            relay: !loopback && infra.has_relays(),
            lookup: !loopback && infra.has_lookup(),
            bootstrap: seeds > 0,
            no_peers_after: no_peers_after.filter(|_| seeds > 0 || cached > 0),
            storage: false,
        }
    }
}

/// The health of one node: one state machine per check.
#[derive(Debug, Clone)]
pub struct Health {
    checks: Vec<Check>,
}

impl Health {
    pub fn new(plan: Plan) -> Self {
        let check = |what, applicable, debounce| Check {
            what,
            applicable,
            debounce,
            seen_ok: false,
            bad: None,
        };
        Health {
            checks: vec![
                check(What::Relay, plan.relay, RELAY_ALERT_AFTER),
                check(What::Lookup, plan.lookup, LOOKUP_ALERT_AFTER),
                check(What::Bootstrap, plan.bootstrap, BOOTSTRAP_ALERT_AFTER),
                check(
                    What::Peers,
                    plan.no_peers_after.is_some(),
                    plan.no_peers_after.unwrap_or_default(),
                ),
                // Blob collection is called stopped only once overdue.
                check(What::Storage, plan.storage, Duration::ZERO),
            ],
        }
    }

    pub fn applies(&self, what: What) -> bool {
        self.checks.iter().any(|c| c.what == what && c.applicable)
    }

    /// Feeds one observation of `what` (`None`: fine) at `now` (unix
    /// seconds); returns the transition to log, if any.
    pub fn observe(
        &mut self,
        what: What,
        problem: Option<Problem>,
        now: u64,
    ) -> Option<Transition> {
        self.checks
            .iter_mut()
            .find(|c| c.what == what)?
            .observe(problem, now)
    }

    pub fn is_degraded(&self) -> bool {
        self.checks.iter().any(|c| c.issue().is_some())
    }

    /// The `health` value of `status` (see the module documentation).
    pub fn json(&self) -> Value {
        let checks: serde_json::Map<String, Value> = self
            .checks
            .iter()
            .map(|c| (c.what.to_string(), c.state().into()))
            .collect();
        let issues: Vec<Value> = self
            .checks
            .iter()
            .filter_map(|c| {
                let (since, problem) = c.issue()?;
                Some(json!({
                    "what": c.what.as_str(),
                    "detail": problem.describe(since),
                    "since": crate::update::format_timestamp(since),
                }))
            })
            .collect();
        let mut health = serde_json::Map::new();
        let state = if issues.is_empty() { "ok" } else { "degraded" };
        health.insert("state".into(), state.into());
        health.insert("checks".into(), checks.into());
        if !issues.is_empty() {
            health.insert("issues".into(), issues.into());
        }
        health.into()
    }
}

/// One line for humans (`wats status`): "ok", or "degraded: " and the
/// issues' details.
pub fn summary(health: &Value) -> String {
    let state = health["state"].as_str().unwrap_or("unknown");
    let details: Vec<&str> = health["issues"]
        .as_array()
        .map(|issues| issues.iter().filter_map(|i| i["detail"].as_str()).collect())
        .unwrap_or_default();
    if details.is_empty() {
        state.to_string()
    } else {
        format!("{state}: {}", details.join("; "))
    }
}

fn host(url: &str) -> &str {
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    // A fully qualified name ("relay.n0.iroh.link.") is the same host.
    url.split(['/', ':'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('.')
}

/// The relay check, from the home relays and whether each is connected.
pub fn relay_problem(infra: &Infra, home: &[(String, bool)]) -> Option<Problem> {
    let ours = host(crate::infra::WATS_RELAY_URL);
    let connected: Vec<&str> = home
        .iter()
        .filter(|(_, c)| *c)
        .map(|(u, _)| host(u))
        .collect();
    if !infra.relays.is_empty() {
        // Custom relays replace the preset's: one of them must be home.
        let custom: Vec<&str> = infra.relays.iter().map(|r| host(r.url.as_str())).collect();
        if connected.iter().any(|h| custom.contains(h)) {
            return None;
        }
        let failed = match custom.as_slice() {
            [one] => format!("{one} unreachable"),
            many => format!("relays {} unreachable", many.join(", ")),
        };
        return Some(Problem::new(failed, Some("no relay")));
    }
    match infra.preset {
        Preset::None | Preset::Custom => None,
        Preset::N0 if connected.is_empty() => {
            Some(Problem::new("n0's relays unreachable", Some("no relay")))
        }
        Preset::N0 => None,
        Preset::Wats if !connected.contains(&ours) => Some(Problem::new(
            format!("{ours} unreachable"),
            Some("no relay"),
        )),
        Preset::Wats => None,
        Preset::WatsAndN0 if connected.contains(&ours) => None,
        Preset::WatsAndN0 if connected.is_empty() => Some(Problem::new(
            format!("{ours} and n0's relays unreachable"),
            Some("no relay"),
        )),
        Preset::WatsAndN0 => Some(Problem::new(
            format!("{ours} unreachable"),
            Some("using n0"),
        )),
    }
}

/// What a probe found in one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Our record is there, fresh, and DNS serves it.
    Ok,
    /// Not found, or the directory did not answer.
    NotFound,
    /// Found, but older than [`RECORD_STALE_AFTER`]: publishing fails.
    Stale,
    /// Pkarr serves it, DNS does not.
    DnsFails,
}

/// The lookup check, from one probe per directory (in the profile's
/// order).
pub fn lookup_problem(probes: &[(Directory, Probe)]) -> Option<Problem> {
    let name = |d: &Directory| d.host();
    let failing: Vec<String> = probes
        .iter()
        .filter_map(|(d, p)| {
            let why = match p {
                Probe::Ok => return None,
                Probe::NotFound => "our record not found",
                Probe::Stale => "our record not republished",
                Probe::DnsFails => "DNS lookup fails",
            };
            Some(format!("{}: {why}", name(d)))
        })
        .collect();
    if failing.is_empty() {
        return None;
    }
    let working: Vec<String> = probes
        .iter()
        .filter(|(_, p)| *p == Probe::Ok)
        .map(|(d, _)| name(d))
        .collect();
    let effect = if working.is_empty() {
        "peers cannot find this node".to_string()
    } else {
        format!("using {}", working.join(", "))
    };
    Some(Problem::new(failing.join("; "), Some(&effect)))
}

/// The bootstrap check. `reachable`: a bootstrap node or configured peer
/// is a neighbor or has an active path; `newest_cached`: when the most
/// recent peer of the cache was last seen.
pub fn bootstrap_problem(reachable: bool, newest_cached: Option<u64>, now: u64) -> Option<Problem> {
    if reachable {
        return None;
    }
    match newest_cached {
        None => Some(Problem::new(
            "no bootstrap node reachable",
            Some("peer cache empty"),
        )),
        Some(seen) if now.saturating_sub(seen) >= PEER_CACHE_STALE_AFTER.as_secs() => {
            Some(Problem::new(
                "no bootstrap node reachable",
                Some(&format!(
                    "peer cache stale (last peer seen {})",
                    crate::update::format_timestamp(seen)
                )),
            ))
        }
        // Peers met lately: the node can rejoin through them.
        Some(_) => None,
    }
}

/// The peer check.
pub fn peers_problem(connected: usize) -> Option<Problem> {
    (connected == 0).then(|| Problem::new("no connected peer", None))
}

/// The storage check, from whether blob collection stopped.
pub fn storage_problem(gc_stopped: bool) -> Option<Problem> {
    gc_stopped.then(|| {
        Problem::new(
            "blob collection stopped",
            Some("restart the node to resume it"),
        )
    })
}

/// Resolves this node's own record through `directory`: pkarr over HTTPS
/// (found, and fresh), then DNS when the directory has a DNS origin.
pub async fn probe(endpoint: &Endpoint, directory: &Directory, now: u64) -> Probe {
    let me: EndpointId = endpoint.id();
    let Ok(dns) = endpoint.dns_resolver().cloned() else {
        return Probe::NotFound;
    };
    let Ok(url) = directory.pkarr_url.parse::<url::Url>() else {
        return Probe::NotFound;
    };
    let client = PkarrRelayClient::new(url, endpoint.tls_config().clone(), dns.clone());
    let packet = match tokio::time::timeout(LOOKUP_PROBE_TIMEOUT, client.resolve(me)).await {
        Ok(Ok(packet)) => packet,
        Ok(Err(e)) => {
            debug!(directory = %directory.pkarr_url, "lookup probe: {e:#}");
            return Probe::NotFound;
        }
        Err(_) => {
            debug!(directory = %directory.pkarr_url, "lookup probe: timeout");
            return Probe::NotFound;
        }
    };
    let published = packet.timestamp().as_micros() / 1_000_000;
    if now.saturating_sub(published) >= RECORD_STALE_AFTER.as_secs() {
        return Probe::Stale;
    }
    let Some(origin) = directory.dns_origin.as_deref() else {
        return Probe::Ok;
    };
    let lookup = dns.lookup_endpoint_by_id(&me, origin);
    let failed = match tokio::time::timeout(LOOKUP_PROBE_TIMEOUT, lookup).await {
        Ok(Ok(_)) => return Probe::Ok,
        Ok(Err(e)) => format!("{e:#}"),
        Err(_) => "timeout".to_string(),
    };
    // The record is there (the directory served it above): the resolver
    // is what fails. One built before the host's DNS was ready (a
    // container's first seconds), or a cached miss from before the first
    // publish, stays wrong until rebuilt: the next probe, and the
    // endpoint's own lookups, use a fresh one.
    debug!(directory = origin, "DNS lookup probe: {failed}; resetting the resolver");
    dns.reset();
    Probe::DnsFails
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
    use crate::infra::{N0_DIRECTORY, WATS_DIRECTORY};

    const T0: u64 = 1_792_346_400; // 2026-10-18T18:00:00Z

    fn all_on() -> Health {
        Health::new(Plan::new(
            false,
            &Infra::default(),
            1,
            0,
            Some(DEFAULT_NO_PEERS_AFTER),
        ))
    }

    fn fallback() -> Option<Problem> {
        relay_problem(
            &Infra::default(),
            &[(
                "https://euw1-1.relay.n0.iroh-canary.iroh.link./".into(),
                true,
            )],
        )
    }

    #[test]
    fn a_problem_is_raised_after_its_debounce_and_logged_once() {
        let mut health = all_on();
        assert_eq!(health.json()["checks"]["relay"], "unknown");
        // A blip shorter than the debounce: nothing.
        assert_eq!(health.observe(What::Relay, fallback(), T0), None);
        assert_eq!(health.observe(What::Relay, fallback(), T0 + 20), None);
        assert_eq!(health.observe(What::Relay, None, T0 + 25), None);
        assert_eq!(health.json()["checks"]["relay"], "ok");
        assert_eq!(health.json()["state"], "ok");
        // A lasting one: raised once, with when it started.
        assert_eq!(health.observe(What::Relay, fallback(), T0 + 100), None);
        let raised = health.observe(What::Relay, fallback(), T0 + 130).unwrap();
        assert_eq!(
            raised,
            Transition::Raised {
                what: What::Relay,
                detail: "relay.wearetheswarm.ai unreachable since 2026-10-18T18:01:40Z, using n0"
                    .into()
            }
        );
        for t in 131..1000 {
            assert_eq!(health.observe(What::Relay, fallback(), T0 + t), None);
        }
        assert_eq!(health.json()["checks"]["relay"], "degraded");
        // Recovered: one INFO.
        assert_eq!(
            health.observe(What::Relay, None, T0 + 1000),
            Some(Transition::Recovered {
                what: What::Relay,
                since: T0 + 100,
                now: T0 + 1000
            })
        );
        assert_eq!(health.observe(What::Relay, None, T0 + 1010), None);
        assert_eq!(
            health.json(),
            json!({"state": "ok", "checks": {
            "relay": "ok", "lookup": "unknown", "bootstrap": "unknown", "peers": "unknown",
            "storage": "off"}})
        );
    }

    #[test]
    fn a_flapping_description_is_not_logged_until_it_lasts() {
        let mut health = all_on();
        let none = relay_problem(&Infra::default(), &[]);
        health.observe(What::Relay, fallback(), T0);
        assert!(matches!(
            health.observe(What::Relay, fallback(), T0 + 30),
            Some(Transition::Raised { .. })
        ));
        // The fallback retries ours alone every 5 minutes: 15 s without a
        // relay, then n0 again. No log line for that.
        assert_eq!(health.observe(What::Relay, none.clone(), T0 + 300), None);
        assert_eq!(health.observe(What::Relay, none.clone(), T0 + 315), None);
        assert_eq!(health.observe(What::Relay, fallback(), T0 + 320), None);
        assert_eq!(health.observe(What::Relay, fallback(), T0 + 400), None);
        // No relay at all for good: one WARN, same `since`.
        health.observe(What::Relay, none.clone(), T0 + 600);
        let changed = health.observe(What::Relay, none.clone(), T0 + 630).unwrap();
        assert_eq!(
            changed,
            Transition::Changed {
                what: What::Relay,
                detail: "relay.wearetheswarm.ai and n0's relays unreachable since \
                         2026-10-18T18:00:00Z, no relay"
                    .into()
            }
        );
        assert_eq!(health.observe(What::Relay, none, T0 + 700), None);
    }

    #[test]
    fn no_peers_timer() {
        let mut health = all_on();
        let after = DEFAULT_NO_PEERS_AFTER.as_secs();
        assert_eq!(health.observe(What::Peers, peers_problem(0), T0), None);
        assert_eq!(
            health.observe(What::Peers, peers_problem(0), T0 + after - 1),
            None
        );
        let raised = health
            .observe(What::Peers, peers_problem(0), T0 + after)
            .unwrap();
        assert_eq!(
            raised,
            Transition::Raised {
                what: What::Peers,
                detail: "no connected peer since 2026-10-18T18:00:00Z".into()
            }
        );
        // A peer comes and goes: the timer starts over.
        assert!(matches!(
            health.observe(What::Peers, peers_problem(2), T0 + after + 10),
            Some(Transition::Recovered { .. })
        ));
        assert_eq!(
            health.observe(What::Peers, peers_problem(0), T0 + after + 20),
            None
        );
        assert_eq!(
            health.observe(What::Peers, peers_problem(0), T0 + 2 * after + 19),
            None
        );
        assert!(
            health
                .observe(What::Peers, peers_problem(0), T0 + 2 * after + 20)
                .is_some()
        );
    }

    #[test]
    fn checks_that_cannot_apply_are_off_and_never_degrade() {
        let after = Some(DEFAULT_NO_PEERS_AFTER);
        // Hermetic (loopback) or `infra = none`, nothing configured.
        for plan in [
            Plan::new(true, &Infra::default(), 0, 0, after),
            Plan::new(false, &Infra::NONE, 0, 0, after),
        ] {
            let mut health = Health::new(plan);
            for what in What::ALL {
                assert!(!health.applies(what), "{what}");
                for t in [0, 3600, 86_400] {
                    let problem = Some(Problem::new("down", None));
                    assert_eq!(health.observe(what, problem, T0 + t), None);
                }
            }
            assert_eq!(
                health.json(),
                json!({"state": "ok", "checks": {
                    "relay": "off", "lookup": "off", "bootstrap": "off", "peers": "off",
                    "storage": "off"}})
            );
            assert!(!health.is_degraded());
        }
        // Peers met before, no bootstrap: only the peer check.
        let plan = Plan::new(true, &Infra::NONE, 0, 3, after);
        assert_eq!(
            (plan.relay, plan.lookup, plan.bootstrap, plan.no_peers_after),
            (false, false, false, after)
        );
        // Custom relays only: no lookup check; a custom dns only: no relay
        // check.
        let relays = Infra::resolve(None, &["https://relay.example.org".into()], None).unwrap();
        let plan = Plan::new(false, &relays, 0, 0, after);
        assert_eq!((plan.relay, plan.lookup), (true, false));
        let dns = Infra::resolve(None, &[], Some("https://dns.example.org")).unwrap();
        let plan = Plan::new(false, &dns, 0, 0, after);
        assert_eq!((plan.relay, plan.lookup), (false, true));
        // `health.no_peers_after = 0`.
        assert_eq!(
            Plan::new(false, &Infra::preset(Preset::Wats), 1, 0, None).no_peers_after,
            None
        );
        assert_eq!(relay_problem(&Infra::NONE, &[]), None);
    }

    #[test]
    fn relay_states() {
        let ours = ("https://relay.wearetheswarm.ai./".to_string(), true);
        let n0 = ("https://use1-1.relay.n0.iroh.link./".to_string(), true);
        let down = |(u, _): &(String, bool)| (u.clone(), false);
        let wats = Infra::preset(Preset::Wats);
        let n0_infra = Infra::preset(Preset::N0);
        assert_eq!(
            relay_problem(&Infra::default(), std::slice::from_ref(&ours)),
            None
        );
        assert_eq!(
            relay_problem(&Infra::default(), std::slice::from_ref(&n0)).unwrap(),
            Problem::new("relay.wearetheswarm.ai unreachable", Some("using n0"))
        );
        assert_eq!(
            relay_problem(&Infra::default(), &[down(&ours)])
                .unwrap()
                .effect
                .as_deref(),
            Some("no relay")
        );
        assert_eq!(relay_problem(&wats, std::slice::from_ref(&ours)), None);
        assert_eq!(
            relay_problem(&wats, &[]).unwrap(),
            Problem::new("relay.wearetheswarm.ai unreachable", Some("no relay"))
        );
        assert_eq!(relay_problem(&n0_infra, std::slice::from_ref(&n0)), None);
        assert!(relay_problem(&n0_infra, &[down(&n0)]).is_some());
        // The relay URL of the profile matches the one checked.
        assert_eq!(host(&wats.relay_urls()[0]), "relay.wearetheswarm.ai");
        // Custom relays: one of them home, whatever the preset.
        let custom = Infra::resolve(
            Some("wats+n0"),
            &[
                "https://relay.example.org".into(),
                "https://r2.example.org#quic=7843".into(),
            ],
            None,
        )
        .unwrap();
        let mine = ("https://r2.example.org./".to_string(), true);
        assert_eq!(relay_problem(&custom, std::slice::from_ref(&mine)), None);
        assert_eq!(
            relay_problem(&custom, &[ours.clone(), down(&mine)]).unwrap(),
            Problem::new(
                "relays relay.example.org, r2.example.org unreachable",
                Some("no relay")
            )
        );
        let one = Infra::resolve(None, &["http://127.0.0.1:3340".into()], None).unwrap();
        assert_eq!(
            relay_problem(&one, &[]).unwrap().failed,
            "127.0.0.1 unreachable"
        );
    }

    #[test]
    fn lookup_states() {
        let both = |a, b| lookup_problem(&[(WATS_DIRECTORY, a), (N0_DIRECTORY, b)]);
        assert_eq!(both(Probe::Ok, Probe::Ok), None);
        assert_eq!(
            both(Probe::NotFound, Probe::Ok).unwrap().describe(T0),
            "dns.wearetheswarm.ai: our record not found since 2026-10-18T18:00:00Z, \
             using dns.iroh.link"
        );
        assert_eq!(
            both(Probe::Stale, Probe::DnsFails).unwrap(),
            Problem::new(
                "dns.wearetheswarm.ai: our record not republished; dns.iroh.link: DNS lookup fails",
                Some("peers cannot find this node")
            )
        );
        assert_eq!(lookup_problem(&[]), None);
        // A custom directory is named by its host.
        let mine: Directory = "https://pkarr.example.org/pkarr".parse().unwrap();
        assert_eq!(
            lookup_problem(&[(mine, Probe::NotFound)]).unwrap(),
            Problem::new(
                "pkarr.example.org: our record not found",
                Some("peers cannot find this node")
            )
        );
    }

    #[test]
    fn bootstrap_states() {
        let stale = PEER_CACHE_STALE_AFTER.as_secs();
        assert_eq!(bootstrap_problem(true, None, T0), None);
        assert_eq!(
            bootstrap_problem(false, None, T0)
                .unwrap()
                .effect
                .as_deref(),
            Some("peer cache empty")
        );
        assert_eq!(bootstrap_problem(false, Some(T0 - 60), T0), None);
        assert_eq!(
            bootstrap_problem(false, Some(T0 - stale), T0)
                .unwrap()
                .effect
                .as_deref(),
            Some("peer cache stale (last peer seen 2026-10-17T18:00:00Z)")
        );
    }

    #[test]
    fn status_shape_and_summary() {
        let mut plan = Plan::new(false, &Infra::default(), 1, 0, Some(DEFAULT_NO_PEERS_AFTER));
        plan.storage = true;
        let mut health = Health::new(plan);
        // Blob collection: raised as soon as it is called stopped.
        assert_eq!(
            health.observe(What::Storage, storage_problem(false), T0),
            None
        );
        assert!(
            health
                .observe(What::Storage, storage_problem(true), T0)
                .is_some()
        );
        health.observe(What::Relay, fallback(), T0);
        health.observe(What::Relay, fallback(), T0 + 30);
        health.observe(What::Peers, peers_problem(0), T0);
        health.observe(What::Peers, peers_problem(0), T0 + 300);
        health.observe(What::Bootstrap, None, T0);
        let value = health.json();
        assert_eq!(
            value,
            json!({
                "state": "degraded",
                "checks": {"relay": "degraded", "lookup": "unknown",
                           "bootstrap": "ok", "peers": "degraded", "storage": "degraded"},
                "issues": [
                    {"what": "relay",
                     "detail": "relay.wearetheswarm.ai unreachable since 2026-10-18T18:00:00Z, using n0",
                     "since": "2026-10-18T18:00:00Z"},
                    {"what": "peers",
                     "detail": "no connected peer since 2026-10-18T18:00:00Z",
                     "since": "2026-10-18T18:00:00Z"},
                    {"what": "storage",
                     "detail": "blob collection stopped since 2026-10-18T18:00:00Z, \
                                restart the node to resume it",
                     "since": "2026-10-18T18:00:00Z"},
                ]
            })
        );
        assert!(health.is_degraded());
        assert_eq!(
            summary(&value),
            "degraded: relay.wearetheswarm.ai unreachable since 2026-10-18T18:00:00Z, \
             using n0; no connected peer since 2026-10-18T18:00:00Z; \
             blob collection stopped since 2026-10-18T18:00:00Z, restart the node to resume it"
        );
        assert_eq!(summary(&json!({"state": "ok", "checks": {}})), "ok");
        assert_eq!(format_span(59), "59s");
        assert_eq!(format_span(3 * 3600 + 120), "3h02m");
    }
}
