//! Which relay and address-lookup services a node uses (ADR 0009).
//!
//! Iroh nodes rely on two kinds of infrastructure services, neither of
//! which stores swarm content:
//! - **relays**: rendezvous for hole punching, and fallback transport when a
//!   direct path fails;
//! - **address lookup** (pkarr over HTTPS, and DNS): a node publishes
//!   "endpoint id -> relay URL and addresses" so others can reach it from
//!   its id alone.
//!
//! Profiles:
//! - `wats+n0` (default): WeAreTheSwarm's own relay and DNS first, n0's as
//!   fallback. The address record is published to both directories and
//!   resolved through ours first. Our relay is the only home relay
//!   candidate while it is reachable; n0's relays join only while it is not
//!   (see [`relay_step`]);
//! - `wats`: WeAreTheSwarm's own relay and DNS only, no third party;
//! - `n0`: Iroh's public services operated by n0 (the Iroh defaults);
//! - `none`: no relay and no lookup; peers must be dialed by address
//!   (bootstrap entries, tickets, local networks).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::address_lookup::{
    AddressLookup, AddressLookupBuilder, AddressLookupBuilderError, DnsAddressLookup,
    Error as LookupError, Item, N0_DNS_ENDPOINT_ORIGIN_PROD, N0_DNS_PKARR_RELAY_PROD,
    PkarrPublisher, PkarrResolver,
};
use iroh::endpoint::{Builder, presets};
use iroh::{Endpoint, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, Watcher};
use iroh_relay::RelayQuicConfig;
use n0_future::boxed::BoxStream;
use n0_future::{MergeUnbounded, Stream};
use tracing::{info, warn};

/// WeAreTheSwarm relay (provisional, ADR 0009).
pub const WATS_RELAY_URL: &str = "https://relay.wearetheswarm.ai";
/// Its QUIC address-discovery port: 7843, because the router node holds
/// 7842 on the same VM.
pub const WATS_RELAY_QUIC_PORT: u16 = 7843;
/// WeAreTheSwarm pkarr server (publish and resolve over HTTPS).
pub const WATS_PKARR_URL: &str = "https://dns.wearetheswarm.ai/pkarr";
/// DNS origin for lookups: `_iroh.<z32 id>.dns.wearetheswarm.ai` TXT.
pub const WATS_DNS_ORIGIN: &str = "dns.wearetheswarm.ai";

/// How long a resolution waits for a directory before also asking the
/// next one. A directory that answers "not found" or fails hands over at
/// once.
pub const RESOLVE_HEAD_START: Duration = Duration::from_secs(2);
/// How long our relay may stay out of reach before n0's relays become
/// home relay candidates (`wats+n0`).
pub const RELAY_FALLBACK_AFTER: Duration = Duration::from_secs(15);
/// How long n0's relays stay candidates before the node tries ours alone
/// again (`wats+n0`). If ours is still out of reach, the node keeps its
/// current home relay and falls back again after [`RELAY_FALLBACK_AFTER`].
pub const RELAY_RETRY_OURS_AFTER: Duration = Duration::from_secs(5 * 60);
/// How often the relay fallback checks the home relay.
const RELAY_CHECK_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Infra {
    /// Iroh's public relays and DNS, operated by n0.
    N0,
    /// WeAreTheSwarm's own relay and DNS only: failures of our
    /// infrastructure stay visible instead of falling back to n0.
    Wats,
    /// Default (spec D25): ours first, n0 as fallback, for both the relay
    /// and the directory.
    #[default]
    WatsAndN0,
    /// No relay, no address lookup.
    None,
}

impl std::str::FromStr for Infra {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "n0" => Ok(Infra::N0),
            "wats" => Ok(Infra::Wats),
            "wats+n0" => Ok(Infra::WatsAndN0),
            "none" => Ok(Infra::None),
            other => anyhow::bail!("unknown infra {other:?}: use wats+n0, wats, n0 or none"),
        }
    }
}

impl std::fmt::Display for Infra {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Infra::N0 => "n0",
            Infra::Wats => "wats",
            Infra::WatsAndN0 => "wats+n0",
            Infra::None => "none",
        })
    }
}

/// An address directory: pkarr over HTTPS (publish and resolve) and DNS
/// (resolve).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Directory {
    pub pkarr_url: &'static str,
    pub dns_origin: &'static str,
}

/// WeAreTheSwarm's directory.
pub const WATS_DIRECTORY: Directory = Directory {
    pkarr_url: WATS_PKARR_URL,
    dns_origin: WATS_DNS_ORIGIN,
};
/// n0's directory (`dns.iroh.link`).
pub const N0_DIRECTORY: Directory = Directory {
    pkarr_url: N0_DNS_PKARR_RELAY_PROD,
    dns_origin: N0_DNS_ENDPOINT_ORIGIN_PROD,
};

fn wats_relay() -> Result<RelayConfig> {
    let url: RelayUrl = WATS_RELAY_URL.parse().context("WATS_RELAY_URL")?;
    Ok(RelayConfig::new(
        url,
        Some(RelayQuicConfig::new(WATS_RELAY_QUIC_PORT)),
    ))
}

/// n0's relays, the Iroh defaults.
fn n0_relays() -> RelayMap {
    RelayMode::Default.relay_map()
}

impl Infra {
    /// An endpoint builder for Internet mode with this infrastructure.
    ///
    /// For `wats+n0`, the relay map starts with our relay alone; call
    /// [`Infra::spawn_relay_fallback`] once the endpoint is bound so n0's
    /// relays join when ours is out of reach.
    pub fn builder(self) -> Result<Builder> {
        Ok(match self {
            Infra::N0 => Endpoint::builder(presets::N0),
            Infra::None => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
            Infra::Wats | Infra::WatsAndN0 => {
                let mut builder = Endpoint::builder(presets::Minimal);
                for directory in self.publish_to() {
                    let pkarr: url::Url = directory.pkarr_url.parse().context("pkarr URL")?;
                    builder = builder.address_lookup(PkarrPublisher::builder(pkarr));
                }
                builder
                    .address_lookup(OrderedLookupBuilder {
                        tiers: self.resolve_order(),
                        head_start: RESOLVE_HEAD_START,
                    })
                    .relay_mode(RelayMode::Custom(RelayMap::from_iter([wats_relay()?])))
            }
        })
    }

    /// The directories a node publishes its address record to.
    pub fn publish_to(self) -> Vec<Directory> {
        match self {
            Infra::N0 => vec![N0_DIRECTORY],
            Infra::Wats => vec![WATS_DIRECTORY],
            Infra::WatsAndN0 => vec![WATS_DIRECTORY, N0_DIRECTORY],
            Infra::None => vec![],
        }
    }

    /// The directories a node resolves peers through, in order: each one
    /// is asked only when the ones before it found nothing, failed, or did
    /// not answer within [`RESOLVE_HEAD_START`]. (`n0` uses Iroh's preset,
    /// which asks its pkarr and DNS endpoints together.)
    pub fn resolve_order(self) -> Vec<Directory> {
        self.publish_to()
    }

    /// Home relay candidates, by preference: the first list while one of
    /// its relays is reachable, the next one otherwise.
    pub fn relay_tiers(self) -> Vec<Vec<String>> {
        let urls = |map: RelayMap| -> Vec<String> {
            let mut urls: Vec<String> =
                map.urls::<Vec<_>>().iter().map(|u| u.to_string()).collect();
            urls.sort();
            urls
        };
        let ours = || urls(RelayMap::from_iter(wats_relay().ok()));
        match self {
            Infra::N0 => vec![urls(n0_relays())],
            Infra::Wats => vec![ours()],
            Infra::WatsAndN0 => vec![ours(), urls(n0_relays())],
            Infra::None => vec![],
        }
    }

    /// The relays nodes of this profile may use, by preference, for
    /// display.
    pub fn relay_urls(self) -> Vec<String> {
        self.relay_tiers().concat()
    }

    /// For `wats+n0`, watch the home relay and add n0's relays to the relay
    /// map while ours is out of reach (see [`relay_step`]). Stops when the
    /// endpoint closes. Does nothing for the other profiles.
    pub fn spawn_relay_fallback(self, endpoint: &Endpoint) {
        if self != Infra::WatsAndN0 {
            return;
        }
        let ours = match wats_relay() {
            Ok(ours) => ours.url,
            Err(e) => {
                warn!("relay fallback disabled: {e:#}");
                return;
            }
        };
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let make = || {
                let endpoint = endpoint.clone();
                let task = relay_fallback(endpoint.clone(), ours.clone());
                async move { endpoint.closed().run_until(task).await }
            };
            crate::supervise::supervise("relay fallback", make).await;
        });
    }
}

/// What the relay fallback does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStep {
    Wait,
    /// Add n0's relays to the relay map: ours has been out of reach for
    /// [`RELAY_FALLBACK_AFTER`].
    AddFallback,
    /// Remove n0's relays: ours is home again, or it is time to retry it.
    DropFallback,
}

/// The relay fallback's decision. `elapsed` is the time since ours was
/// last connected as home (without fallback), or since the fallback
/// started (with it).
///
/// Iroh 1.3 picks the home relay by latency alone, with no priority
/// between relays of one map. So "prefer ours" is expressed by keeping
/// n0's relays out of the map while ours is reachable. A node can still
/// reach a peer whose home relay is not in its map.
pub fn relay_step(fallback_on: bool, ours_connected: bool, elapsed: Duration) -> RelayStep {
    match (fallback_on, ours_connected) {
        (false, true) => RelayStep::Wait,
        (false, false) if elapsed >= RELAY_FALLBACK_AFTER => RelayStep::AddFallback,
        (false, false) => RelayStep::Wait,
        // Iroh chose ours as home again: it is reachable and the fastest.
        (true, true) => RelayStep::DropFallback,
        (true, false) if elapsed >= RELAY_RETRY_OURS_AFTER => RelayStep::DropFallback,
        (true, false) => RelayStep::Wait,
    }
}

async fn relay_fallback(endpoint: Endpoint, ours: RelayUrl) {
    let fallback = n0_relays();
    let mut status = endpoint.home_relay_status();
    let mut fallback_on = false;
    let mut since = tokio::time::Instant::now();
    loop {
        let ours_connected = status
            .get()
            .iter()
            .any(|s| s.url() == &ours && s.is_connected());
        if !fallback_on && ours_connected {
            since = tokio::time::Instant::now();
        }
        match relay_step(fallback_on, ours_connected, since.elapsed()) {
            RelayStep::Wait => {}
            RelayStep::AddFallback => {
                warn!(relay = %ours, "our relay is out of reach; n0's relays are now candidates");
                for config in fallback.relays::<Vec<_>>() {
                    endpoint.insert_relay(config.url.clone(), config).await;
                }
                fallback_on = true;
                since = tokio::time::Instant::now();
            }
            RelayStep::DropFallback => {
                info!(relay = %ours, home = ours_connected, "trying our relay alone again");
                for url in fallback.urls::<Vec<_>>() {
                    endpoint.remove_relay(&url).await;
                }
                fallback_on = false;
                since = tokio::time::Instant::now();
            }
        }
        tokio::time::sleep(RELAY_CHECK_EVERY).await;
    }
}

type ItemStream = BoxStream<Result<Item, LookupError>>;

/// Resolves through directories in order (see [`Infra::resolve_order`]).
#[derive(Debug)]
struct OrderedLookupBuilder {
    tiers: Vec<Directory>,
    head_start: Duration,
}

impl AddressLookupBuilder for OrderedLookupBuilder {
    fn into_address_lookup(
        self,
        endpoint: &Endpoint,
    ) -> Result<impl AddressLookup, AddressLookupBuilderError> {
        let mut tiers = Vec::new();
        for directory in self.tiers {
            let pkarr: url::Url = directory
                .pkarr_url
                .parse()
                .map_err(|e| AddressLookupBuilderError::from_err("pkarr", e))?;
            let resolvers: Vec<Box<dyn AddressLookup>> = vec![
                Box::new(PkarrResolver::builder(pkarr).into_address_lookup(endpoint)?),
                Box::new(
                    DnsAddressLookup::builder(directory.dns_origin.to_string())
                        .into_address_lookup(endpoint)?,
                ),
            ];
            tiers.push(resolvers);
        }
        Ok(OrderedLookup {
            tiers: Arc::new(tiers),
            head_start: self.head_start,
        })
    }
}

#[derive(Debug)]
struct OrderedLookup {
    tiers: Arc<Vec<Vec<Box<dyn AddressLookup>>>>,
    head_start: Duration,
}

impl AddressLookup for OrderedLookup {
    fn resolve(&self, endpoint_id: EndpointId) -> Option<ItemStream> {
        let tiers = Arc::clone(&self.tiers);
        let start = move |tier: usize| -> Option<Vec<ItemStream>> {
            tiers.get(tier).map(|lookups| {
                lookups
                    .iter()
                    .filter_map(|lookup| lookup.resolve(endpoint_id))
                    .collect()
            })
        };
        Some(Box::pin(OrderedResolve::new(start, self.head_start)))
    }
}

/// A resolution through tiers of lookups. The lookups of one tier run
/// together; the next tier starts only when the running ones have produced
/// no item and either all ended or `head_start` elapsed. Once an item is
/// found, no further tier starts.
struct OrderedResolve<F> {
    start: F,
    next_tier: usize,
    exhausted: bool,
    found: bool,
    running: MergeUnbounded<ItemStream>,
    head_start: Duration,
    timer: Pin<Box<tokio::time::Sleep>>,
}

impl<F> OrderedResolve<F>
where
    F: FnMut(usize) -> Option<Vec<ItemStream>>,
{
    fn new(start: F, head_start: Duration) -> Self {
        let mut this = Self {
            start,
            next_tier: 0,
            exhausted: false,
            found: false,
            running: MergeUnbounded::new(),
            head_start,
            timer: Box::pin(tokio::time::sleep(head_start)),
        };
        this.start_next();
        this
    }

    /// Starts the next tier; false when there is none.
    fn start_next(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        let Some(streams) = (self.start)(self.next_tier) else {
            self.exhausted = true;
            return false;
        };
        self.next_tier += 1;
        for stream in streams {
            self.running.push(stream);
        }
        self.timer
            .as_mut()
            .reset(tokio::time::Instant::now() + self.head_start);
        true
    }
}

impl<F> Stream for OrderedResolve<F>
where
    F: FnMut(usize) -> Option<Vec<ItemStream>> + Unpin,
{
    type Item = Result<Item, LookupError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if !this.found
                && !this.exhausted
                && this.timer.as_mut().poll(cx).is_ready()
                && this.start_next()
            {
                continue;
            }
            match Pin::new(&mut this.running).poll_next(cx) {
                Poll::Ready(Some(Ok(item))) => {
                    this.found = true;
                    return Poll::Ready(Some(Ok(item)));
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    if !this.found && this.start_next() {
                        continue;
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use iroh::address_lookup::EndpointInfo;
    use n0_future::StreamExt;

    use super::*;

    #[test]
    fn parses_profiles() {
        for name in ["n0", "wats", "wats+n0", "none"] {
            assert_eq!(name.parse::<Infra>().unwrap().to_string(), name);
        }
        assert!("aws".parse::<Infra>().is_err());
    }

    #[test]
    fn default_is_ours_first_with_n0_fallback() {
        assert_eq!(Infra::default(), Infra::WatsAndN0);
        assert_eq!(Infra::default().to_string(), "wats+n0");
    }

    #[test]
    fn directories_are_ours_first() {
        assert_eq!(
            Infra::WatsAndN0.publish_to(),
            vec![WATS_DIRECTORY, N0_DIRECTORY]
        );
        assert_eq!(
            Infra::WatsAndN0.resolve_order(),
            vec![WATS_DIRECTORY, N0_DIRECTORY]
        );
        assert_eq!(Infra::Wats.publish_to(), vec![WATS_DIRECTORY]);
        assert_eq!(Infra::Wats.resolve_order(), vec![WATS_DIRECTORY]);
        assert_eq!(Infra::N0.publish_to(), vec![N0_DIRECTORY]);
        assert!(Infra::None.publish_to().is_empty());
        assert_eq!(
            N0_DIRECTORY.dns_origin.trim_end_matches('.'),
            "dns.iroh.link"
        );
        assert_eq!(WATS_DIRECTORY.dns_origin, "dns.wearetheswarm.ai");
    }

    #[test]
    fn relay_sets() {
        let ours = vec![format!("{WATS_RELAY_URL}/")];
        assert_eq!(Infra::Wats.relay_urls(), ours);
        assert!(Infra::None.relay_urls().is_empty());
        let n0 = Infra::N0.relay_urls();
        assert!(!n0.is_empty() && n0.iter().all(|u| u.contains("iroh.link")));
        assert_eq!(
            Infra::WatsAndN0.relay_tiers(),
            vec![ours.clone(), n0.clone()]
        );
        let both = Infra::WatsAndN0.relay_urls();
        assert_eq!(both.len(), n0.len() + 1);
        assert_eq!(both[0], ours[0]);
    }

    #[test]
    fn relay_fallback_steps() {
        let soon = RELAY_FALLBACK_AFTER - Duration::from_secs(1);
        // Ours is home: nothing to do, however long.
        assert_eq!(relay_step(false, true, Duration::ZERO), RelayStep::Wait);
        assert_eq!(
            relay_step(false, true, RELAY_RETRY_OURS_AFTER),
            RelayStep::Wait
        );
        // Ours out of reach: wait, then fall back.
        assert_eq!(relay_step(false, false, soon), RelayStep::Wait);
        assert_eq!(
            relay_step(false, false, RELAY_FALLBACK_AFTER),
            RelayStep::AddFallback
        );
        // During the fallback: back to ours as soon as Iroh picks it...
        assert_eq!(
            relay_step(true, true, Duration::ZERO),
            RelayStep::DropFallback
        );
        // ...otherwise retry it periodically.
        assert_eq!(relay_step(true, false, soon), RelayStep::Wait);
        assert_eq!(
            relay_step(true, false, RELAY_RETRY_OURS_AFTER),
            RelayStep::DropFallback
        );
    }

    fn item(tag: &'static str) -> Item {
        let id = iroh::SecretKey::from_bytes(&[7; 32]).public();
        Item::new(EndpointInfo::new(id), tag, None)
    }

    fn found(tag: &'static str, after: Duration) -> ItemStream {
        Box::pin(n0_future::stream::once_future(async move {
            tokio::time::sleep(after).await;
            Ok(item(tag))
        }))
    }

    fn not_found() -> ItemStream {
        Box::pin(n0_future::stream::once(Err(LookupError::from_err(
            "test",
            std::io::Error::other("not found"),
        ))))
    }

    fn silent() -> ItemStream {
        Box::pin(n0_future::stream::pending())
    }

    /// Runs a resolution over `tiers`; returns the provenance of each item
    /// and how many tiers started.
    async fn resolve(tiers: Vec<Vec<ItemStream>>, head_start: Duration) -> (Vec<String>, usize) {
        let tiers = Arc::new(Mutex::new(tiers.into_iter().map(Some).collect::<Vec<_>>()));
        let started = Arc::new(Mutex::new(0));
        let start = {
            let tiers = Arc::clone(&tiers);
            let started = Arc::clone(&started);
            move |tier: usize| {
                let streams = tiers.lock().unwrap().get_mut(tier)?.take()?;
                *started.lock().unwrap() += 1;
                Some(streams)
            }
        };
        let mut stream = OrderedResolve::new(start, head_start);
        let mut items = Vec::new();
        let deadline = Duration::from_secs(5);
        while let Ok(Some(next)) = tokio::time::timeout(deadline, stream.next()).await {
            if let Ok(item) = next {
                items.push(item.provenance().to_string());
            }
            if !items.is_empty() {
                break;
            }
        }
        let started = *started.lock().unwrap();
        (items, started)
    }

    #[tokio::test]
    async fn resolves_ours_first() {
        let fast = Duration::from_millis(10);
        let head_start = Duration::from_millis(500);
        // Ours answers: n0 is never asked.
        let (items, started) = resolve(
            vec![vec![found("ours", fast)], vec![found("n0", Duration::ZERO)]],
            head_start,
        )
        .await;
        assert_eq!((items, started), (vec!["ours".to_string()], 1));
        // Ours does not know the peer: n0 is asked at once.
        let begin = tokio::time::Instant::now();
        let (items, started) = resolve(
            vec![vec![not_found(), not_found()], vec![found("n0", fast)]],
            head_start,
        )
        .await;
        assert_eq!((items, started), (vec!["n0".to_string()], 2));
        assert!(begin.elapsed() < head_start);
        // Ours is silent: n0 is asked after the head start.
        let begin = tokio::time::Instant::now();
        let (items, started) =
            resolve(vec![vec![silent()], vec![found("n0", fast)]], head_start).await;
        assert_eq!((items, started), (vec!["n0".to_string()], 2));
        assert!(begin.elapsed() >= head_start);
    }

    #[tokio::test]
    async fn endpoints_build_for_every_profile() {
        for infra in [Infra::N0, Infra::Wats, Infra::WatsAndN0, Infra::None] {
            let endpoint = infra.builder().unwrap().bind().await.unwrap();
            endpoint.close().await;
        }
    }
}
