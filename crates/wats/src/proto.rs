//! The wats peer protocol (ALPN `/wats/1`), next to gossip and blobs:
//!
//! - `sync`: page through a peer's events of a network, to catch up on what
//!   gossip delivered while this node was offline (SPEC §2.6, full
//!   replication);
//! - `replicate`: ask a peer to hold the blobs of an event (SPEC §2.5,
//!   replication k at publish time).
//!
//! - `hello`: exchange supported protocol and event versions and features
//!   (ADR 0007).
//!
//! One request per QUIC bi-stream: the requester writes a JSON request and
//! finishes the stream, the responder writes a JSON response. See
//! `docs/adr/0005-sync-and-replication.md` and
//! `docs/adr/0007-protocol-evolution.md`.
//!
//! Forward compatibility: unknown request types get `unsupported`, unknown
//! fields are ignored, and every node serves each ALPN in [`ALPNS`].

use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use tracing::debug;

use serde_json::Value;

use crate::node::Node;
use crate::wire::{MAX_RELAY_VERSION, UNDERSTOOD_VERSIONS};

/// Protocol versions this node speaks, newest first. `/wats/2` goes in
/// front of `/wats/1` when it exists; both are then served side by side
/// for the deprecation window of ADR 0007.
pub const ALPNS: &[&[u8]] = &[b"/wats/1"];
/// The same, as numbers, for `hello`.
pub const PROTOCOL_VERSIONS: &[u32] = &[1];
/// Optional capabilities, for `hello`.
pub const FEATURES: &[&str] = &["sync", "replicate"];

/// Largest request a node accepts.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Largest response a node accepts.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Most events in one sync page.
pub const MAX_SYNC_PAGE: u32 = 256;
/// Most requests served on one connection.
const MAX_REQUESTS_PER_CONNECTION: usize = 1024;

/// What a node says about itself in `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Free-form software name and version, e.g. `wats/0.0.1`.
    pub software: String,
    /// `/wats/N` protocol versions spoken.
    pub protocols: Vec<u32>,
    /// Event versions understood (indexed and shown), inclusive.
    pub events_min: u64,
    pub events_max: u64,
    /// Highest event version stored and relayed as opaque data.
    pub relay_max: u64,
    pub features: Vec<String>,
}

impl NodeInfo {
    /// [`NodeInfo::ours`], with a GREASE feature now and then.
    pub fn ours_greased() -> Self {
        let mut info = Self::ours();
        if crate::grease::roll() {
            info.features.push(crate::grease::name());
        }
        info
    }

    pub fn ours() -> Self {
        Self {
            software: format!("wats/{}", env!("CARGO_PKG_VERSION")),
            protocols: PROTOCOL_VERSIONS.to_vec(),
            events_min: UNDERSTOOD_VERSIONS.0,
            events_max: UNDERSTOOD_VERSIONS.1,
            relay_max: MAX_RELAY_VERSION,
            features: FEATURES.iter().map(|f| f.to_string()).collect(),
        }
    }

    /// Whether two nodes understand at least one common event version.
    pub fn shares_event_version(&self, other: &NodeInfo) -> bool {
        self.events_min <= other.events_max && other.events_min <= self.events_max
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        info: NodeInfo,
    },
    /// Events of `net` strictly after `(after_ts, after_id)` in `(ts, id)`
    /// order.
    Sync {
        net: String,
        after_ts: u64,
        after_id: String,
        limit: u32,
    },
    /// Store `event` and fetch the listed blobs of it from the requester.
    Replicate {
        event: Value,
        hashes: Vec<String>,
    },
    /// A request type from a newer protocol revision.
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        info: NodeInfo,
    },
    /// Events as wire JSON: understood or not, they are all relayed.
    SyncPage {
        events: Vec<Value>,
        more: bool,
    },
    Replicated {
        hashes: Vec<String>,
    },
    Refused {
        reason: String,
    },
    /// The peer does not know this request type.
    Unsupported,
    /// A response type from a newer protocol revision.
    #[serde(other)]
    Unknown,
}

/// Send one request to a peer and wait for the response.
pub async fn call(endpoint: &Endpoint, peer: EndpointId, request: &Request) -> Result<Response> {
    let mut value = serde_json::to_value(request)?;
    crate::grease::maybe_add_to(&mut value);
    call_value(endpoint, peer, value).await
}

/// Send a request given as JSON (used for GREASE request types).
pub async fn call_value(endpoint: &Endpoint, peer: EndpointId, request: Value) -> Result<Response> {
    // Newest protocol first; fall back to older ones.
    let mut last_err = None;
    let mut connected = None;
    for alpn in ALPNS {
        match endpoint.connect(peer, alpn).await {
            Ok(conn) => {
                connected = Some(conn);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let conn = match (connected, last_err) {
        (Some(conn), _) => conn,
        (None, Some(e)) => {
            return Err(e).with_context(|| format!("connect to {}", peer.fmt_short()));
        }
        (None, None) => bail!("no protocol version configured"),
    };
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&serde_json::to_vec(&request)?).await?;
    send.finish()?;
    let bytes = recv.read_to_end(MAX_RESPONSE_BYTES).await?;
    conn.close(0u32.into(), b"done");
    // Untrusted: parse under the strict profile (ADR 0010, A3).
    Ok(crate::strict_json::from_slice(&bytes)?)
}

/// Serves the protocol for a node. The node is set after it is built, since
/// the router is created first.
#[derive(Debug, Clone, Default)]
pub struct WatsProtocol {
    node: Arc<OnceLock<Weak<Node>>>,
}

impl WatsProtocol {
    pub fn set_node(&self, node: &Arc<Node>) {
        let _ = self.node.set(Arc::downgrade(node));
    }

    async fn serve(&self, conn: Connection) -> Result<()> {
        let peer = conn.remote_id();
        for _ in 0..MAX_REQUESTS_PER_CONNECTION {
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                break;
            };
            let Some(node) = self.node.get().and_then(Weak::upgrade) else {
                bail!("node is shutting down");
            };
            let bytes = recv.read_to_end(MAX_REQUEST_BYTES).await?;
            let response = match crate::strict_json::from_slice::<Request>(&bytes) {
                Ok(Request::Unsupported) => Response::Unsupported,
                Ok(request) => node.handle_request(peer, request).await,
                Err(e) => Response::Refused {
                    reason: format!("malformed request: {e}"),
                },
            };
            let mut response = serde_json::to_value(&response)?;
            crate::grease::maybe_add_to(&mut response);
            send.write_all(&serde_json::to_vec(&response)?).await?;
            send.finish()?;
        }
        Ok(())
    }
}

impl ProtocolHandler for WatsProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();
        if let Err(e) = self.serve(connection).await {
            debug!(peer = %peer.fmt_short(), "wats protocol: {e:#}");
        }
        Ok(())
    }
}

/// XOR distance between a content hash and a node id, for "replication by
/// hash proximity" (SPEC §2.5). Smaller is closer.
pub fn distance(hash: &[u8; 32], node: &EndpointId) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, (a, b)) in hash.iter().zip(node.as_bytes()).enumerate() {
        out[i] = a ^ b;
    }
    out
}

/// The `k` peers closest to `hash`.
pub fn closest(hash: &[u8; 32], peers: &[EndpointId], k: usize) -> Vec<EndpointId> {
    let mut sorted = peers.to_vec();
    sorted.sort_by_key(|p| distance(hash, p));
    sorted.dedup();
    sorted.truncate(k);
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn closest_is_deterministic_and_bounded() {
        let peers: Vec<EndpointId> = (1..=10u8)
            .map(|i| SecretKey::from_bytes(&[i; 32]).public())
            .collect();
        let hash = *blake3::hash(b"blob").as_bytes();
        let a = closest(&hash, &peers, 3);
        let mut reversed = peers.clone();
        reversed.reverse();
        assert_eq!(a, closest(&hash, &reversed, 3));
        assert_eq!(a.len(), 3);
        assert_eq!(closest(&hash, &peers[..2], 3).len(), 2);
        let d0 = distance(&hash, &a[0]);
        assert!(peers.iter().all(|p| distance(&hash, p) >= d0));
    }

    #[test]
    fn newer_peers_are_understood() {
        // A hello from a future node, with fields this version ignores.
        let hello: Request = serde_json::from_str(
            r#"{"type":"hello","info":{"software":"wats/9.0.0","protocols":[2,1],
                "events_min":1,"events_max":3,"relay_max":65535,
                "features":["sync","replicate","reconcile"],"region":"eu"}}"#,
        )
        .unwrap();
        let Request::Hello { info } = hello else {
            panic!("hello expected");
        };
        assert!(NodeInfo::ours().shares_event_version(&info));
        let old = NodeInfo {
            events_min: 5,
            events_max: 6,
            ..info
        };
        assert!(!NodeInfo::ours().shares_event_version(&old));
        // Unknown request and response types do not break the parser.
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"type":"reconcile","ranges":[]}"#).unwrap(),
            Request::Unsupported
        ));
        assert!(matches!(
            serde_json::from_str::<Response>(r#"{"type":"fingerprint","x":1}"#).unwrap(),
            Response::Unknown
        ));
    }

    #[test]
    fn grease_is_ignored_in_requests_and_responses() {
        let request: Request = crate::strict_json::from_slice(
            br#"{"type":"sync","net":"public","after_ts":0,"after_id":"","limit":5,"_grease_0a1b":{"_grease_x":1}}"#,
        )
        .unwrap();
        assert!(matches!(request, Request::Sync { limit: 5, .. }));
        let grease_type: Request =
            crate::strict_json::from_slice(br#"{"type":"_grease_9c3d"}"#).unwrap();
        assert!(matches!(grease_type, Request::Unsupported));
        let response: Response = crate::strict_json::from_slice(
            br#"{"type":"replicated","hashes":[],"_grease_77":true}"#,
        )
        .unwrap();
        assert!(matches!(response, Response::Replicated { .. }));
    }

    #[test]
    fn requests_roundtrip_as_json() {
        let request = Request::Sync {
            net: "public".into(),
            after_ts: 1,
            after_id: String::new(),
            limit: 10,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"sync\""));
        assert!(matches!(
            serde_json::from_str::<Request>(&json).unwrap(),
            Request::Sync { limit: 10, .. }
        ));
    }
}
