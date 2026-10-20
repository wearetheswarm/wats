//! The wats peer protocol (ALPN `/wats/1`), next to gossip and blobs:
//!
//! - `sync`: page through a peer's events of a network, to catch up on what
//!   gossip delivered while this node was offline: ascending after a
//!   cursor (ADR 0005), or newest first before one (`sync_desc`, ADR 0025);
//! - `get`: events by id, the threads they root, or the signals on them
//!   (ADR 0025 §3), for history fetched on demand;
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
//!
//! Load challenge (`crate::challenge`, ADR 0014): a node under load may
//! answer `challenge` to a peer that advertised the `challenge` feature;
//! the requester resubmits the same request with a top-level `solution`
//! field, which older nodes ignore like any unknown field.

// Network input must never panic the node (security review, item D).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use tracing::debug;

use serde_json::Value;

use crate::node::Node;
use crate::wire::{RELAY_VERSIONS_AHEAD, UNDERSTOOD_VERSIONS};

/// Protocol versions this node speaks, newest first. `/wats/2` goes in
/// front of `/wats/1` when it exists; both are then served side by side
/// for the deprecation window of ADR 0007.
pub const ALPNS: &[&[u8]] = &[b"/wats/1"];
/// The same, as numbers, for `hello`.
pub const PROTOCOL_VERSIONS: &[u32] = &[1];
/// Optional capabilities, for `hello`.
pub const FEATURES: &[&str] = &[
    "sync",
    "replicate",
    crate::challenge::FEATURE,
    FEATURE_SYNC_DESC,
    FEATURE_GET,
];
/// The node answers descending `sync` (`before_ts`, ADR 0025 §3).
pub const FEATURE_SYNC_DESC: &str = "sync_desc";
/// The node answers `get` (ADR 0025 §3).
pub const FEATURE_GET: &str = "get";
/// The node holds the complete history of every network it joined: its
/// `sync.initial_max` is 0, with no cut and no pending range. Advertised
/// only while it is true.
pub const FEATURE_HISTORY_FULL: &str = "history_full";
/// Most entries in a `sync` filter (`kinds`, `authors`).
pub const MAX_SYNC_FILTER: usize = 32;
/// Most ids in one `get`.
pub const MAX_GET_IDS: usize = 64;

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
            relay_max: UNDERSTOOD_VERSIONS.1.saturating_add(RELAY_VERSIONS_AHEAD),
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
    /// order. With `before_ts` (ADR 0025, peers with `sync_desc` only):
    /// strictly before `(before_ts, before_id)` and still strictly after
    /// `(after_ts, after_id)`, newest first. `kinds` and `authors` (at most
    /// [`MAX_SYNC_FILTER`] each) keep only those events. `partial`: serve
    /// what is held even where a range of the server is still pending (its
    /// cut and skipped ranges still stop the page); the requester knows the
    /// page may be incomplete.
    Sync {
        net: String,
        #[serde(default)]
        after_ts: u64,
        #[serde(default)]
        after_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_ts: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        kinds: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        authors: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        partial: bool,
        limit: u32,
    },
    /// Events of `net` by id (ADR 0025 §3), answered with a `sync_page`:
    /// `include` is `event` (the events held among `ids`), `thread` (the
    /// thread rooted at each id) or `signals` (what replies to or cites
    /// them). At most [`MAX_GET_IDS`] ids.
    Get {
        net: String,
        ids: Vec<String>,
        include: String,
    },
    /// Store `event` and fetch the listed blobs of it from the requester.
    Replicate {
        event: Value,
        hashes: Vec<String>,
    },
    /// A crash report for the collector (`report.accept`). Nodes that do
    /// not collect answer `Unsupported`, like older nodes.
    CrashReport {
        report: Value,
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
        /// The page stopped because the history below `hi_ts` is not
        /// complete on this node (its cut, or a range still pending, ADR
        /// 0025 §3). Older nodes ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<Gap>,
        /// When the serving node first stored each event (its
        /// `received_at`, Unix seconds), in the order of `events`. Optional:
        /// older nodes neither send nor read it. A parallel list rather than
        /// a field in each event, which is signed wire JSON.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        seen: Vec<u64>,
    },
    Replicated {
        hashes: Vec<String>,
    },
    /// A crash report was stored.
    Accepted,
    Refused {
        reason: String,
    },
    /// The peer does not know this request type.
    Unsupported,
    /// Overloaded: resubmit the request with a `solution` to this
    /// challenge (`crate::challenge`). Only sent to peers that advertise
    /// the `challenge` feature.
    Challenge {
        /// Algorithm id; unknown ids are given up on.
        alg: String,
        seed: String,
        /// Leading zero bits asked.
        effort: u32,
        /// Unix seconds.
        expires: u64,
    },
    /// A response type from a newer protocol revision.
    #[serde(other)]
    Unknown,
}

/// The `gap` of a `sync_page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub hi_ts: u64,
}

impl Request {
    /// An ascending `sync` page request, as every version understands it.
    pub fn sync_after(net: &str, after_ts: u64, after_id: &str, limit: u32) -> Self {
        Request::Sync {
            net: net.to_string(),
            after_ts,
            after_id: after_id.to_string(),
            before_ts: None,
            before_id: None,
            kinds: Vec::new(),
            authors: Vec::new(),
            partial: false,
            limit,
        }
    }

    /// The same `sync`, asking what the peer holds even in ranges of its
    /// own still pending: a check, not a claim of complete history.
    pub fn partial(mut self) -> Self {
        if let Request::Sync { partial, .. } = &mut self {
            *partial = true;
        }
        self
    }
}

/// Send one request to a peer and wait for the response.
pub async fn call(endpoint: &Endpoint, peer: EndpointId, request: &Request) -> Result<Response> {
    let mut value = serde_json::to_value(request)?;
    crate::grease::maybe_add_to(&mut value);
    call_value(endpoint, peer, value).await
}

/// [`call`], solving a load challenge once if the peer answers with one.
/// A challenge that cannot be solved (unknown algorithm, too much work,
/// expired) is returned as is: callers treat it like `Refused`.
pub async fn call_solving(
    endpoint: &Endpoint,
    peer: EndpointId,
    request: &Request,
) -> Result<Response> {
    let response = call(endpoint, peer, request).await?;
    let Response::Challenge {
        alg,
        seed,
        effort,
        expires,
    } = &response
    else {
        return Ok(response);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if *expires < now {
        return Ok(response);
    }
    let (alg, seed, effort, me) = (alg.clone(), seed.clone(), *effort, endpoint.id());
    let solved = tokio::task::spawn_blocking(move || {
        crate::challenge::solve(&alg, &seed, effort, &me, &peer)
    })
    .await?;
    let Some(solution) = solved else {
        debug!(peer = %peer.fmt_short(), "challenge given up: {response:?}");
        return Ok(response);
    };
    let mut value = serde_json::to_value(request)?;
    if let Value::Object(object) = &mut value {
        object.insert("solution".into(), serde_json::to_value(solution)?);
    }
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

/// Parse a request, with the `solution` to a load challenge it may carry.
/// A malformed solution is ignored, like an unknown field.
pub fn parse_request(bytes: &[u8]) -> Result<(Request, Option<crate::challenge::Solution>)> {
    let value = crate::strict_json::parse(bytes)?;
    let solution = value
        .get("solution")
        .and_then(|s| serde_json::from_value(s.clone()).ok());
    let request = serde_json::from_value(value)?;
    Ok((request, solution))
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
            let response = match parse_request(&bytes) {
                Ok((Request::Unsupported, _)) => Response::Unsupported,
                Ok((request, solution)) => node.handle_request(peer, request, solution).await,
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
    for (o, (a, b)) in out.iter_mut().zip(hash.iter().zip(node.as_bytes())) {
        *o = a ^ b;
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
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
    fn challenges_and_solutions_are_forward_compatible() {
        // A solution rides along any request; a malformed one is ignored.
        let (request, solution) = parse_request(
            br#"{"type":"sync","net":"public","after_ts":0,"after_id":"","limit":5,
                "solution":{"alg":"b3","seed":"00","nonce":"7","_grease_1":1}}"#,
        )
        .unwrap();
        assert!(matches!(request, Request::Sync { limit: 5, .. }));
        assert_eq!(solution.unwrap().nonce, "7");
        let (_, solution) = parse_request(
            br#"{"type":"hello","info":{"software":"x","protocols":[1],"events_min":1,
                "events_max":1,"relay_max":1,"features":[]},"solution":42}"#,
        )
        .unwrap();
        assert!(solution.is_none());
        // A challenge with an unknown algorithm and extra fields parses.
        let response: Response = crate::strict_json::from_slice(
            br#"{"type":"challenge","alg":"m1","seed":"ab","effort":30,"expires":1,"memory":64}"#,
        )
        .unwrap();
        assert!(matches!(response, Response::Challenge { effort: 30, .. }));
        assert!(NodeInfo::ours().features.iter().any(|f| f == "challenge"));
    }

    #[test]
    fn history_fields_are_optional_both_ways() {
        // A descending, filtered request round-trips.
        let request = Request::Sync {
            net: "public".into(),
            after_ts: 5,
            after_id: String::new(),
            before_ts: Some(9),
            before_id: Some("ab".into()),
            kinds: vec!["vouch".into()],
            authors: vec![],
            partial: true,
            limit: 8,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"before_ts\":9") && !json.contains("authors"));
        assert!(matches!(
            crate::strict_json::from_slice::<Request>(json.as_bytes()).unwrap(),
            Request::Sync {
                before_ts: Some(9),
                partial: true,
                ..
            }
        ));
        // A 0.4.0 page has neither `gap` nor `seen`; a 0.4.1 page with them
        // still reads as a page on any node.
        let old: Response =
            crate::strict_json::from_slice(br#"{"type":"sync_page","events":[],"more":false}"#)
                .unwrap();
        assert!(matches!(old, Response::SyncPage { gap: None, ref seen, .. } if seen.is_empty()));
        let new: Response = crate::strict_json::from_slice(
            br#"{"type":"sync_page","events":[],"more":false,"gap":{"hi_ts":7},"seen":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            new,
            Response::SyncPage {
                gap: Some(Gap { hi_ts: 7 }),
                ..
            }
        ));
        let get: Request = crate::strict_json::from_slice(
            br#"{"type":"get","net":"public","ids":["ab"],"include":"thread"}"#,
        )
        .unwrap();
        assert!(matches!(get, Request::Get { ref include, .. } if include == "thread"));
        let ours = NodeInfo::ours();
        assert!(ours.features.iter().any(|f| f == FEATURE_SYNC_DESC));
        assert!(ours.features.iter().any(|f| f == FEATURE_GET));
    }

    #[test]
    fn requests_roundtrip_as_json() {
        let request = Request::sync_after("public", 1, "", 10);
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"sync\""));
        // An ascending request carries no new field: older nodes read it.
        assert!(!json.contains("before") && !json.contains("kinds"));
        assert!(matches!(
            serde_json::from_str::<Request>(&json).unwrap(),
            Request::Sync { limit: 10, .. }
        ));
    }
}
