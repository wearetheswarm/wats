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
//! Presets (`infra`):
//! - `wats+n0` (default): WeAreTheSwarm's own relay and DNS first, n0's as
//!   fallback. The address record is published to both directories and
//!   resolved through ours first. Our relay is the only home relay
//!   candidate while it is reachable; n0's relays join only while it is not
//!   (see [`relay_step`]);
//! - `wats`: WeAreTheSwarm's own relay and DNS only, no third party;
//! - `n0`: Iroh's public services operated by n0 (the Iroh defaults);
//! - `none`: no relay and no lookup; peers must be dialed by address
//!   (bootstrap entries, tickets, local networks);
//! - `custom`: only the relays and directory given below, nothing else.
//!
//! Custom services (ADR 0009, 2026-10-20): `infra_relays` / `--relay` and
//! `infra_dns` / `--dns` *replace* the preset's relays and directory. What
//! they do not replace comes from `infra` when it names a preset, and is
//! left out otherwise (`custom`, implied when `infra` is unset).

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
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
/// The QUIC address-discovery port of a custom https relay when its URL
/// does not say (`#quic=<port>`): Iroh's default.
pub const DEFAULT_RELAY_QUIC_PORT: u16 = iroh_relay::defaults::DEFAULT_RELAY_QUIC_PORT;
/// The path of the pkarr API on an `iroh-dns-server`, added to a custom
/// directory URL that has no path.
pub const PKARR_PATH: &str = "/pkarr";

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

/// The `infra` value: a preset of relays and directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
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
    /// Only the custom relays and directory: what they leave out, the node
    /// goes without.
    Custom,
}

impl std::str::FromStr for Preset {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "n0" => Ok(Preset::N0),
            "wats" => Ok(Preset::Wats),
            "wats+n0" => Ok(Preset::WatsAndN0),
            "none" => Ok(Preset::None),
            "custom" => Ok(Preset::Custom),
            other => bail!("unknown infra {other:?}: use wats+n0, wats, n0, none or custom"),
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Preset::N0 => "n0",
            Preset::Wats => "wats",
            Preset::WatsAndN0 => "wats+n0",
            Preset::None => "none",
            Preset::Custom => "custom",
        })
    }
}

/// An address directory: pkarr over HTTPS (publish and resolve) and DNS
/// (resolve).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directory {
    pub pkarr_url: Cow<'static, str>,
    /// The zone of the `_iroh.<z32 id>.<origin>` TXT records; `None`: pkarr
    /// over HTTPS only.
    pub dns_origin: Option<Cow<'static, str>>,
}

/// WeAreTheSwarm's directory.
pub const WATS_DIRECTORY: Directory = Directory {
    pkarr_url: Cow::Borrowed(WATS_PKARR_URL),
    dns_origin: Some(Cow::Borrowed(WATS_DNS_ORIGIN)),
};
/// n0's directory (`dns.iroh.link`).
pub const N0_DIRECTORY: Directory = Directory {
    pkarr_url: Cow::Borrowed(N0_DNS_PKARR_RELAY_PROD),
    dns_origin: Some(Cow::Borrowed(N0_DNS_ENDPOINT_ORIGIN_PROD)),
};

impl Directory {
    /// The host of its pkarr URL, for display.
    pub fn host(&self) -> String {
        url_host(&self.pkarr_url)
    }
}

/// A custom directory, `infra_dns` / `--dns`: the pkarr URL of an
/// `iroh-dns-server`, `https://dns.example.org/pkarr`. Without a path,
/// [`PKARR_PATH`] is added. The DNS origin is the URL's host, unless the
/// fragment says otherwise: `#origin=<zone>`, or `#origin=none` for pkarr
/// over HTTPS only (the default when the host is an IP address or
/// `localhost`).
impl std::str::FromStr for Directory {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        let (mut url, fragment) = service_url(raw, "dns")?;
        if url.path().is_empty() || url.path() == "/" {
            url.set_path(PKARR_PATH);
        }
        let derived = match url.host() {
            Some(url::Host::Domain(name)) if name != "localhost" && name.contains('.') => {
                Some(name.trim_end_matches('.').to_ascii_lowercase())
            }
            _ => None,
        };
        let dns_origin = match fragment.as_deref() {
            None => derived,
            Some(fragment) => match fragment.strip_prefix("origin=") {
                Some("none") => None,
                Some(origin) => Some(check_origin(origin).with_context(|| format!("dns {raw:?}"))?),
                None => bail!(
                    "dns {raw:?}: unknown #{fragment}; the fragment can only be #origin=<zone> or #origin=none"
                ),
            },
        };
        Ok(Directory {
            pkarr_url: Cow::Owned(url.to_string()),
            dns_origin: dns_origin.map(Cow::Owned),
        })
    }
}

/// A DNS zone: dot-separated labels of 1-63 chars of `[a-z0-9_-]`, 253
/// chars at most; a trailing dot is dropped.
fn check_origin(origin: &str) -> Result<String> {
    let origin = origin.trim_end_matches('.').to_ascii_lowercase();
    ensure!(
        !origin.is_empty() && origin.len() <= 253,
        "#origin={origin:?} is not a DNS name"
    );
    for label in origin.split('.') {
        ensure!(
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && !label.starts_with('-')
                && !label.ends_with('-'),
            "#origin={origin:?} is not a DNS name"
        );
    }
    Ok(origin)
}

/// A custom relay, an item of `infra_relays` / `--relay`: the URL of an
/// `iroh-relay`, `https://relay.example.org`, with its QUIC
/// address-discovery port in the fragment when it is not
/// [`DEFAULT_RELAY_QUIC_PORT`]: `#quic=7843`, or `#quic=off`. A plain http
/// relay (loopback only) has no QUIC address discovery, which needs TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomRelay {
    pub url: RelayUrl,
    pub quic_port: Option<u16>,
}

impl std::str::FromStr for CustomRelay {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        let (url, fragment) = service_url(raw, "relay")?;
        ensure!(
            url.path() == "/" || url.path().is_empty(),
            "relay {raw:?}: a relay URL has no path (Iroh adds /relay itself)"
        );
        let quic_port = match fragment.as_deref() {
            None if url.scheme() == "http" => None,
            None => Some(DEFAULT_RELAY_QUIC_PORT),
            Some(fragment) => match fragment.strip_prefix("quic=") {
                Some("off") => None,
                Some(port) => match port.parse::<u16>() {
                    Ok(port) if port > 0 => Some(port),
                    _ => bail!("relay {raw:?}: #quic={port} is not a port (1-65535) or off"),
                },
                None => bail!(
                    "relay {raw:?}: unknown #{fragment}; the fragment can only be #quic=<port> or #quic=off"
                ),
            },
        };
        let url: RelayUrl = url
            .as_str()
            .parse()
            .with_context(|| format!("relay {raw:?}"))?;
        Ok(CustomRelay { url, quic_port })
    }
}

impl CustomRelay {
    fn config(&self) -> RelayConfig {
        RelayConfig::new(self.url.clone(), self.quic_port.map(RelayQuicConfig::new))
    }
}

/// A service URL, checked: https, or http to localhost or a loopback
/// address (tests); a host; no user, password or query. Returns it
/// without its fragment, and the fragment.
fn service_url(raw: &str, what: &str) -> Result<(url::Url, Option<String>)> {
    let raw = raw.trim();
    let mut url = url::Url::parse(raw).with_context(|| format!("{what} {raw:?} is not a URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => bail!("{what} {raw:?} has no host"),
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => bail!("{what} {raw:?}: use https (plain http only to localhost, for tests)"),
        other => bail!("{what} {raw:?}: {other}: is not https"),
    }
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{what} {raw:?}: no user or password in the URL"
    );
    ensure!(url.query().is_none(), "{what} {raw:?}: no query in the URL");
    let fragment = url.fragment().map(str::to_string);
    url.set_fragment(None);
    Ok((url, fragment))
}

/// "host" or "host:port" of a URL, for display; the input when it does not
/// parse.
fn url_host(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{}:{port}", host.trim_end_matches('.')),
            (Some(host), None) => host.trim_end_matches('.').to_string(),
            _ => url.to_string(),
        },
        Err(_) => url.to_string(),
    }
}

/// The infrastructure of a node: a preset, and the custom services that
/// replace its relays or its directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Infra {
    pub preset: Preset,
    /// Replace the preset's relays when not empty.
    pub relays: Vec<CustomRelay>,
    /// Replaces the preset's directories when set.
    pub dns: Option<Directory>,
}

/// What an endpoint is built with ([`Infra::endpoint_plan`]).
#[derive(Debug, Clone)]
pub struct EndpointPlan {
    /// Iroh's own `n0` preset, unchanged.
    pub n0_preset: bool,
    /// The relay map at start; `None`: relays disabled.
    pub relays: Option<RelayMap>,
    /// Directories the address record is published to.
    pub publish: Vec<Directory>,
    /// Directories peers are resolved through, in order.
    pub resolve: Vec<Directory>,
}

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
    /// No relay, no lookup.
    pub const NONE: Infra = Infra {
        preset: Preset::None,
        relays: Vec::new(),
        dns: None,
    };

    /// A preset with no custom service.
    pub fn preset(preset: Preset) -> Infra {
        Infra {
            preset,
            relays: Vec::new(),
            dns: None,
        }
    }

    /// The infrastructure from `infra`, `infra_relays` and `infra_dns` (or
    /// `--infra`, `--relay` and `--dns`), as one group: `infra` unset with
    /// custom services means `custom`; `none` takes none; `custom` needs at
    /// least one.
    pub fn resolve(infra: Option<&str>, relays: &[String], dns: Option<&str>) -> Result<Infra> {
        let relays: Vec<&str> = relays
            .iter()
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .collect();
        let dns = dns.map(str::trim).filter(|d| !d.is_empty());
        let infra = infra.map(str::trim).filter(|i| !i.is_empty());
        let custom = !relays.is_empty() || dns.is_some();
        let preset = match infra {
            Some(name) => name.parse()?,
            None if custom => Preset::Custom,
            None => Preset::default(),
        };
        match preset {
            Preset::None if custom => bail!(
                "infra none takes no relay or dns; leave infra unset (or custom) to use only \
                 the relays and dns given"
            ),
            Preset::Custom if !custom => bail!(
                "infra custom needs relays or a dns (infra_relays, infra_dns; --relay, --dns)"
            ),
            _ => {}
        }
        let mut parsed: Vec<CustomRelay> = Vec::new();
        for raw in relays {
            let relay: CustomRelay = raw.parse()?;
            ensure!(
                !parsed.iter().any(|r| r.url == relay.url),
                "relay {} is listed twice",
                relay.url
            );
            parsed.push(relay);
        }
        Ok(Infra {
            preset,
            relays: parsed,
            dns: dns.map(str::parse).transpose()?,
        })
    }

    /// Whether custom services replace any of the preset's.
    pub fn is_custom(&self) -> bool {
        self.preset == Preset::Custom || !self.relays.is_empty() || self.dns.is_some()
    }

    /// The preset name, or "<preset>+custom" when custom services replace
    /// some of it: what a crash report says, without host names.
    pub fn kind(&self) -> String {
        if self.preset != Preset::Custom && self.is_custom() {
            format!("{}+custom", self.preset)
        } else {
            self.preset.to_string()
        }
    }

    /// The relays by tier: the first tier while one of its relays is
    /// reachable, the next one otherwise.
    fn relay_config_tiers(&self) -> Result<Vec<Vec<RelayConfig>>> {
        if !self.relays.is_empty() {
            return Ok(vec![self.relays.iter().map(CustomRelay::config).collect()]);
        }
        let n0 = || -> Vec<RelayConfig> {
            n0_relays()
                .relays::<Vec<_>>()
                .into_iter()
                .map(|c| (*c).clone())
                .collect()
        };
        Ok(match self.preset {
            Preset::N0 => vec![n0()],
            Preset::Wats => vec![vec![wats_relay()?]],
            Preset::WatsAndN0 => vec![vec![wats_relay()?], n0()],
            Preset::None | Preset::Custom => vec![],
        })
    }

    /// What the endpoint is built with: pure, for tests.
    pub fn endpoint_plan(&self) -> Result<EndpointPlan> {
        let tiers = self.relay_config_tiers()?;
        let publish = self.publish_to();
        Ok(EndpointPlan {
            n0_preset: self.preset == Preset::N0 && !self.is_custom(),
            relays: tiers
                .into_iter()
                .next()
                .filter(|tier| !tier.is_empty())
                .map(RelayMap::from_iter),
            resolve: self.resolve_order(),
            publish,
        })
    }

    /// An endpoint builder for Internet mode with this infrastructure.
    ///
    /// For `wats+n0`, the relay map starts with our relay alone; call
    /// [`Infra::spawn_relay_fallback`] once the endpoint is bound so n0's
    /// relays join when ours is out of reach.
    pub fn builder(&self) -> Result<Builder> {
        let plan = self.endpoint_plan()?;
        if plan.n0_preset {
            return Ok(Endpoint::builder(presets::N0));
        }
        let mut builder = Endpoint::builder(presets::Minimal);
        for directory in &plan.publish {
            let pkarr: url::Url = directory.pkarr_url.parse().context("pkarr URL")?;
            builder = builder.address_lookup(PkarrPublisher::builder(pkarr));
        }
        if !plan.resolve.is_empty() {
            builder = builder.address_lookup(OrderedLookupBuilder {
                tiers: plan.resolve,
                head_start: RESOLVE_HEAD_START,
            });
        }
        Ok(builder.relay_mode(match plan.relays {
            Some(map) => RelayMode::Custom(map),
            None => RelayMode::Disabled,
        }))
    }

    /// The directories a node publishes its address record to.
    pub fn publish_to(&self) -> Vec<Directory> {
        if let Some(dns) = &self.dns {
            return vec![dns.clone()];
        }
        match self.preset {
            Preset::N0 => vec![N0_DIRECTORY],
            Preset::Wats => vec![WATS_DIRECTORY],
            Preset::WatsAndN0 => vec![WATS_DIRECTORY, N0_DIRECTORY],
            Preset::None | Preset::Custom => vec![],
        }
    }

    /// The directories a node resolves peers through, in order: each one
    /// is asked only when the ones before it found nothing, failed, or did
    /// not answer within [`RESOLVE_HEAD_START`]. (`n0` uses Iroh's preset,
    /// which asks its pkarr and DNS endpoints together.)
    pub fn resolve_order(&self) -> Vec<Directory> {
        self.publish_to()
    }

    /// Home relay candidates, by preference: the first list while one of
    /// its relays is reachable, the next one otherwise.
    pub fn relay_tiers(&self) -> Vec<Vec<String>> {
        self.relay_config_tiers()
            .unwrap_or_default()
            .into_iter()
            .map(|tier| {
                let mut urls: Vec<String> = tier.iter().map(|c| c.url.to_string()).collect();
                urls.sort();
                urls
            })
            .collect()
    }

    /// The relays nodes of this infrastructure may use, by preference, for
    /// display.
    pub fn relay_urls(&self) -> Vec<String> {
        self.relay_tiers().concat()
    }

    /// Whether the node has a relay, and so a relay check.
    pub fn has_relays(&self) -> bool {
        !self.relay_urls().is_empty()
    }

    /// Whether the node publishes to a directory, and so a lookup check.
    pub fn has_lookup(&self) -> bool {
        !self.publish_to().is_empty()
    }

    /// Whether n0's relays stand by for ours: `wats+n0` with its own
    /// relays.
    fn relay_fallback_on(&self) -> bool {
        self.preset == Preset::WatsAndN0 && self.relays.is_empty()
    }

    /// For `wats+n0`, watch the home relay and add n0's relays to the relay
    /// map while ours is out of reach (see [`relay_step`]). Stops when the
    /// endpoint closes. Does nothing for the other presets, nor when custom
    /// relays replace ours.
    pub fn spawn_relay_fallback(&self, endpoint: &Endpoint) {
        if !self.relay_fallback_on() {
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

impl std::fmt::Display for Infra {
    /// The preset, then the custom services by host: `wats+n0`,
    /// `custom (relay relay.example.org; dns dns.example.org)`,
    /// `wats (relays a.example.org, b.example.org)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.preset)?;
        if !self.is_custom() {
            return Ok(());
        }
        let mut parts = Vec::new();
        match self.relays.as_slice() {
            [] if self.preset == Preset::Custom => parts.push("no relay".to_string()),
            [] => {}
            [one] => parts.push(format!("relay {}", url_host(one.url.as_str()))),
            many => parts.push(format!(
                "relays {}",
                many.iter()
                    .map(|r| url_host(r.url.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
        match &self.dns {
            Some(dns) => parts.push(format!("dns {}", dns.host())),
            None if self.preset == Preset::Custom => parts.push("no lookup".to_string()),
            None => {}
        }
        write!(f, " ({})", parts.join("; "))
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
            let mut resolvers: Vec<Box<dyn AddressLookup>> = vec![Box::new(
                PkarrResolver::builder(pkarr).into_address_lookup(endpoint)?,
            )];
            if let Some(origin) = directory.dns_origin {
                resolvers.push(Box::new(
                    DnsAddressLookup::builder(origin.into_owned()).into_address_lookup(endpoint)?,
                ));
            }
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

    fn infra(name: &str) -> Infra {
        Infra::preset(name.parse().unwrap())
    }

    #[test]
    fn parses_presets() {
        for name in ["n0", "wats", "wats+n0", "none", "custom"] {
            assert_eq!(name.parse::<Preset>().unwrap().to_string(), name);
        }
        assert!("aws".parse::<Preset>().is_err());
    }

    #[test]
    fn default_is_ours_first_with_n0_fallback() {
        assert_eq!(Infra::default(), infra("wats+n0"));
        assert_eq!(Infra::default().to_string(), "wats+n0");
        assert_eq!(Infra::resolve(None, &[], None).unwrap(), Infra::default());
    }

    #[test]
    fn directories_are_ours_first() {
        assert_eq!(
            infra("wats+n0").publish_to(),
            vec![WATS_DIRECTORY, N0_DIRECTORY]
        );
        assert_eq!(
            infra("wats+n0").resolve_order(),
            vec![WATS_DIRECTORY, N0_DIRECTORY]
        );
        assert_eq!(infra("wats").publish_to(), vec![WATS_DIRECTORY]);
        assert_eq!(infra("wats").resolve_order(), vec![WATS_DIRECTORY]);
        assert_eq!(infra("n0").publish_to(), vec![N0_DIRECTORY]);
        assert!(Infra::NONE.publish_to().is_empty());
        assert_eq!(
            N0_DIRECTORY
                .dns_origin
                .as_deref()
                .map(|o| o.trim_end_matches('.')),
            Some("dns.iroh.link")
        );
        assert_eq!(
            WATS_DIRECTORY.dns_origin.as_deref(),
            Some("dns.wearetheswarm.ai")
        );
    }

    #[test]
    fn relay_sets() {
        let ours = vec![format!("{WATS_RELAY_URL}/")];
        assert_eq!(infra("wats").relay_urls(), ours);
        assert!(Infra::NONE.relay_urls().is_empty());
        let n0 = infra("n0").relay_urls();
        assert!(!n0.is_empty() && n0.iter().all(|u| u.contains("iroh.link")));
        assert_eq!(
            infra("wats+n0").relay_tiers(),
            vec![ours.clone(), n0.clone()]
        );
        let both = infra("wats+n0").relay_urls();
        assert_eq!(both.len(), n0.len() + 1);
        assert_eq!(both[0], ours[0]);
    }

    fn custom(infra: Option<&str>, relays: &[&str], dns: Option<&str>) -> Result<Infra> {
        let relays: Vec<String> = relays.iter().map(|r| r.to_string()).collect();
        Infra::resolve(infra, &relays, dns)
    }

    #[test]
    fn custom_relays_parse_with_their_quic_port() {
        let relay = |raw: &str| raw.parse::<CustomRelay>();
        let r = relay("https://relay.example.org").unwrap();
        assert_eq!(r.url.as_str(), "https://relay.example.org/");
        assert_eq!(r.quic_port, Some(DEFAULT_RELAY_QUIC_PORT));
        assert_eq!(DEFAULT_RELAY_QUIC_PORT, 7842);
        let r = relay(" https://relay.example.org:8443/#quic=7843 ").unwrap();
        assert_eq!(r.url.as_str(), "https://relay.example.org:8443/");
        assert_eq!(r.quic_port, Some(7843));
        assert_eq!(
            relay("https://relay.example.org#quic=off")
                .unwrap()
                .quic_port,
            None
        );
        // Plain http, loopback only, and no QUIC (it needs TLS).
        for raw in [
            "http://localhost:3340",
            "http://127.0.0.1:3340",
            "http://[::1]:3340",
        ] {
            assert_eq!(relay(raw).unwrap().quic_port, None, "{raw}");
        }
        for (raw, why) in [
            ("http://relay.example.org", "use https"),
            ("http://10.0.0.1", "use https"),
            ("ftp://relay.example.org", "not https"),
            ("relay.example.org", "not a URL"),
            ("https://", "not a URL"),
            ("https://user:pw@relay.example.org", "no user"),
            ("https://relay.example.org/?x=1", "no query"),
            ("https://relay.example.org/relay", "no path"),
            ("https://relay.example.org#quic=0", "not a port"),
            ("https://relay.example.org#quic=99999", "not a port"),
            ("https://relay.example.org#port=7843", "unknown #port"),
        ] {
            let err = format!("{:#}", relay(raw).unwrap_err());
            assert!(err.contains(why), "{raw}: {err}");
        }
    }

    #[test]
    fn custom_directories_derive_their_dns_origin() {
        let dir = |raw: &str| raw.parse::<Directory>();
        let d = dir("https://dns.example.org/pkarr").unwrap();
        assert_eq!(d.pkarr_url, "https://dns.example.org/pkarr");
        assert_eq!(d.dns_origin.as_deref(), Some("dns.example.org"));
        // No path: the iroh-dns-server's /pkarr.
        assert_eq!(
            dir("https://DNS.Example.org").unwrap().pkarr_url,
            "https://dns.example.org/pkarr"
        );
        // Another path is kept (a reverse proxy).
        assert_eq!(
            dir("https://example.org/iroh/pkarr").unwrap().pkarr_url,
            "https://example.org/iroh/pkarr"
        );
        let d = dir("https://pkarr.example.org/pkarr#origin=Peers.Example.org.").unwrap();
        assert_eq!(d.dns_origin.as_deref(), Some("peers.example.org"));
        let d = dir("https://dns.example.org/pkarr#origin=none").unwrap();
        assert_eq!(d.dns_origin, None);
        // Loopback: pkarr only.
        let d = dir("http://127.0.0.1:8080/pkarr").unwrap();
        assert_eq!(d.dns_origin, None);
        assert_eq!(dir("http://localhost:8080").unwrap().dns_origin, None);
        for (raw, why) in [
            ("http://dns.example.org/pkarr", "use https"),
            ("https://dns.example.org/pkarr?x", "no query"),
            (
                "https://dns.example.org/pkarr#origin=a..b",
                "not a DNS name",
            ),
            (
                "https://dns.example.org/pkarr#origin=-a.b",
                "not a DNS name",
            ),
            ("https://dns.example.org/pkarr#zone=a.b", "unknown #zone"),
        ] {
            let err = format!("{:#}", dir(raw).unwrap_err());
            assert!(err.contains(why), "{raw}: {err}");
        }
    }

    #[test]
    fn custom_services_replace_the_preset() {
        let relay = "https://relay.example.org";
        let dns = "https://dns.example.org/pkarr";
        let directory = || dns.parse::<Directory>().unwrap();
        // Only custom services: `custom` implied, nothing else.
        let both = custom(None, &[relay], Some(dns)).unwrap();
        assert_eq!(both.preset, Preset::Custom);
        assert_eq!(both.relay_urls(), vec!["https://relay.example.org/"]);
        assert_eq!(both.publish_to(), vec![directory()]);
        assert_eq!(
            both.to_string(),
            "custom (relay relay.example.org; dns dns.example.org)"
        );
        assert_eq!(both.kind(), "custom");
        assert_eq!(custom(Some("custom"), &[relay], Some(dns)).unwrap(), both);
        // One of them: the other is left out.
        let relays_only = custom(None, &[relay, "https://r2.example.org"], None).unwrap();
        assert!(relays_only.publish_to().is_empty() && !relays_only.has_lookup());
        assert_eq!(
            relays_only.to_string(),
            "custom (relays relay.example.org, r2.example.org; no lookup)"
        );
        let dns_only = custom(None, &[], Some(dns)).unwrap();
        assert!(!dns_only.has_relays() && dns_only.has_lookup());
        assert_eq!(
            dns_only.to_string(),
            "custom (no relay; dns dns.example.org)"
        );
        // With a preset named, it fills in what is not replaced.
        let wats = custom(Some("wats"), &[relay], None).unwrap();
        assert_eq!(wats.publish_to(), vec![WATS_DIRECTORY]);
        assert_eq!(
            wats.relay_tiers(),
            vec![vec!["https://relay.example.org/".to_string()]]
        );
        assert_eq!(wats.to_string(), "wats (relay relay.example.org)");
        assert_eq!(wats.kind(), "wats+custom");
        // wats+n0 with custom relays: no n0 relay fallback, both directories.
        let ours_first = custom(Some("wats+n0"), &[relay], None).unwrap();
        assert!(!ours_first.relay_fallback_on());
        assert_eq!(ours_first.relay_tiers().len(), 1);
        assert_eq!(ours_first.publish_to(), vec![WATS_DIRECTORY, N0_DIRECTORY]);
        assert!(infra("wats+n0").relay_fallback_on());
        let n0 = custom(Some("n0"), &[], Some(dns)).unwrap();
        assert_eq!(n0.relay_urls(), infra("n0").relay_urls());
        assert_eq!(n0.publish_to(), vec![directory()]);
        // Empty values are unset (`WATS_DNS=` in an env file).
        assert_eq!(
            custom(Some(" "), &[""], Some("")).unwrap(),
            Infra::default()
        );
        // Contradictions and garbage.
        type Case<'a> = (Option<&'a str>, Vec<&'a str>, Option<&'a str>, &'a str);
        let cases: [Case; 7] = [
            (Some("none"), vec![relay], None, "infra none takes no relay"),
            (Some("none"), vec![], Some(dns), "infra none takes no relay"),
            (Some("custom"), vec![], None, "needs relays or a dns"),
            (
                None,
                vec![relay, "https://relay.example.org/"],
                None,
                "listed twice",
            ),
            (None, vec!["http://relay.example.org"], None, "use https"),
            (None, vec![], Some("dns.example.org"), "not a URL"),
            (Some("aws"), vec![], None, "unknown infra"),
        ];
        for (infra, relays, dns, why) in cases {
            let err = format!("{:#}", custom(infra, &relays, dns).unwrap_err());
            assert!(err.contains(why), "{infra:?} {relays:?} {dns:?}: {err}");
        }
    }

    #[test]
    fn the_endpoint_plan_carries_the_custom_services() {
        let services = custom(
            None,
            &[
                "https://relay.example.org#quic=7843",
                "https://relay2.example.org#quic=off",
            ],
            Some("https://pkarr.example.org#origin=peers.example.org"),
        )
        .unwrap();
        let plan = services.endpoint_plan().unwrap();
        assert!(!plan.n0_preset);
        let mut relays: Vec<(String, Option<u16>)> = plan
            .relays
            .unwrap()
            .relays::<Vec<_>>()
            .iter()
            .map(|c| (c.url.to_string(), c.quic.as_ref().map(|q| q.port)))
            .collect();
        relays.sort();
        assert_eq!(
            relays,
            vec![
                ("https://relay.example.org/".to_string(), Some(7843)),
                ("https://relay2.example.org/".to_string(), None),
            ]
        );
        let directory = Directory {
            pkarr_url: "https://pkarr.example.org/pkarr".into(),
            dns_origin: Some("peers.example.org".into()),
        };
        assert_eq!(plan.publish, vec![directory.clone()]);
        assert_eq!(plan.resolve, vec![directory]);
        // Custom relays only: relays, no lookup at all.
        let plan = custom(None, &["https://relay.example.org"], None)
            .unwrap()
            .endpoint_plan()
            .unwrap();
        assert!(plan.relays.is_some() && plan.publish.is_empty() && plan.resolve.is_empty());
        // Custom dns only: relays disabled.
        let plan = custom(None, &[], Some("https://dns.example.org"))
            .unwrap()
            .endpoint_plan()
            .unwrap();
        assert!(plan.relays.is_none() && plan.publish.len() == 1);
        // The presets: n0 is Iroh's own unless a custom service replaces
        // part of it; wats+n0 starts with ours alone.
        assert!(infra("n0").endpoint_plan().unwrap().n0_preset);
        let n0_custom = custom(Some("n0"), &[], Some("https://dns.example.org")).unwrap();
        assert!(!n0_custom.endpoint_plan().unwrap().n0_preset);
        let plan = infra("wats+n0").endpoint_plan().unwrap();
        assert_eq!(
            plan.relays.unwrap().urls::<Vec<_>>(),
            vec![WATS_RELAY_URL.parse::<RelayUrl>().unwrap()]
        );
        assert!(Infra::NONE.endpoint_plan().unwrap().relays.is_none());
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
        // Custom services on this machine only (nothing listens there).
        let services = custom(
            None,
            &["http://127.0.0.1:9#quic=off"],
            Some("http://127.0.0.1:9/pkarr"),
        )
        .unwrap();
        for infra in [
            infra("n0"),
            infra("wats"),
            infra("wats+n0"),
            Infra::NONE,
            services,
        ] {
            let endpoint = infra.builder().unwrap().bind().await.unwrap();
            endpoint.close().await;
        }
    }
}
