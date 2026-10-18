//! Bootstrap peers (ADR 0008).
//!
//! An entry is `<endpoint-id>@<host>:<port>`. The host is resolved with DNS
//! at startup and again at every reconnection attempt (see
//! [`crate::reconnect`]), so the IP can move and a DNS failure is never
//! final; the endpoint id is pinned, because Iroh
//! authenticates every connection against the dialed id: a host answering
//! with another key is simply not connected to.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use tracing::warn;

/// Bootstrap nodes compiled into the binary, used unless disabled with
/// `--no-default-bootstrap` / `WATS_NO_DEFAULT_BOOTSTRAP`.
///
/// `router.wearetheswarm.ai`: the public bootstrap node, deployed
/// 2026-10-15 (ADR 0008). The endpoint id is pinned; the host is resolved
/// by DNS at startup, so the VM can move without a new release.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "9132c3e3e9a3313114adecc9a45fba05721ec2125fcd3f51494a376acdd603d8@router.wearetheswarm.ai:7842",
];

/// The UDP port the public bootstrap node listens on (provisional, ADR 0008).
pub const DEFAULT_BOOTSTRAP_PORT: u16 = 7842;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapEntry {
    pub id: EndpointId,
    pub host: String,
    pub port: u16,
}

impl std::str::FromStr for BootstrapEntry {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (id, host_port) = s
            .trim()
            .split_once('@')
            .context("bootstrap entry must be <endpoint-id>@<host>:<port>")?;
        let id: EndpointId = id
            .parse()
            .context("invalid endpoint id in bootstrap entry")?;
        let (host, port) = host_port
            .rsplit_once(':')
            .context("bootstrap entry needs a port: <endpoint-id>@<host>:<port>")?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            bail!("bootstrap entry has an empty host");
        }
        let port: u16 = port.parse().context("invalid port in bootstrap entry")?;
        Ok(Self {
            id,
            host: host.to_string(),
            port,
        })
    }
}

impl std::fmt::Display for BootstrapEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "{}@[{}]:{}", self.id, self.host, self.port)
        } else {
            write!(f, "{}@{}:{}", self.id, self.host, self.port)
        }
    }
}

impl BootstrapEntry {
    /// Resolve the host. The id stays pinned whatever the DNS says.
    pub async fn resolve(&self) -> Result<EndpointAddr> {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .with_context(|| format!("resolve {}", self.host))?
            .collect();
        if addrs.is_empty() {
            bail!("{} has no address", self.host);
        }
        Ok(EndpointAddr::from_parts(
            self.id,
            addrs.into_iter().map(TransportAddr::Ip),
        ))
    }
}

type ResolveFn = dyn Fn(BootstrapEntry) -> Pin<Box<dyn Future<Output = Result<EndpointAddr>> + Send>>
    + Send
    + Sync;

/// How bootstrap entries are resolved: DNS, or a function (tests that make
/// DNS fail, then come back).
#[derive(Clone, Default)]
pub struct Resolver(Option<Arc<ResolveFn>>);

impl Resolver {
    pub fn custom<F, Fut>(resolve: F) -> Self
    where
        F: Fn(BootstrapEntry) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<EndpointAddr>> + Send + 'static,
    {
        Resolver(Some(Arc::new(move |entry| Box::pin(resolve(entry)))))
    }

    pub async fn resolve(&self, entry: &BootstrapEntry) -> Result<EndpointAddr> {
        match &self.0 {
            None => entry.resolve().await,
            Some(resolve) => resolve(entry.clone()).await,
        }
    }
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Resolver(custom)"
        } else {
            "Resolver(dns)"
        })
    }
}

/// The compiled-in list, parsed.
pub fn defaults() -> Vec<BootstrapEntry> {
    DEFAULT_BOOTSTRAP
        .iter()
        .filter_map(|s| match s.parse() {
            Ok(entry) => Some(entry),
            Err(e) => {
                warn!("ignoring invalid built-in bootstrap entry {s:?}: {e:#}");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";

    #[test]
    fn parses_and_prints_entries() {
        let entry: BootstrapEntry = format!("{ID}@router.wearetheswarm.ai:7842")
            .parse()
            .unwrap();
        assert_eq!(entry.host, "router.wearetheswarm.ai");
        assert_eq!(entry.port, 7842);
        assert_eq!(
            entry.to_string(),
            format!("{ID}@router.wearetheswarm.ai:7842")
        );
        let v6: BootstrapEntry = format!("{ID}@[::1]:7842").parse().unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.to_string(), format!("{ID}@[::1]:7842"));
        for bad in [
            "router.wearetheswarm.ai:7842".to_string(),
            format!("{ID}@router.wearetheswarm.ai"),
            "nothex@router:1".to_string(),
            format!("{ID}@:7842"),
        ] {
            assert!(bad.parse::<BootstrapEntry>().is_err(), "{bad}");
        }
    }

    #[test]
    fn built_in_list_is_valid() {
        assert_eq!(defaults().len(), DEFAULT_BOOTSTRAP.len());
    }

    #[tokio::test]
    async fn resolves_localhost_and_keeps_the_id() {
        let entry: BootstrapEntry = format!("{ID}@localhost:7842").parse().unwrap();
        let addr = entry.resolve().await.unwrap();
        assert_eq!(addr.id.to_string(), ID);
        assert!(addr.ip_addrs().any(|a| a.ip().is_loopback()));
    }
}
