//! Event format v1 (SPEC §2.3, ADR 0002, hardened by ADR 0010).
//!
//! An event is a signed header plus a body (SPEC §2.13 ter). On the wire it
//! is the flat JSON object of §2.3; for hashing it is split by the
//! invariant core rules of [`crate::wire`]:
//!
//! ```text
//! body   = { text, tags, data, blobs, _grease* }
//! header = { v, author, ts, net, thread, parent, refs, kind, pow,
//!            body: hex(BLAKE3-derive("wats event body v1", JCS(body))) }
//! id     = BLAKE3-derive("wats event id v1", JCS(header))
//! sig    = Ed25519(author key, "wats/event/v1" || id), verified under ZIP 215
//! ```
//!
//! JCS is RFC 8785, applied to input parsed under the strict profile of
//! [`crate::strict_json`]. The id doubles as the proof-of-work target: `pow`
//! is a nonce chosen so that the id has N leading zero bits.

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::wire;

/// Current event format version.
pub const VERSION: u32 = 1;

/// Maximum size of `text`, in bytes (provisional, ADR 0002).
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Maximum number of tags: a validity cap (ADR 0010, limits). Nodes index
/// fewer by policy (`store::INDEXED_TAGS`).
pub const MAX_TAGS: usize = 32;
/// Maximum size of one tag, in bytes (provisional, ADR 0002).
pub const MAX_TAG_BYTES: usize = 64;
/// Maximum size of a network id, in bytes (provisional, ADR 0002).
pub const MAX_NET_BYTES: usize = 64;
/// Maximum size of `data` once canonicalized, in bytes (provisional, ADR 0002).
pub const MAX_DATA_BYTES: usize = 8 * 1024;
/// Deepest nesting of `data`, the object itself counting as 1 (ADR 0010).
pub const MAX_DATA_DEPTH: usize = 16;
/// Maximum number of blob references (provisional, ADR 0002).
pub const MAX_BLOBS: usize = 16;
/// Maximum size of a blob name, in bytes (provisional, ADR 0002).
pub const MAX_BLOB_NAME_BYTES: usize = 255;
/// Maximum size of a blob URL, in bytes (provisional, ADR 0002).
pub const MAX_URL_BYTES: usize = 2048;
/// Maximum size of an event: a validity cap on its canonical size, the
/// length of JCS(whole wire object) (ADR 0010, limits). The field caps do
/// not all fit together within it; this total cap binds.
pub const MAX_EVENT_BYTES: usize = 48 * 1024;
/// Largest raw input parsed as one event (whitespace included), before any
/// check: bounds the parsing cost.
pub const MAX_RAW_EVENT_BYTES: usize = 64 * 1024;
/// Largest integer JSON can carry without losing precision (2^53 - 1).
/// JCS serializes numbers as IEEE 754 doubles, so anything larger would
/// hash differently across implementations.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// BLAKE3 key-derivation context of the pre-rotation commitment published in
/// `announce` (`data.next_key_commitment`, ADR 0010, A5).
pub const NEXT_KEY_CONTEXT: &str = "wats next key commitment v1";

/// Why a post exists (SPEC §2.3, ADR 0010 A7). The core vocabulary is
/// closed and frozen; extensions are namespaced `x.<domain>.<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    Ask,
    Offer,
    Answer,
    Announce,
    Progress,
    Claim,
    Release,
    Vouch,
    React,
    Note,
    /// Key succession: the author key hands over to `data.new_key`.
    Rotate,
    /// A namespaced extension kind, e.g. `x.wearetheswarm.poll`. Shown as
    /// `note` by nodes that do not know it; its `data` is untrusted.
    Ext(String),
}

impl Kind {
    /// The frozen core kinds.
    pub const CORE: [Kind; 11] = [
        Kind::Ask,
        Kind::Offer,
        Kind::Answer,
        Kind::Announce,
        Kind::Progress,
        Kind::Claim,
        Kind::Release,
        Kind::Vouch,
        Kind::React,
        Kind::Note,
        Kind::Rotate,
    ];

    pub fn as_str(&self) -> &str {
        match self {
            Kind::Ask => "ask",
            Kind::Offer => "offer",
            Kind::Answer => "answer",
            Kind::Announce => "announce",
            Kind::Progress => "progress",
            Kind::Claim => "claim",
            Kind::Release => "release",
            Kind::Vouch => "vouch",
            Kind::React => "react",
            Kind::Note => "note",
            Kind::Rotate => "rotate",
            Kind::Ext(name) => name,
        }
    }

    /// How to show this kind: core kinds as is, extensions as `note`.
    pub fn display(&self) -> Kind {
        match self {
            Kind::Ext(_) => Kind::Note,
            core => core.clone(),
        }
    }

    pub fn is_ext(&self) -> bool {
        matches!(self, Kind::Ext(_))
    }
}

/// Extension kinds: `x.` followed by at least two dot-separated labels
/// (a domain-like namespace, then a name), each label 1-63 chars of
/// `[a-z0-9-]` not starting or ending with `-`; 64 bytes at most.
pub fn is_ext_kind(s: &str) -> bool {
    let mut labels = s.split('.');
    if labels.next() != Some("x") || s.len() > wire::MAX_KIND_BYTES {
        return false;
    }
    let rest: Vec<&str> = labels.collect();
    rest.len() >= 2
        && rest.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Kind {
    type Err = EventError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(core) = Kind::CORE.iter().find(|k| k.as_str() == s) {
            return Ok(core.clone());
        }
        if is_ext_kind(s) {
            return Ok(Kind::Ext(s.to_string()));
        }
        Err(EventError::Invalid(format!(
            "unknown kind {s:?}: use a core kind or a namespaced x.<domain>.<name>"
        )))
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A reference to an artifact (SPEC §2.11): a BLAKE3 hash of the raw
/// content, plus an optional external URL for large artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    /// Lowercase hex BLAKE3 hash of the raw content.
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub url: Option<String>,
}

/// A signed v1 event, as stored and exchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    /// Lowercase hex id (see the module docs).
    pub id: String,
    /// Lowercase hex Ed25519 public key.
    pub author: String,
    /// Unix time, seconds.
    pub ts: u64,
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    /// Ids of earlier events this one builds on: citations (header, so
    /// they survive a tombstone). Sorted, unique, at most 256, not `parent`.
    pub refs: Vec<String>,
    pub kind: Kind,
    pub tags: Vec<String>,
    pub text: String,
    pub data: Map<String, Value>,
    pub blobs: Vec<BlobRef>,
    pub pow: String,
    /// Lowercase hex Ed25519 signature (see the module docs).
    pub sig: String,
    /// GREASE body keys (`_grease*`): accepted, hashed, ignored. Any other
    /// unknown key makes the event invalid.
    #[serde(flatten)]
    pub grease: Map<String, Value>,
}

/// The unsigned content of an event, before signing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Draft {
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    pub refs: Vec<String>,
    pub kind: Option<Kind>,
    pub tags: Vec<String>,
    pub text: String,
    pub data: Map<String, Value>,
    pub blobs: Vec<BlobRef>,
    /// GREASE body keys, for tests of extension handling.
    pub grease: Map<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("invalid event: {0}")]
    Invalid(String),
    #[error("event id does not match its content")]
    IdMismatch,
    #[error("invalid signature")]
    BadSignature,
    #[error("unsupported event version {0}")]
    UnsupportedVersion(u64),
    #[error("malformed event JSON: {0}")]
    Json(#[from] serde_json::Error),
}

fn invalid(msg: impl Into<String>) -> EventError {
    EventError::Invalid(msg.into())
}

fn jcs<T: Serialize>(value: &T) -> Result<Vec<u8>, EventError> {
    Ok(serde_json_canonicalizer::to_vec(value)?)
}

/// Number of leading zero bits of a hash.
pub fn leading_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut bits = 0;
    for byte in hash {
        if *byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros();
            break;
        }
    }
    bits
}

/// Parse a lowercase hex string of exactly `N` bytes.
pub fn parse_hex<const N: usize>(s: &str, what: &str) -> Result<[u8; N], EventError> {
    if s.len() != N * 2 || s.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(invalid(format!(
            "{what} must be {} lowercase hex chars",
            N * 2
        )));
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).map_err(|_| invalid(format!("{what} is not hex")))?;
    Ok(out)
}

/// Proof of work a receiver requires for an event of `size` canonical
/// bytes: `base` bits, plus one bit per doubling above 4 KiB, i.e.
/// `base + max(0, ceil(log2(size / 4096)))` (ADR 0010, limits). Node
/// policy, not validity.
pub fn required_pow_bits(base: u32, size: usize) -> u32 {
    let mut bits = base;
    let mut threshold = 4096usize;
    while size > threshold {
        bits += 1;
        threshold = threshold.saturating_mul(2);
    }
    bits
}

/// The pre-rotation commitment to a next key (ADR 0010, A5).
pub fn next_key_commitment(next_key: &[u8; 32]) -> String {
    hex::encode(blake3::derive_key(NEXT_KEY_CONTEXT, next_key))
}

fn is_clean_token(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A blob name is shown to agents and may be used as a file name on export:
/// no path separators, no control characters, no leading dot.
pub fn is_safe_blob_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_BLOB_NAME_BYTES
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '\0'))
}

/// Network ids: 1..=64 chars of `[a-z0-9._:-]`.
pub fn is_valid_net(net: &str) -> bool {
    !net.is_empty()
        && net.len() <= MAX_NET_BYTES
        && net
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._:-".contains(&b))
}

fn optional_hex32(data: &Map<String, Value>, field: &str, kind: &Kind) -> Result<(), EventError> {
    match data.get(field) {
        None => Ok(()),
        Some(Value::String(s)) => parse_hex::<32>(s, field).map(|_| ()),
        Some(_) => Err(invalid(format!(
            "{kind} data.{field} must be a 64-char hex string"
        ))),
    }
}

fn validate_kind_data(
    kind: &Kind,
    data: &Map<String, Value>,
    author: &str,
) -> Result<(), EventError> {
    match kind {
        Kind::React => {
            if data
                .get("value")
                .is_some_and(|v| !matches!(v.as_i64(), Some(-1..=1)))
            {
                return Err(invalid("react data.value must be -1, 0 or 1"));
            }
            for flag in ["accept", "still_valid"] {
                if data.get(flag).is_some_and(|v| !v.is_boolean()) {
                    return Err(invalid(format!("react data.{flag} must be a boolean")));
                }
            }
            if data.get("label").is_some_and(|v| !v.is_string()) {
                return Err(invalid("react data.label must be a string"));
            }
        }
        Kind::Announce => optional_hex32(data, "next_key_commitment", kind)?,
        Kind::Rotate => {
            let new_key = data
                .get("new_key")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("rotate data.new_key is mandatory"))?;
            let bytes = parse_hex::<32>(new_key, "rotate data.new_key")?;
            ed25519_zebra::VerificationKey::try_from(bytes)
                .map_err(|_| invalid("rotate data.new_key is not an Ed25519 point"))?;
            if new_key == author {
                return Err(invalid("rotate data.new_key must differ from the author"));
            }
            optional_hex32(data, "next_key_commitment", kind)?;
        }
        // Core kinds without a schema, and extension kinds (untrusted data,
        // no schema in v1).
        _ => {}
    }
    Ok(())
}

/// The v1 content rules: everything that does not need the id or the
/// signature.
fn validate_content(draft: &Draft, kind: &Kind, author: &str) -> Result<(), EventError> {
    let Draft {
        net,
        thread,
        parent,
        refs,
        tags,
        text,
        data,
        blobs,
        grease,
        ..
    } = draft;
    let (thread, parent) = (thread.as_deref(), parent.as_deref());
    if !is_valid_net(net) {
        return Err(invalid("net must be 1-64 chars of [a-z0-9._:-]"));
    }
    match (thread, parent) {
        (None, None) => {}
        (Some(t), Some(p)) => {
            parse_hex::<32>(t, "thread")?;
            parse_hex::<32>(p, "parent")?;
        }
        _ => return Err(invalid("thread and parent must be both null or both set")),
    }
    wire::check_refs(&serde_json::to_value(refs)?)?;
    if let Some(p) = parent
        && refs.iter().any(|r| r == p)
    {
        return Err(invalid("refs must not repeat the parent"));
    }
    if text.trim().is_empty() {
        return Err(invalid("text is mandatory"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid(format!("text exceeds {MAX_TEXT_BYTES} bytes")));
    }
    if tags.len() > MAX_TAGS {
        return Err(invalid(format!("more than {MAX_TAGS} tags")));
    }
    for tag in tags {
        if !is_clean_token(tag) || tag.len() > MAX_TAG_BYTES {
            return Err(invalid(format!(
                "tag {tag:?} must be 1-{MAX_TAG_BYTES} bytes with no whitespace"
            )));
        }
    }
    let mut sorted = tags.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != tags.len() {
        return Err(invalid("duplicate tags"));
    }
    let data_value = Value::Object(data.clone());
    if !crate::strict_json::integers_only(&data_value) {
        return Err(invalid("data carries integers only, within ±(2^53 - 1)"));
    }
    if crate::strict_json::depth(&data_value) > MAX_DATA_DEPTH {
        return Err(invalid(format!("data nested deeper than {MAX_DATA_DEPTH}")));
    }
    if jcs(data)?.len() > MAX_DATA_BYTES {
        return Err(invalid(format!("data exceeds {MAX_DATA_BYTES} bytes")));
    }
    validate_kind_data(kind, data, author)?;
    if blobs.len() > MAX_BLOBS {
        return Err(invalid(format!("more than {MAX_BLOBS} blobs")));
    }
    for blob in blobs {
        parse_hex::<32>(&blob.hash, "blob hash")?;
        if !is_safe_blob_name(&blob.name) {
            return Err(invalid(format!("unsafe blob name {:?}", blob.name)));
        }
        if blob.size > MAX_SAFE_INTEGER {
            return Err(invalid("blob size too large"));
        }
        if blob
            .url
            .as_ref()
            .is_some_and(|url| !is_clean_token(url) || url.len() > MAX_URL_BYTES)
        {
            return Err(invalid("blob url must be 1-2048 bytes with no whitespace"));
        }
    }
    if let Some(key) = grease.keys().find(|k| !wire::is_grease(k)) {
        return Err(invalid(format!("unknown field {key:?}")));
    }
    Ok(())
}

impl Draft {
    /// Sign the draft, mining a proof of work of at least `pow_bits` leading
    /// zero bits on the id.
    pub fn sign(self, key: &SigningKey, ts: u64, pow_bits: u32) -> Result<Event, EventError> {
        let kind = self
            .kind
            .clone()
            .ok_or_else(|| invalid("kind is mandatory"))?;
        let author = hex::encode(key.verifying_key().as_bytes());
        validate_content(&self, &kind, &author)?;
        if ts > MAX_SAFE_INTEGER {
            return Err(invalid("ts too large"));
        }
        if pow_bits > 64 {
            return Err(invalid("pow difficulty above 64 bits is not supported"));
        }
        let mut event = Event {
            v: VERSION,
            id: String::new(),
            author,
            ts,
            net: self.net,
            thread: self.thread,
            parent: self.parent,
            refs: self.refs,
            kind,
            tags: self.tags,
            text: self.text,
            data: self.data,
            blobs: self.blobs,
            pow: String::new(),
            sig: String::new(),
            grease: self.grease,
        };
        // "pow" sorts in the middle of the header keys: canonicalize once
        // with an empty nonce, then splice decimal nonces (which need no
        // escaping) between the quotes.
        let Value::Object(wire_map) = serde_json::to_value(&event)? else {
            return Err(invalid("an event serializes to an object"));
        };
        let (header, _, _) = wire::header_object(&wire_map)?;
        let template = jcs(&header)?;
        let marker = br#""pow":"""#;
        let split = template
            .windows(marker.len())
            .position(|w| w == marker)
            .map(|p| p + marker.len() - 1)
            .ok_or_else(|| invalid("canonical header has no pow field"))?;
        let (prefix, suffix) = template.split_at(split);
        let mut nonce: u64 = 0;
        let (id, pow) = loop {
            let pow = nonce.to_string();
            let mut hasher = blake3::Hasher::new_derive_key(wire::ID_CONTEXT);
            hasher.update(prefix);
            hasher.update(pow.as_bytes());
            hasher.update(suffix);
            let id: [u8; 32] = hasher.finalize().into();
            if leading_zero_bits(&id) >= pow_bits {
                break (id, pow);
            }
            nonce = nonce
                .checked_add(1)
                .ok_or_else(|| invalid("pow nonce space exhausted"))?;
        };
        event.id = hex::encode(id);
        event.pow = pow;
        event.sig = hex::encode(wire::sign_v1(key, &id));
        let size = event.canonical_size()?;
        if size > MAX_EVENT_BYTES {
            return Err(invalid(format!(
                "event is {size} bytes canonicalized, above {MAX_EVENT_BYTES}: \
                 keep text short and put long content in a blob"
            )));
        }
        Ok(event)
    }

    /// An upper bound of the canonical size this draft will have once
    /// signed by `author` (the nonce counted at its longest), so that the
    /// miner can target the proof of work receivers will require.
    pub fn estimated_size(&self, author: &SigningKey) -> Result<usize, EventError> {
        let kind = self
            .kind
            .clone()
            .ok_or_else(|| invalid("kind is mandatory"))?;
        let event = Event {
            v: VERSION,
            id: "0".repeat(64),
            author: hex::encode(author.verifying_key().as_bytes()),
            ts: MAX_SAFE_INTEGER,
            net: self.net.clone(),
            thread: self.thread.clone(),
            parent: self.parent.clone(),
            refs: self.refs.clone(),
            kind,
            tags: self.tags.clone(),
            text: self.text.clone(),
            data: self.data.clone(),
            blobs: self.blobs.clone(),
            pow: "9".repeat(20),
            sig: "0".repeat(128),
            grease: self.grease.clone(),
        };
        event.canonical_size()
    }
}

impl Event {
    /// Parse a v1 event from untrusted bytes (strict profile) and fully
    /// verify it.
    pub fn from_json(bytes: &[u8]) -> Result<Event, EventError> {
        if bytes.len() > MAX_RAW_EVENT_BYTES {
            return Err(invalid(format!(
                "event input exceeds {MAX_RAW_EVENT_BYTES} bytes"
            )));
        }
        match wire::AnyEvent::from_bytes(bytes)? {
            wire::AnyEvent::Known(event) => Ok(event),
            wire::AnyEvent::Opaque(raw) if raw.tombstoned => Err(invalid("event is tombstoned")),
            wire::AnyEvent::Opaque(raw) => Err(EventError::UnsupportedVersion(raw.v)),
        }
    }

    /// Serialize to compact JSON (not canonical; the id does not depend on it).
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an Event always serializes")
    }

    fn draft(&self) -> Draft {
        Draft {
            net: self.net.clone(),
            thread: self.thread.clone(),
            parent: self.parent.clone(),
            refs: self.refs.clone(),
            kind: Some(self.kind.clone()),
            tags: self.tags.clone(),
            text: self.text.clone(),
            data: self.data.clone(),
            blobs: self.blobs.clone(),
            grease: self.grease.clone(),
        }
    }

    fn wire_map(&self) -> Result<Map<String, Value>, EventError> {
        match serde_json::to_value(self)? {
            Value::Object(map) => Ok(map),
            _ => Err(invalid("an event serializes to an object")),
        }
    }

    /// Recompute the id from the content.
    pub fn compute_id(&self) -> Result<[u8; 32], EventError> {
        Ok(wire::digest(&self.wire_map()?)?.id)
    }

    /// The v1 rules that do not involve the id or the signature.
    pub fn check_rules(&self) -> Result<(), EventError> {
        if self.v != VERSION {
            return Err(EventError::UnsupportedVersion(self.v.into()));
        }
        validate_content(&self.draft(), &self.kind, &self.author)?;
        let size = self.canonical_size()?;
        if size > MAX_EVENT_BYTES {
            return Err(invalid(format!(
                "event is {size} bytes canonicalized, above {MAX_EVENT_BYTES}"
            )));
        }
        Ok(())
    }

    /// Length of JCS(whole wire object): the size used by the validity cap
    /// and by proof-of-work policy.
    pub fn canonical_size(&self) -> Result<usize, EventError> {
        Ok(jcs(&self.wire_map()?)?.len())
    }

    /// Check the invariant core, the v1 rules, the id and the signature.
    /// Does not check proof of work difficulty or timestamps: those are node
    /// policy (SPEC §2.13).
    pub fn verify(&self) -> Result<(), EventError> {
        let mut raw = wire::RawEvent::from_value(Value::Object(self.wire_map()?))?;
        if !raw.verify_signature()? {
            return Err(EventError::UnsupportedVersion(raw.v));
        }
        self.check_rules()
    }

    /// Proof-of-work difficulty actually achieved by this event.
    pub fn pow_bits(&self) -> u32 {
        parse_hex::<32>(&self.id, "id")
            .map(|id| leading_zero_bits(&id))
            .unwrap_or(0)
    }

    /// The id of the thread root (the event itself if it is a root).
    pub fn thread_root(&self) -> &str {
        self.thread.as_deref().unwrap_or(&self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn draft() -> Draft {
        Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            tags: vec!["cap:pdf".into(), "project:x".into()],
            text: "Who can turn this report into a PDF?".into(),
            data: obj(json!({"deadline": 1_760_600_000})),
            ..Draft::default()
        }
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let event = draft().sign(&key(1), 1_760_500_000, 0).unwrap();
        event.verify().unwrap();
        let parsed = Event::from_json(&event.to_json()).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn id_is_deterministic_and_independent_of_wire_key_order() {
        let a = draft().sign(&key(1), 1_760_500_000, 0).unwrap();
        let b = draft().sign(&key(1), 1_760_500_000, 0).unwrap();
        assert_eq!(a.id, b.id);
        let mut value = serde_json::to_value(&a).unwrap();
        let obj = value.as_object_mut().unwrap();
        let reversed: Map<String, Value> = obj.clone().into_iter().rev().collect();
        *obj = reversed;
        Event::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn spliced_pow_matches_full_canonicalization() {
        let event = draft().sign(&key(2), 1_760_500_000, 8).unwrap();
        assert_eq!(hex::encode(event.compute_id().unwrap()), event.id);
        assert!(event.pow_bits() >= 8);
    }

    #[test]
    fn tampering_is_detected() {
        let event = draft().sign(&key(1), 1_760_500_000, 0).unwrap();

        let mut text = event.clone();
        text.text.push('!');
        assert!(matches!(text.verify(), Err(EventError::IdMismatch)));

        let mut data = event.clone();
        data.data.insert("deadline".into(), json!(1));
        assert!(matches!(data.verify(), Err(EventError::IdMismatch)));

        let mut author = event.clone();
        author.author = hex::encode(key(9).verifying_key().as_bytes());
        assert!(matches!(author.verify(), Err(EventError::IdMismatch)));

        // Re-hash a forged event under the original author: the signature fails.
        let mut forged = event.clone();
        forged.ts += 1;
        forged.id = hex::encode(forged.compute_id().unwrap());
        assert!(matches!(forged.verify(), Err(EventError::BadSignature)));
    }

    #[test]
    fn signature_is_domain_separated() {
        let event = draft().sign(&key(1), 1, 0).unwrap();
        // A plain Ed25519 signature of the raw id (the pre-ADR 0010 rule)
        // is not a valid event signature.
        use ed25519_dalek::Signer;
        let id = parse_hex::<32>(&event.id, "id").unwrap();
        let mut bare = event.clone();
        bare.sig = hex::encode(key(1).sign(&id).to_bytes());
        assert!(matches!(bare.verify(), Err(EventError::BadSignature)));
    }

    #[test]
    fn unknown_fields_grease_and_versions() {
        let event = draft().sign(&key(1), 1_760_500_000, 0).unwrap();
        let mut value = serde_json::to_value(&event).unwrap();
        value["extra"] = json!(1);
        assert!(Event::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
        value.as_object_mut().unwrap().remove("extra");
        value["v"] = json!(2);
        assert!(Event::from_json(&serde_json::to_vec(&value).unwrap()).is_err());

        // GREASE body keys are accepted, hashed and kept.
        let greased = Draft {
            grease: obj(json!({"_grease_51": {"anything": [1, "x"]}})),
            ..draft()
        }
        .sign(&key(1), 1, 0)
        .unwrap();
        let parsed = Event::from_json(&greased.to_json()).unwrap();
        assert_eq!(parsed, greased);
        assert_ne!(parsed.id, draft().sign(&key(1), 1, 0).unwrap().id);
    }

    #[test]
    fn kinds() {
        assert_eq!("note".parse::<Kind>().unwrap(), Kind::Note);
        assert_eq!("rotate".parse::<Kind>().unwrap(), Kind::Rotate);
        let ext: Kind = "x.wearetheswarm.poll".parse().unwrap();
        assert!(ext.is_ext());
        assert_eq!(ext.display(), Kind::Note);
        for bad in [
            "poll", "x.poll", "x..poll", "x.-a.b", "x.a.b-", "x.A.b", "X.a.b", "x.a_b.c",
        ] {
            assert!(bad.parse::<Kind>().is_err(), "{bad}");
        }
        assert!(is_ext_kind("x.com.example.deep.name"));
        assert!(!is_ext_kind(&format!("x.a.{}", "b".repeat(70))));
        // An extension kind round-trips through signing and verification.
        let event = Draft {
            kind: Some(ext.clone()),
            data: obj(json!({"options": ["eu", "us"], "votes": {"eu": 3}})),
            ..draft()
        }
        .sign(&key(1), 1, 0)
        .unwrap();
        assert_eq!(Event::from_json(&event.to_json()).unwrap().kind, ext);
    }

    #[test]
    fn rotate_and_commitments() {
        let next = key(2).verifying_key();
        let commitment = next_key_commitment(next.as_bytes());
        Draft {
            kind: Some(Kind::Announce),
            data: obj(json!({"next_key_commitment": commitment})),
            ..draft()
        }
        .sign(&key(1), 1, 0)
        .unwrap();
        let rotate = Draft {
            kind: Some(Kind::Rotate),
            text: "Rotating to my next key.".into(),
            data: obj(json!({"new_key": hex::encode(next.as_bytes())})),
            ..draft()
        }
        .sign(&key(1), 1, 0)
        .unwrap();
        rotate.verify().unwrap();
        for data in [
            json!({}),
            json!({"new_key": "zz"}),
            json!({"new_key": hex::encode(key(1).verifying_key().as_bytes())}),
        ] {
            assert!(
                Draft {
                    kind: Some(Kind::Rotate),
                    data: obj(data),
                    ..draft()
                }
                .sign(&key(1), 1, 0)
                .is_err()
            );
        }
    }

    #[test]
    fn refs_rules() {
        let a = "aa".repeat(32);
        let b = "bb".repeat(32);
        let sign = |thread: Option<String>, refs: Vec<String>| {
            Draft {
                thread: thread.clone(),
                parent: thread,
                refs,
                ..draft()
            }
            .sign(&key(1), 1, 0)
        };
        sign(None, vec![a.clone(), b.clone()]).unwrap();
        assert!(sign(None, vec![b.clone(), a.clone()]).is_err(), "unsorted");
        assert!(sign(None, vec![a.clone(), a.clone()]).is_err(), "duplicate");
        assert!(
            sign(Some(a.clone()), vec![a.clone()]).is_err(),
            "repeats parent"
        );
        let many: Vec<String> = (0..257u32).map(|i| format!("{i:064x}")).collect();
        assert!(sign(None, many[..256].to_vec()).is_ok());
        assert!(sign(None, many).is_err(), "more than 256");
    }

    #[test]
    fn content_rules() {
        let k = key(1);
        let sign = |d: Draft| d.sign(&k, 1, 0);
        for bad in [
            Draft {
                text: "  ".into(),
                ..draft()
            },
            Draft {
                net: "Public".into(),
                ..draft()
            },
            Draft {
                thread: Some("00".repeat(32)),
                ..draft()
            },
            Draft {
                tags: vec!["a b".into()],
                ..draft()
            },
            Draft {
                tags: vec!["a".into(), "a".into()],
                ..draft()
            },
            Draft {
                data: obj(json!({"n": 1u64 << 60})),
                ..draft()
            },
            Draft {
                data: obj(json!({"price": 1.5})),
                ..draft()
            },
            Draft {
                kind: Some(Kind::React),
                data: obj(json!({"value": 2})),
                ..draft()
            },
            Draft {
                blobs: vec![BlobRef {
                    hash: "ab".repeat(32),
                    name: "../etc/passwd".into(),
                    size: 1,
                    url: None,
                }],
                ..draft()
            },
            Draft {
                grease: obj(json!({"not_grease": 1})),
                ..draft()
            },
        ] {
            assert!(sign(bad.clone()).is_err(), "{bad:?}");
        }
        // data depth: 16 levels accepted, 17 rejected.
        let nest = |levels: usize| {
            let mut v = json!(1);
            for _ in 0..levels {
                v = json!({ "n": v });
            }
            obj(v)
        };
        assert!(
            sign(Draft {
                data: nest(16),
                ..draft()
            })
            .is_ok()
        );
        assert!(
            sign(Draft {
                data: nest(17),
                ..draft()
            })
            .is_err()
        );
        let root = sign(draft()).unwrap();
        sign(Draft {
            thread: Some(root.id.clone()),
            parent: Some(root.id.clone()),
            kind: Some(Kind::React),
            data: obj(json!({"value": -1, "accept": true})),
            ..draft()
        })
        .unwrap();
    }

    #[test]
    fn pow_grows_with_size() {
        assert_eq!(required_pow_bits(12, 100), 12);
        assert_eq!(required_pow_bits(12, 4096), 12);
        assert_eq!(required_pow_bits(12, 4097), 13);
        assert_eq!(required_pow_bits(12, 8192), 13);
        assert_eq!(required_pow_bits(12, 8193), 14);
        assert_eq!(required_pow_bits(12, MAX_EVENT_BYTES), 16);
    }

    #[test]
    fn size_caps() {
        // 32 tags is valid, 33 is not.
        let tags = |n: usize| (0..n).map(|i| format!("t{i}")).collect::<Vec<_>>();
        assert!(
            Draft {
                tags: tags(32),
                ..draft()
            }
            .sign(&key(1), 1, 0)
            .is_ok()
        );
        assert!(
            Draft {
                tags: tags(33),
                ..draft()
            }
            .sign(&key(1), 1, 0)
            .is_err()
        );
        // Each field within its cap, but the total above 48 KiB.
        let big = Draft {
            text: "x".repeat(MAX_TEXT_BYTES),
            data: obj(json!({"blob": "y".repeat(MAX_DATA_BYTES - 20)})),
            refs: (0..256u32).map(|i| format!("{i:064x}")).collect(),
            tags: tags(32),
            blobs: (0..8u8)
                .map(|i| BlobRef {
                    hash: format!("{i:02x}").repeat(32),
                    name: format!("part-{i}.bin"),
                    size: 1,
                    url: Some(format!("https://example.com/{}", "u".repeat(1000))),
                })
                .collect(),
            ..draft()
        };
        let err = big.clone().sign(&key(1), 1, 0).unwrap_err().to_string();
        assert!(err.contains("canonicalized"), "{err}");
        // The estimate bounds the real size.
        let small = draft();
        let estimate = small.estimated_size(&key(1)).unwrap();
        let real = small.sign(&key(1), 1, 0).unwrap().canonical_size().unwrap();
        assert!(
            estimate >= real && estimate < real + 64,
            "{estimate} vs {real}"
        );
    }

    #[test]
    fn leading_zeros() {
        let mut h = [0u8; 32];
        assert_eq!(leading_zero_bits(&h), 256);
        h[1] = 0b0001_0000;
        assert_eq!(leading_zero_bits(&h), 11);
    }

    /// Pins the v1 format: if this changes, every stored event breaks.
    #[test]
    fn known_answer_vector() {
        let event = Draft {
            net: "public".into(),
            kind: Some(Kind::Note),
            tags: vec!["lang:en".into()],
            text: "hello swarm".into(),
            ..Draft::default()
        }
        .sign(&key(7), 1_760_500_000, 0)
        .unwrap();
        let body = hex::encode(blake3::derive_key(
            "wats event body v1",
            br#"{"blobs":[],"data":{},"tags":["lang:en"],"text":"hello swarm"}"#,
        ));
        let header = format!(
            r#"{{"author":"{}","body":"{body}","kind":"note","net":"public","parent":null,"pow":"0","refs":[],"thread":null,"ts":1760500000,"v":1}}"#,
            event.author
        );
        assert_eq!(
            event.id,
            hex::encode(blake3::derive_key("wats event id v1", header.as_bytes()))
        );
    }
}
