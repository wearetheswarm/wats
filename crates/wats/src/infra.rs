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
//! - `wats` (default): WeAreTheSwarm's own relay and DNS, no third party;
//! - `n0`: Iroh's public services operated by n0 (the Iroh defaults);
//! - `wats+n0`: both (redundancy, at the cost of silent fallback);
//! - `none`: no relay and no lookup; peers must be dialed by address
//!   (bootstrap entries, tickets, local networks).

use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver};
use iroh::endpoint::{Builder, presets};
use iroh::{Endpoint, RelayConfig, RelayMap, RelayMode, RelayUrl};
use iroh_relay::RelayQuicConfig;

/// WeAreTheSwarm relay (provisional, ADR 0009).
pub const WATS_RELAY_URL: &str = "https://relay.wearetheswarm.ai";
/// Its QUIC address-discovery port: 7843, because the router node holds
/// 7842 on the same VM.
pub const WATS_RELAY_QUIC_PORT: u16 = 7843;
/// WeAreTheSwarm pkarr server (publish and resolve over HTTPS).
pub const WATS_PKARR_URL: &str = "https://dns.wearetheswarm.ai/pkarr";
/// DNS origin for lookups: `_iroh.<z32 id>.dns.wearetheswarm.ai` TXT.
pub const WATS_DNS_ORIGIN: &str = "dns.wearetheswarm.ai";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Infra {
    /// Iroh's public relays and DNS, operated by n0.
    N0,
    /// WeAreTheSwarm's own relay and DNS only. Default: failures of our
    /// infrastructure stay visible instead of falling back to n0.
    #[default]
    Wats,
    /// Both: our relay and n0's in one relay map, publish and resolve with
    /// both DNS servers.
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
            other => anyhow::bail!("unknown infra {other:?}: use n0, wats, wats+n0 or none"),
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

fn wats_relay() -> Result<RelayConfig> {
    let url: RelayUrl = WATS_RELAY_URL.parse().context("WATS_RELAY_URL")?;
    Ok(RelayConfig::new(
        url,
        Some(RelayQuicConfig::new(WATS_RELAY_QUIC_PORT)),
    ))
}

fn add_wats_lookup(builder: Builder) -> Result<Builder> {
    let pkarr: url::Url = WATS_PKARR_URL.parse().context("WATS_PKARR_URL")?;
    Ok(builder
        .address_lookup(PkarrPublisher::builder(pkarr.clone()))
        .address_lookup(PkarrResolver::builder(pkarr))
        .address_lookup(DnsAddressLookup::builder(WATS_DNS_ORIGIN.to_string())))
}

impl Infra {
    /// An endpoint builder for Internet mode with this infrastructure.
    pub fn builder(self) -> Result<Builder> {
        Ok(match self {
            Infra::N0 => Endpoint::builder(presets::N0),
            Infra::Wats => add_wats_lookup(Endpoint::builder(presets::Minimal))?
                .relay_mode(RelayMode::Custom(RelayMap::from_iter([wats_relay()?]))),
            Infra::WatsAndN0 => {
                // n0's lookups and relays, plus ours.
                let map = RelayMode::Default.relay_map();
                let ours = wats_relay()?;
                map.insert(ours.url.clone(), Arc::new(ours));
                add_wats_lookup(Endpoint::builder(presets::N0))?.relay_mode(RelayMode::Custom(map))
            }
            Infra::None => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        })
    }

    /// The relays nodes of this profile use, for display.
    pub fn relay_urls(self) -> Vec<String> {
        let map = match self {
            Infra::N0 => RelayMode::Default.relay_map(),
            Infra::Wats => RelayMap::from_iter(wats_relay().ok()),
            Infra::WatsAndN0 => {
                let map = RelayMode::Default.relay_map();
                if let Ok(ours) = wats_relay() {
                    map.insert(ours.url.clone(), Arc::new(ours));
                }
                map
            }
            Infra::None => RelayMap::empty(),
        };
        let mut urls: Vec<String> = map.urls::<Vec<_>>().iter().map(|u| u.to_string()).collect();
        urls.sort();
        urls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles() {
        for name in ["n0", "wats", "wats+n0", "none"] {
            assert_eq!(name.parse::<Infra>().unwrap().to_string(), name);
        }
        assert!("aws".parse::<Infra>().is_err());
        assert_eq!(Infra::default(), Infra::Wats);
    }

    #[test]
    fn relay_sets() {
        assert_eq!(Infra::Wats.relay_urls(), vec![format!("{WATS_RELAY_URL}/")]);
        assert!(Infra::None.relay_urls().is_empty());
        let n0 = Infra::N0.relay_urls();
        assert!(!n0.is_empty() && n0.iter().all(|u| u.contains("iroh.link")));
        let both = Infra::WatsAndN0.relay_urls();
        assert_eq!(both.len(), n0.len() + 1);
    }

    #[tokio::test]
    async fn endpoints_build_for_every_profile() {
        for infra in [Infra::N0, Infra::Wats, Infra::WatsAndN0, Infra::None] {
            let endpoint = infra.builder().unwrap().bind().await.unwrap();
            endpoint.close().await;
        }
    }
}
