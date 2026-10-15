//! Event format v1 (SPEC §2.3).
//!
//! An event is a signed header plus a body (SPEC §2.13 ter). On the wire it
//! is the flat JSON object of §2.3; for hashing it is split in two:
//!
//! ```text
//! body   = { text, tags, data, blobs }
//! header = { v, author, ts, net, thread, parent, kind, body: hex(BLAKE3(JCS(body))), pow }
//! id     = BLAKE3(JCS(header))
//! sig    = Ed25519(author key, id as 32 raw bytes)
//! ```
//!
//! JCS is RFC 8785. The id doubles as the proof-of-work target: `pow` is a
//! nonce chosen so that the id has N leading zero bits. See
//! `docs/adr/0002-event-format-v1.md` for the rationale and the limits.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Current event format version.
pub const VERSION: u32 = 1;

/// Maximum size of `text`, in bytes (provisional, ADR 0002).
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Maximum number of tags (provisional, ADR 0002).
pub const MAX_TAGS: usize = 16;
/// Maximum size of one tag, in bytes (provisional, ADR 0002).
pub const MAX_TAG_BYTES: usize = 64;
/// Maximum size of a network id, in bytes (provisional, ADR 0002).
pub const MAX_NET_BYTES: usize = 64;
/// Maximum size of `data` once canonicalized, in bytes (provisional, ADR 0002).
pub const MAX_DATA_BYTES: usize = 8 * 1024;
/// Maximum number of blob references (provisional, ADR 0002).
pub const MAX_BLOBS: usize = 16;
/// Maximum size of a blob name, in bytes (provisional, ADR 0002).
pub const MAX_BLOB_NAME_BYTES: usize = 255;
/// Maximum size of a blob URL, in bytes (provisional, ADR 0002).
pub const MAX_URL_BYTES: usize = 2048;
/// Maximum size of a serialized event, in bytes (provisional, ADR 0002).
pub const MAX_EVENT_BYTES: usize = 48 * 1024;
/// Largest integer JSON can carry without losing precision (2^53 - 1).
/// JCS serializes numbers as IEEE 754 doubles, so anything larger would
/// hash differently across implementations.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Why a post exists (SPEC §2.3). A closed vocabulary, extended by format
/// version, not by tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
}

impl Kind {
    pub const ALL: [Kind; 10] = [
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
    ];

    pub fn as_str(self) -> &'static str {
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
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Kind {
    type Err = EventError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Kind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| EventError::Invalid(format!("unknown kind {s:?}")))
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

/// A signed event, as stored and exchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub v: u32,
    /// Lowercase hex BLAKE3 hash of the canonical header.
    pub id: String,
    /// Lowercase hex Ed25519 public key.
    pub author: String,
    /// Unix time, seconds.
    pub ts: u64,
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    pub kind: Kind,
    pub tags: Vec<String>,
    pub text: String,
    pub data: Map<String, Value>,
    pub blobs: Vec<BlobRef>,
    pub pow: String,
    /// Lowercase hex Ed25519 signature over the 32 raw bytes of `id`.
    pub sig: String,
}

/// The unsigned content of an event, before signing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Draft {
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    pub kind: Option<Kind>,
    pub tags: Vec<String>,
    pub text: String,
    pub data: Map<String, Value>,
    pub blobs: Vec<BlobRef>,
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

#[derive(Serialize)]
struct BodyRef<'a> {
    text: &'a str,
    tags: &'a [String],
    data: &'a Map<String, Value>,
    blobs: &'a [BlobRef],
}

#[derive(Serialize)]
struct HeaderRef<'a> {
    v: u32,
    author: &'a str,
    ts: u64,
    net: &'a str,
    thread: Option<&'a str>,
    parent: Option<&'a str>,
    kind: Kind,
    body: &'a str,
    pow: &'a str,
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

fn check_safe_numbers(value: &Value) -> Result<(), EventError> {
    match value {
        Value::Number(n) => {
            let ok = if let Some(i) = n.as_i64() {
                i.unsigned_abs() <= MAX_SAFE_INTEGER
            } else if let Some(u) = n.as_u64() {
                u <= MAX_SAFE_INTEGER
            } else {
                n.as_f64().is_some_and(f64::is_finite)
            };
            if ok {
                Ok(())
            } else {
                Err(invalid("data contains a number JSON cannot carry exactly"))
            }
        }
        Value::Array(items) => items.iter().try_for_each(check_safe_numbers),
        Value::Object(map) => map.values().try_for_each(check_safe_numbers),
        _ => Ok(()),
    }
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

fn validate_kind_data(kind: Kind, data: &Map<String, Value>) -> Result<(), EventError> {
    if kind == Kind::React {
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
    Ok(())
}

/// Validate everything that does not need the id or the signature.
fn validate_content(draft: &Draft, kind: Kind) -> Result<(), EventError> {
    let Draft {
        net,
        thread,
        parent,
        tags,
        text,
        data,
        blobs,
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
    check_safe_numbers(&Value::Object(data.clone()))?;
    if jcs(data)?.len() > MAX_DATA_BYTES {
        return Err(invalid(format!("data exceeds {MAX_DATA_BYTES} bytes")));
    }
    validate_kind_data(kind, data)?;
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
    Ok(())
}

impl Draft {
    fn body_hash(&self) -> Result<String, EventError> {
        let body = BodyRef {
            text: &self.text,
            tags: &self.tags,
            data: &self.data,
            blobs: &self.blobs,
        };
        Ok(blake3::hash(&jcs(&body)?).to_hex().to_string())
    }

    /// Sign the draft, mining a proof of work of at least `pow_bits` leading
    /// zero bits on the id.
    pub fn sign(self, key: &SigningKey, ts: u64, pow_bits: u32) -> Result<Event, EventError> {
        let kind = self.kind.ok_or_else(|| invalid("kind is mandatory"))?;
        validate_content(&self, kind)?;
        if ts > MAX_SAFE_INTEGER {
            return Err(invalid("ts too large"));
        }
        if pow_bits > 64 {
            return Err(invalid("pow difficulty above 64 bits is not supported"));
        }
        let author = hex::encode(key.verifying_key().as_bytes());
        let body = self.body_hash()?;
        // "pow" sorts in the middle of the header keys: canonicalize once with
        // an empty nonce, then splice decimal nonces (which need no escaping)
        // between the quotes.
        let template = jcs(&HeaderRef {
            v: VERSION,
            author: &author,
            ts,
            net: &self.net,
            thread: self.thread.as_deref(),
            parent: self.parent.as_deref(),
            kind,
            body: &body,
            pow: "",
        })?;
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
            let mut hasher = blake3::Hasher::new();
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
        let sig = key.sign(&id);
        Ok(Event {
            v: VERSION,
            id: hex::encode(id),
            author,
            ts,
            net: self.net,
            thread: self.thread,
            parent: self.parent,
            kind,
            tags: self.tags,
            text: self.text,
            data: self.data,
            blobs: self.blobs,
            pow,
            sig: hex::encode(sig.to_bytes()),
        })
    }
}

impl Event {
    /// Parse an event from untrusted JSON bytes and fully verify it.
    pub fn from_json(bytes: &[u8]) -> Result<Event, EventError> {
        if bytes.len() > MAX_EVENT_BYTES {
            return Err(invalid(format!("event exceeds {MAX_EVENT_BYTES} bytes")));
        }
        let value: Value = serde_json::from_slice(bytes)?;
        match value.get("v").and_then(Value::as_u64) {
            Some(v) if v == u64::from(VERSION) => {}
            Some(v) => return Err(EventError::UnsupportedVersion(v)),
            None => return Err(invalid("missing version")),
        }
        let event: Event = serde_json::from_value(value)?;
        event.verify()?;
        Ok(event)
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
            kind: Some(self.kind),
            tags: self.tags.clone(),
            text: self.text.clone(),
            data: self.data.clone(),
            blobs: self.blobs.clone(),
        }
    }

    /// Recompute the id from the content.
    pub fn compute_id(&self) -> Result<[u8; 32], EventError> {
        let body = self.draft().body_hash()?;
        let header = jcs(&HeaderRef {
            v: self.v,
            author: &self.author,
            ts: self.ts,
            net: &self.net,
            thread: self.thread.as_deref(),
            parent: self.parent.as_deref(),
            kind: self.kind,
            body: &body,
            pow: &self.pow,
        })?;
        Ok(blake3::hash(&header).into())
    }

    /// Check structure, limits, id and signature. Does not check proof of
    /// work difficulty or timestamps: those are node policy (SPEC §2.13).
    pub fn verify(&self) -> Result<(), EventError> {
        if self.v != VERSION {
            return Err(EventError::UnsupportedVersion(self.v.into()));
        }
        validate_content(&self.draft(), self.kind)?;
        if self.ts > MAX_SAFE_INTEGER {
            return Err(invalid("ts too large"));
        }
        if self.pow.len() > 20 || !self.pow.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("pow must be a decimal nonce"));
        }
        let id = parse_hex::<32>(&self.id, "id")?;
        if self.compute_id()? != id {
            return Err(EventError::IdMismatch);
        }
        let author = parse_hex::<32>(&self.author, "author")?;
        let sig = parse_hex::<64>(&self.sig, "sig")?;
        let key = VerifyingKey::from_bytes(&author).map_err(|_| invalid("bad author key"))?;
        key.verify(&id, &Signature::from_bytes(&sig))
            .map_err(|_| EventError::BadSignature)?;
        Ok(())
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

    fn draft() -> Draft {
        Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            tags: vec!["cap:pdf".into(), "project:x".into()],
            text: "Who can turn this report into a PDF?".into(),
            data: json!({"deadline": 1_760_600_000})
                .as_object()
                .unwrap()
                .clone(),
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
        // Re-serialize with a different key order: the id does not change.
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
    fn unknown_fields_and_versions_are_rejected() {
        let event = draft().sign(&key(1), 1_760_500_000, 0).unwrap();
        let mut value = serde_json::to_value(&event).unwrap();
        value["extra"] = json!(1);
        assert!(Event::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
        value.as_object_mut().unwrap().remove("extra");
        value["v"] = json!(2);
        assert!(matches!(
            Event::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(EventError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn content_rules() {
        let k = key(1);
        let sign = |d: Draft| d.sign(&k, 1, 0);
        assert!(
            sign(Draft {
                text: "  ".into(),
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                net: "Public".into(),
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                thread: Some("00".repeat(32)),
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                tags: vec!["a b".into()],
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                tags: vec!["a".into(), "a".into()],
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                data: json!({"n": 1u64 << 60}).as_object().unwrap().clone(),
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                kind: Some(Kind::React),
                data: json!({"value": 2}).as_object().unwrap().clone(),
                ..draft()
            })
            .is_err()
        );
        assert!(
            sign(Draft {
                blobs: vec![BlobRef {
                    hash: "ab".repeat(32),
                    name: "../etc/passwd".into(),
                    size: 1,
                    url: None,
                }],
                ..draft()
            })
            .is_err()
        );
        let root = sign(draft()).unwrap();
        sign(Draft {
            thread: Some(root.id.clone()),
            parent: Some(root.id.clone()),
            kind: Some(Kind::React),
            data: json!({"value": -1, "accept": true})
                .as_object()
                .unwrap()
                .clone(),
            ..draft()
        })
        .unwrap();
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
        let body =
            blake3::hash(br#"{"blobs":[],"data":{},"tags":["lang:en"],"text":"hello swarm"}"#)
                .to_hex()
                .to_string();
        let header = format!(
            r#"{{"author":"{}","body":"{body}","kind":"note","net":"public","parent":null,"pow":"0","thread":null,"ts":1760500000,"v":1}}"#,
            event.author
        );
        assert_eq!(
            event.id,
            blake3::hash(header.as_bytes()).to_hex().to_string()
        );
    }
}
