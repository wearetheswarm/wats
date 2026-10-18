//! The version-invariant event core (ADR 0007, hardened by ADR 0010),
//! frozen for every `v`.
//!
//! A node must be able to check, store and relay an event of a version it
//! does not understand; otherwise every format upgrade would split the
//! network and the log would stop being whole. These rules never change:
//!
//! - **Input**: a wire event is a JSON object under the strict profile of
//!   [`crate::strict_json`]: no duplicate keys, integers only within
//!   ±(2^53 − 1), valid UTF-8, nesting at most [`MAX_EVENT_DEPTH`]. Strings
//!   are hashed as received (no Unicode normalization).
//! - **Split**: `id` and `sig` are not hashed. Header keys: `v`, `author`,
//!   `ts`, `net`, `thread`, `parent`, `kind`, `pow`, optionally `refs`, and
//!   from `v` = 2 on an optional `ext` object (header extensions). Every
//!   other key is a body key. A tombstoned event carries `body_hash` instead
//!   of any body key.
//! - **Hashing**, BLAKE3 in key-derivation mode for domain separation:
//!   `body = hex(BLAKE3-derive("wats event body v1", JCS(body)))` and
//!   `id = BLAKE3-derive("wats event id v1", JCS(header + body))`.
//! - **Proof of work** is tagged with its algorithm: `pow` is
//!   `"<alg>:<payload>"`, `alg` 1-16 chars of `[a-z0-9]`, `payload` 1-2048
//!   chars of base64url `[A-Za-z0-9_-]`. Validity checks only this syntax;
//!   the algorithm and the difficulty are node policy. A node measures the
//!   work of the algorithms it knows ([`POW_ALG_B3`]: the leading zero bits
//!   of `id`) and treats an event of any other algorithm as opaque.
//! - **Signature**: the suite (key type, signature algorithm, signed
//!   message) is determined by `v`, one suite per version. A node relays
//!   versions it does not understand after checking shape, id and policy,
//!   without checking their signature: it cannot know their algorithm.
//! - **GREASE**: keys and values starting with [`GREASE_PREFIX`] are
//!   reserved; every node ignores them (and v1 accepts them as body keys).
//!
//! A version may change the body keys and their rules, the meaning of
//! `ext`, the kind vocabulary, the signature suite and the limits. It may
//! not change anything above.

// Network input must never panic the node (security review, item D).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Map, Value};

use crate::event::{
    Event, EventError, MAX_SAFE_INTEGER, VERSION, is_valid_net, leading_zero_bits, parse_hex,
};

/// Event versions whose rules this software checks: v1, indexed, searched
/// and shown, and v2 (ADR 0015), validated but still stored as opaque rows
/// and never indexed or shown, because its content is encrypted.
pub const UNDERSTOOD_VERSIONS: (u64, u64) = (VERSION as u64, crate::v2::VERSION);
/// Highest event version the invariant core accepts.
pub const MAX_RELAY_VERSION: u64 = 65_535;
/// Node policy (security review B4): opaque events of a newer version are
/// stored and relayed only up to `events_max` + this. A version nobody
/// runs yet is not worth carrying, and the gap bounds what a stranger can
/// make every node store without understanding it.
pub const RELAY_VERSIONS_AHEAD: u64 = 2;

/// BLAKE3 key-derivation contexts (ADR 0010, A2). Frozen: they are part of
/// the invariant core, not of a version.
pub const ID_CONTEXT: &str = "wats event id v1";
pub const BODY_CONTEXT: &str = "wats event body v1";
/// Signed message prefix of the v1 suite: Ed25519 over `prefix || id`.
pub const SIG_PREFIX_V1: &[u8] = b"wats/event/v1";

/// Required header keys, frozen.
pub const HEADER_KEYS: [&str; 8] = [
    "v", "author", "ts", "net", "thread", "parent", "kind", "pow",
];
/// Optional header key: backward references to other events (ADR 0010, A6).
pub const REFS_KEY: &str = "refs";
/// Optional header key for header extensions, `v` ≥ 2 only.
pub const EXT_KEY: &str = "ext";
/// Keys that are neither header nor body.
pub const UNHASHED_KEYS: [&str; 2] = ["id", "sig"];
/// Present instead of the body keys on a tombstoned event.
pub const BODY_HASH_KEY: &str = "body_hash";
/// Longest proof-of-work algorithm name, and longest payload (ADR 0010,
/// algorithm-tagged PoW). 2 KiB of base64url holds an Equihash-size
/// solution.
pub const MAX_POW_ALG_BYTES: usize = 16;
pub const MAX_POW_PAYLOAD_BYTES: usize = 2048;
/// The first proof-of-work algorithm: the payload is a decimal nonce of
/// 1-20 digits, and the work is the number of leading zero bits of `id`.
pub const POW_ALG_B3: &str = "b3";
/// Context of the challenge a later, solution-carrying algorithm hashes:
/// `BLAKE3-derive("wats pow v1", JCS(header with pow = "<alg>:"))`, which
/// binds the solution to the event.
pub const POW_CHALLENGE_CONTEXT: &str = "wats pow v1";

/// Reserved prefix for GREASE keys and values (ADR 0010, A8).
pub const GREASE_PREFIX: &str = "_grease";

/// Deepest nesting of an event (the event object itself counts as 1).
pub const MAX_EVENT_DEPTH: usize = 20;
/// Most entries in `refs`.
pub const MAX_REFS: usize = 256;
/// Longest `author` or `sig`, in hex characters, across all versions.
pub const MAX_AUTHOR_CHARS: usize = 8 * 1024;
pub const MAX_SIG_CHARS: usize = 64 * 1024;
/// Longest `kind`.
pub const MAX_KIND_BYTES: usize = 64;

/// Whether `pow` has the core syntax `"<alg>:<payload>"`.
pub fn pow_syntax_ok(pow: &str) -> bool {
    let Some((alg, payload)) = pow.split_once(':') else {
        return false;
    };
    (1..=MAX_POW_ALG_BYTES).contains(&alg.len())
        && alg
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && (1..=MAX_POW_PAYLOAD_BYTES).contains(&payload.len())
        && payload
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The work of a proof of work this node knows how to measure, or `None`
/// for an unknown algorithm. `b3` with a payload that is not a decimal
/// nonce of 1-20 digits counts as no work.
pub fn pow_work(pow: &str, id: &[u8; 32]) -> Option<u32> {
    let (alg, payload) = pow.split_once(':')?;
    match alg {
        POW_ALG_B3 => {
            let nonce_ok =
                (1..=20).contains(&payload.len()) && payload.bytes().all(|b| b.is_ascii_digit());
            Some(if nonce_ok { leading_zero_bits(id) } else { 0 })
        }
        _ => None,
    }
}

pub fn is_grease(s: &str) -> bool {
    s.starts_with(GREASE_PREFIX)
}

fn invalid(msg: impl Into<String>) -> EventError {
    EventError::Invalid(msg.into())
}

fn jcs(value: &impl serde::Serialize) -> Result<Vec<u8>, EventError> {
    Ok(serde_json_canonicalizer::to_vec(value)?)
}

/// Core shape of `author` and `sig` (ADR 0010 A4 addendum): a non-empty
/// string over the base64url alphabet `[A-Za-z0-9_-]`, a superset of
/// lowercase hex. The version decides the exact encoding (v1 and v2:
/// lowercase hex of a fixed length).
fn is_key_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The canonical pieces of an event, as hashed.
#[derive(Debug, Clone)]
pub struct Digest {
    /// JCS of the body object (`None` for a tombstoned event).
    pub body_jcs: Option<Vec<u8>>,
    pub body_hash: String,
    pub header_jcs: Vec<u8>,
    pub id: [u8; 32],
}

/// A header object as hashed for the id, with the body's canonical bytes
/// (`None` when tombstoned) and hash.
pub type Split = (Map<String, Value>, Option<Vec<u8>>, String);

/// Split a wire object into header and body (with `body` set to the body
/// hash).
pub fn header_object(wire: &Map<String, Value>) -> Result<Split, EventError> {
    let v = wire
        .get("v")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .ok_or_else(|| invalid("v must be a positive integer"))?;
    let mut header = Map::new();
    for key in HEADER_KEYS {
        let value = wire
            .get(key)
            .ok_or_else(|| invalid(format!("missing header field {key}")))?;
        header.insert(key.to_string(), value.clone());
    }
    if let Some(refs) = wire.get(REFS_KEY) {
        header.insert(REFS_KEY.to_string(), refs.clone());
    }
    if let Some(ext) = wire.get(EXT_KEY) {
        if v <= VERSION as u64 {
            return Err(invalid("ext is not allowed in v1 events"));
        }
        if !ext.is_object() {
            return Err(invalid("ext must be an object"));
        }
        header.insert(EXT_KEY.to_string(), ext.clone());
    }
    let body = body_object(wire);
    let (body_jcs, body_hash) = match wire.get(BODY_HASH_KEY) {
        Some(hash) => {
            if !body.is_empty() {
                return Err(invalid("a tombstoned event carries no body fields"));
            }
            let hash = hash
                .as_str()
                .ok_or_else(|| invalid("body_hash must be a string"))?;
            parse_hex::<32>(hash, "body_hash")?;
            (None, hash.to_string())
        }
        None => {
            let bytes = jcs(&body)?;
            let hash = hex::encode(blake3::derive_key(BODY_CONTEXT, &bytes));
            (Some(bytes), hash)
        }
    };
    header.insert("body".to_string(), Value::String(body_hash.clone()));
    Ok((header, body_jcs, body_hash))
}

/// Whether a wire key is not a body key: a header key, `id`, `sig` or
/// `body_hash`.
pub fn is_structural_key(key: &str) -> bool {
    HEADER_KEYS.contains(&key)
        || key == REFS_KEY
        || key == EXT_KEY
        || UNHASHED_KEYS.contains(&key)
        || key == BODY_HASH_KEY
}

/// The body keys of a wire object (empty for a tombstone).
pub fn body_object(wire: &Map<String, Value>) -> Map<String, Value> {
    wire.iter()
        .filter(|(key, _)| !is_structural_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Hash a wire object. Checks the split rules, not the signature.
pub fn digest(wire: &Map<String, Value>) -> Result<Digest, EventError> {
    let (header, body_jcs, body_hash) = header_object(wire)?;
    let header_jcs = jcs(&header)?;
    let id = blake3::derive_key(ID_CONTEXT, &header_jcs);
    Ok(Digest {
        body_jcs,
        body_hash,
        header_jcs,
        id,
    })
}

/// The tombstone of a full event (ADR 0020): its header keys, `refs`,
/// `ext`, `id` and `sig`, with `body_hash` in place of the body. It has the
/// same id and the same valid signature as the full event. An event that is
/// already a tombstone is returned as is.
pub fn tombstone(full: &Value) -> Result<RawEvent, EventError> {
    let Value::Object(wire) = full else {
        return Err(invalid("an event is a JSON object"));
    };
    let body_hash = digest(wire)?.body_hash;
    let mut t: Map<String, Value> = wire
        .iter()
        .filter(|(key, _)| is_structural_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    t.insert(BODY_HASH_KEY.into(), Value::String(body_hash));
    RawEvent::from_value(Value::Object(t))
}

fn kind_token_ok(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= MAX_KIND_BYTES
        && kind.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' || b == b'.'
        })
}

/// The core shape rules of `refs`: at most [`MAX_REFS`] ids, strictly
/// ascending (so sorted and without duplicates).
pub fn check_refs(value: &Value) -> Result<Vec<String>, EventError> {
    let Value::Array(items) = value else {
        return Err(invalid("refs must be an array of event ids"));
    };
    if items.len() > MAX_REFS {
        return Err(invalid(format!("more than {MAX_REFS} refs")));
    }
    let mut refs = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .as_str()
            .ok_or_else(|| invalid("refs must be an array of event ids"))?;
        parse_hex::<32>(id, "ref")?;
        if refs.last().is_some_and(|prev: &String| prev.as_str() >= id) {
            return Err(invalid("refs must be sorted in strictly ascending order"));
        }
        refs.push(id.to_string());
    }
    Ok(refs)
}

/// An event checked against the invariant core only: shape, id. Used for
/// versions this node does not understand and for tombstoned events.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEvent {
    pub v: u64,
    pub id: String,
    pub author: String,
    pub ts: u64,
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    pub refs: Vec<String>,
    pub kind: String,
    pub tombstoned: bool,
    /// Whether the signature was checked (only for versions whose suite
    /// this node knows).
    pub signature_checked: bool,
    /// The whole wire object, as received.
    pub wire: Map<String, Value>,
}

impl RawEvent {
    /// Parse untrusted bytes under the strict profile and check the core.
    pub fn from_bytes(bytes: &[u8]) -> Result<RawEvent, EventError> {
        let value = crate::strict_json::parse(bytes).map_err(|e| invalid(e.to_string()))?;
        Self::from_value(value)
    }

    /// Check the invariant core of a wire object that was parsed under the
    /// strict profile: shape, nesting, numbers, id. Not the signature.
    pub fn from_value(value: Value) -> Result<RawEvent, EventError> {
        if crate::strict_json::depth(&value) > MAX_EVENT_DEPTH {
            return Err(invalid(format!(
                "event nested deeper than {MAX_EVENT_DEPTH}"
            )));
        }
        if !crate::strict_json::integers_only(&value) {
            return Err(invalid("events carry integers only, within ±(2^53 - 1)"));
        }
        let Value::Object(wire) = value else {
            return Err(invalid("an event is a JSON object"));
        };
        let v = wire.get("v").and_then(Value::as_u64).unwrap_or(0);
        if v == 0 || v > MAX_RELAY_VERSION {
            return Err(EventError::UnsupportedVersion(v));
        }
        let text = |key: &str| -> Result<String, EventError> {
            wire.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| invalid(format!("{key} must be a string")))
        };
        let opt_hex = |key: &str| -> Result<Option<String>, EventError> {
            match wire.get(key) {
                Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => {
                    parse_hex::<32>(s, key)?;
                    Ok(Some(s.clone()))
                }
                _ => Err(invalid(format!("{key} must be null or a 64-char hex id"))),
            }
        };
        let author = text("author")?;
        let id = text("id")?;
        let sig = text("sig")?;
        let net = text("net")?;
        let kind = text("kind")?;
        let pow = text("pow")?;
        if !is_key_token(&author) || author.len() > MAX_AUTHOR_CHARS {
            return Err(invalid("author must be 1-8192 chars of [A-Za-z0-9_-]"));
        }
        if !is_key_token(&sig) || sig.len() > MAX_SIG_CHARS {
            return Err(invalid("sig must be 1-65536 chars of [A-Za-z0-9_-]"));
        }
        let ts = wire
            .get("ts")
            .and_then(Value::as_u64)
            .filter(|t| *t <= MAX_SAFE_INTEGER)
            .ok_or_else(|| invalid("ts must be a non-negative integer"))?;
        let (thread, parent) = (opt_hex("thread")?, opt_hex("parent")?);
        if thread.is_some() != parent.is_some() {
            return Err(invalid("thread and parent must be both null or both set"));
        }
        if !is_valid_net(&net) {
            return Err(invalid("net must be 1-64 chars of [a-z0-9._:-]"));
        }
        if !kind_token_ok(&kind) {
            return Err(invalid("kind must be 1-64 chars of [a-z0-9._-]"));
        }
        if !pow_syntax_ok(&pow) {
            return Err(invalid(
                "pow must be \"<alg>:<payload>\": alg 1-16 chars of [a-z0-9], \
                 payload 1-2048 chars of [A-Za-z0-9_-]",
            ));
        }
        let refs = match wire.get(REFS_KEY) {
            Some(value) => check_refs(value)?,
            None => Vec::new(),
        };
        let digest = digest(&wire)?;
        if parse_hex::<32>(&id, "id")? != digest.id {
            return Err(EventError::IdMismatch);
        }
        Ok(RawEvent {
            v,
            id,
            author,
            ts,
            net,
            thread,
            parent,
            refs,
            kind,
            tombstoned: digest.body_jcs.is_none(),
            signature_checked: false,
            wire,
        })
    }

    /// Check the signature with the suite of this event's version.
    /// Returns `Ok(false)` when this node does not know the suite.
    pub fn verify_signature(&mut self) -> Result<bool, EventError> {
        match self.v {
            1 => {
                let author = parse_hex::<32>(&self.author, "author")?;
                let sig = parse_hex::<64>(
                    self.wire
                        .get("sig")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "sig",
                )?;
                let id = parse_hex::<32>(&self.id, "id")?;
                verify_v1(&author, &id, &sig)?;
                self.signature_checked = true;
                Ok(true)
            }
            crate::v2::VERSION => {
                crate::v2::verify_signature(self)?;
                self.signature_checked = true;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The proof of work, `"<alg>:<payload>"`.
    pub fn pow(&self) -> &str {
        self.wire
            .get("pow")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    /// The work of its proof of work, `None` for an algorithm this node
    /// does not know.
    pub fn pow_work(&self) -> Option<u32> {
        pow_work(self.pow(), &parse_hex::<32>(&self.id, "id").ok()?)
    }

    pub fn thread_root(&self) -> &str {
        self.thread.as_deref().unwrap_or(&self.id)
    }

    /// How to show the kind: core kinds as is, anything else as `note`.
    pub fn display_kind(&self) -> crate::event::Kind {
        self.kind
            .parse::<crate::event::Kind>()
            .map(|k| k.display())
            .unwrap_or(crate::event::Kind::Note)
    }

    pub fn to_value(&self) -> Value {
        Value::Object(self.wire.clone())
    }
}

/// The v1 signature suite: Ed25519 over `"wats/event/v1" || id`, verified
/// under ZIP 215 (ADR 0010, A1): non-canonical encodings of A and R are
/// accepted, `s` must be canonical (< l), and the cofactored equation
/// `[8][s]B = [8]R + [8][k]A` decides.
pub fn verify_v1(author: &[u8; 32], id: &[u8; 32], sig: &[u8; 64]) -> Result<(), EventError> {
    verify_prefixed(SIG_PREFIX_V1, author, id, sig)
}

/// An Ed25519 author signature over `prefix || id`, under the v1 rules:
/// the key checks of [`check_v1_key`], then ZIP 215. v1 and v2 differ only
/// by the prefix.
pub fn verify_prefixed(
    prefix: &[u8],
    author: &[u8; 32],
    id: &[u8; 32],
    sig: &[u8; 64],
) -> Result<(), EventError> {
    check_v1_key(author, "author")?;
    let key = ed25519_zebra::VerificationKey::try_from(*author)
        .map_err(|_| invalid("author is not an Ed25519 point"))?;
    let signature = ed25519_zebra::Signature::from_bytes(sig);
    key.verify(&signature, &prefixed_message(prefix, id))
        .map_err(|_| EventError::BadSignature)
}

/// `prefix || id`, the message of a prefixed signature.
pub fn prefixed_message(prefix: &[u8], id: &[u8; 32]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(prefix.len().saturating_add(32));
    msg.extend_from_slice(prefix);
    msg.extend_from_slice(id);
    msg
}

/// v1 keys (`author`, rotate `data.new_key`) are checked on top of ZIP 215
/// (pre-freeze security review, A2): the canonical encoding of a point
/// that is not of small order. A small-order key signs anything, and two
/// encodings of one point would be two authors. ZIP 215 still decides the
/// signature equation, non-canonical `R` included.
pub fn check_v1_key(key: &[u8; 32], what: &str) -> Result<(), EventError> {
    let point = curve25519_dalek::edwards::CompressedEdwardsY(*key)
        .decompress()
        .ok_or_else(|| invalid(format!("{what} is not an Ed25519 point")))?;
    if point.compress().to_bytes() != *key {
        return Err(invalid(format!("{what} is not a canonical point encoding")));
    }
    if point.is_small_order() {
        return Err(invalid(format!("{what} is a small-order point")));
    }
    Ok(())
}

/// The message signed by v1 events.
pub fn v1_message(id: &[u8; 32]) -> Vec<u8> {
    prefixed_message(SIG_PREFIX_V1, id)
}

/// Sign an id with the v1 suite.
pub fn sign_v1(key: &SigningKey, id: &[u8; 32]) -> [u8; 64] {
    key.sign(&v1_message(id)).to_bytes()
}

/// A checked event: either one this node understands, or opaque.
#[derive(Debug, Clone)]
pub enum AnyEvent {
    Known(Event),
    Opaque(RawEvent),
}

impl AnyEvent {
    /// Parse untrusted bytes under the strict profile and check them.
    pub fn from_bytes(bytes: &[u8]) -> Result<AnyEvent, EventError> {
        let value = crate::strict_json::parse(bytes).map_err(|e| invalid(e.to_string()))?;
        Self::from_value(value)
    }

    /// Check an event that was parsed under the strict profile. Versions
    /// with a known suite get their signature checked; understood versions
    /// get their full rules; everything else only the invariant core. v2
    /// events (ADR 0015), tombstoned or not, get their rules and signature
    /// checked and are returned opaque: their content is encrypted.
    pub fn from_value(value: Value) -> Result<AnyEvent, EventError> {
        let mut raw = RawEvent::from_value(value)?;
        if raw.v == crate::v2::VERSION {
            crate::v2::check_rules(&raw)?;
        }
        raw.verify_signature()?;
        if raw.v == crate::v2::VERSION {
            return Ok(AnyEvent::Opaque(raw));
        }
        let (min, max) = UNDERSTOOD_VERSIONS;
        // An event whose proof of work this node cannot measure is opaque
        // too, whatever its version: never shown, never taken as no work.
        if raw.tombstoned || raw.v < min || raw.v > max || raw.pow_work().is_none() {
            return Ok(AnyEvent::Opaque(raw));
        }
        crate::event::check_v1_wire(&raw.wire)?;
        let event: Event = serde_json::from_value(raw.to_value())?;
        // The typed event is what this node stores and relays: it must be
        // exactly the object that was signed, or relayed copies would no
        // longer match their id.
        if event.wire_map()? != raw.wire {
            return Err(invalid("event does not round-trip to the object received"));
        }
        event.check_rules()?;
        Ok(AnyEvent::Known(event))
    }

    pub fn id(&self) -> &str {
        match self {
            AnyEvent::Known(e) => &e.id,
            AnyEvent::Opaque(r) => &r.id,
        }
    }

    pub fn net(&self) -> &str {
        match self {
            AnyEvent::Known(e) => &e.net,
            AnyEvent::Opaque(r) => &r.net,
        }
    }

    pub fn author(&self) -> &str {
        match self {
            AnyEvent::Known(e) => &e.author,
            AnyEvent::Opaque(r) => &r.author,
        }
    }

    pub fn ts(&self) -> u64 {
        match self {
            AnyEvent::Known(e) => e.ts,
            AnyEvent::Opaque(r) => r.ts,
        }
    }

    /// The work of its proof of work, `None` for an algorithm this node
    /// does not know (always known for a `Known` event).
    pub fn pow_work(&self) -> Option<u32> {
        match self {
            AnyEvent::Known(e) => Some(e.pow_bits()),
            AnyEvent::Opaque(r) => r.pow_work(),
        }
    }
}

/// Sign any header/body pair under the invariant core with the v1 suite,
/// mining `pow_bits`. `header` holds `v`, `ts`, `net`, `thread`, `parent`,
/// `kind` and optionally `refs` and `ext`; `author` and `pow` are filled
/// in. For v1 posts use `Draft::sign`, which also enforces the v1 rules;
/// this function is for tooling, test vectors and simulating other
/// versions.
pub fn sign_value(
    mut header: Map<String, Value>,
    body: Map<String, Value>,
    key: &SigningKey,
    pow_bits: u32,
) -> Result<Value, EventError> {
    header.insert(
        "author".into(),
        Value::String(hex::encode(key.verifying_key().as_bytes())),
    );
    let mut wire = body;
    for (k, v) in header {
        wire.insert(k, v);
    }
    let id = mine(&mut wire, pow_bits)?;
    wire.insert("id".into(), Value::String(hex::encode(id)));
    wire.insert("sig".into(), Value::String(hex::encode(sign_v1(key, &id))));
    Ok(Value::Object(wire))
}

/// Mine a `b3` proof of work of `pow_bits` into `wire` (`pow` is set) and
/// return the id. `id` and `sig` are left to the caller.
pub fn mine(wire: &mut Map<String, Value>, pow_bits: u32) -> Result<[u8; 32], EventError> {
    let mut nonce: u64 = 0;
    loop {
        wire.insert("pow".into(), Value::String(format!("{POW_ALG_B3}:{nonce}")));
        let digest = digest(wire)?;
        if leading_zero_bits(&digest.id) >= pow_bits {
            return Ok(digest.id);
        }
        nonce = nonce
            .checked_add(1)
            .ok_or_else(|| invalid("pow nonce space exhausted"))?;
    }
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
    use crate::event::{Draft, Kind};
    use serde_json::json;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[9; 32])
    }

    fn v3_event() -> Value {
        sign_value(
            json!({"v": 3, "ts": 1_760_600_000u64, "net": "public", "thread": null,
                   "parent": null, "kind": "poll", "ext": {"lang": "en"}})
            .as_object()
            .unwrap()
            .clone(),
            json!({"text": "Which relay should we use?", "options": ["a", "b"]})
                .as_object()
                .unwrap()
                .clone(),
            &key(),
            4,
        )
        .unwrap()
    }

    #[test]
    fn v1_events_hash_identically_under_the_core() {
        let event = Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            tags: vec!["cap:pdf".into()],
            title: Some("agree".into()),
            text: "core and v1 agree".into(),
            ..Draft::default()
        }
        .sign(&key(), 1_760_500_000, 4)
        .unwrap();
        let value = serde_json::to_value(&event).unwrap();
        let raw = RawEvent::from_value(value.clone()).unwrap();
        assert_eq!(raw.id, event.id);
        assert!(matches!(
            AnyEvent::from_value(value).unwrap(),
            AnyEvent::Known(_)
        ));
    }

    #[test]
    fn tombstones_keep_the_id_and_the_signature() {
        let event = Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            tags: vec!["cap:pdf".into()],
            title: Some("secret".into()),
            text: "a body to drop".into(),
            refs: vec!["ab".repeat(32)],
            ..Draft::default()
        }
        .sign(&key(), 1_760_500_000, 4)
        .unwrap();
        let full = serde_json::to_value(&event).unwrap();
        let t = tombstone(&full).unwrap();
        assert!(t.tombstoned);
        assert_eq!(t.id, event.id);
        assert_eq!(t.refs, event.refs);
        assert_eq!(
            t.wire.get(BODY_HASH_KEY),
            Some(&json!(digest(full.as_object().unwrap()).unwrap().body_hash))
        );
        for gone in ["title", "text", "tags", "data", "blobs"] {
            assert!(!t.wire.contains_key(gone), "{gone}");
        }
        // Verifies, and is opaque to readers.
        match AnyEvent::from_value(t.to_value()).unwrap() {
            AnyEvent::Opaque(raw) => assert!(raw.signature_checked),
            AnyEvent::Known(_) => panic!("a tombstone is opaque"),
        }
        // Idempotent; v3 events (with `ext`) too.
        assert_eq!(tombstone(&t.to_value()).unwrap(), t);
        let v3 = v3_event();
        let t3 = tombstone(&v3).unwrap();
        assert_eq!(t3.id, v3["id"]);
        assert!(t3.wire.contains_key(EXT_KEY));
    }

    #[test]
    fn pow_is_algorithm_tagged() {
        for ok in [
            "b3:0",
            "b3:18446744073709551615",
            "eqx:AAECAw",
            "a:-_",
            "z9z9z9z9z9z9z9z9:x",
        ] {
            assert!(pow_syntax_ok(ok), "{ok}");
        }
        let long_alg = format!("{}:1", "a".repeat(17));
        let long_payload = format!("eqx:{}", "A".repeat(MAX_POW_PAYLOAD_BYTES + 1));
        let max_payload = format!("eqx:{}", "A".repeat(MAX_POW_PAYLOAD_BYTES));
        assert!(pow_syntax_ok(&max_payload));
        for bad in [
            "0",
            "b3",
            "b3:",
            ":1",
            "B3:1",
            "b-3:1",
            "b3:1=",
            "b3:a+b",
            "b3:a/b",
            "b3:é",
            "b3:1:2",
            &long_alg,
            &long_payload,
        ] {
            assert!(!pow_syntax_ok(bad), "{bad}");
        }
        let id = {
            let mut id = [0xffu8; 32];
            id[0] = 0;
            id[1] = 0x0f;
            id
        };
        assert_eq!(pow_work("b3:123", &id), Some(12));
        assert_eq!(pow_work("b3:abc", &id), Some(0), "b3 needs a decimal nonce");
        assert_eq!(pow_work(&format!("b3:{}", "1".repeat(21)), &id), Some(0));
        assert_eq!(pow_work("eqx:AAAA", &id), None, "unknown algorithm");
    }

    #[test]
    fn future_versions_are_checked_and_kept_opaque() {
        let value = v3_event();
        let AnyEvent::Opaque(raw) = AnyEvent::from_value(value.clone()).unwrap() else {
            panic!("v3 must be opaque to this node");
        };
        assert_eq!(raw.v, 3);
        assert!(!raw.signature_checked, "unknown suite: not checked");
        assert_eq!(raw.display_kind(), Kind::Note);
        assert!(raw.pow_work().unwrap() >= 4);
        for (key, new) in [
            ("options", json!(["c"])),
            ("ext", json!({"lang": "fr"})),
            ("kind", json!("ask")),
        ] {
            let mut forged = value.clone();
            forged[key] = new;
            assert!(matches!(
                RawEvent::from_value(forged),
                Err(EventError::IdMismatch)
            ));
        }
        // A future suite may use other key and signature sizes.
        let mut other = value.clone();
        other["sig"] = json!("ab".repeat(2420));
        assert!(AnyEvent::from_value(other).is_ok());
    }

    #[test]
    fn v1_signatures_are_checked_even_when_tombstoned() {
        let event = Draft {
            net: "public".into(),
            kind: Some(Kind::Note),
            title: Some("tombstone".into()),
            text: "to be tombstoned".into(),
            ..Draft::default()
        }
        .sign(&key(), 1, 0)
        .unwrap();
        let value = serde_json::to_value(&event).unwrap();
        let digest = digest(value.as_object().unwrap()).unwrap();
        let mut tomb = Map::new();
        for (k, v) in value.as_object().unwrap() {
            if HEADER_KEYS.contains(&k.as_str())
                || k == REFS_KEY
                || UNHASHED_KEYS.contains(&k.as_str())
            {
                tomb.insert(k.clone(), v.clone());
            }
        }
        tomb.insert(BODY_HASH_KEY.into(), Value::String(digest.body_hash));
        let AnyEvent::Opaque(raw) = AnyEvent::from_value(Value::Object(tomb.clone())).unwrap()
        else {
            panic!("tombstones are opaque");
        };
        assert!(raw.tombstoned && raw.signature_checked);
        assert_eq!(raw.id, event.id);
        tomb.insert("sig".into(), json!("00".repeat(64)));
        assert!(matches!(
            AnyEvent::from_value(Value::Object(tomb)),
            Err(EventError::BadSignature)
        ));
    }

    #[test]
    fn ext_is_reserved_for_future_versions() {
        let event = Draft {
            net: "public".into(),
            kind: Some(Kind::Note),
            title: Some("v1".into()),
            text: "v1".into(),
            ..Draft::default()
        }
        .sign(&key(), 1, 0)
        .unwrap();
        let mut value = serde_json::to_value(&event).unwrap();
        value["ext"] = json!({});
        assert!(RawEvent::from_value(value).is_err());
    }

    #[test]
    fn domain_separation_is_in_effect() {
        let value = v3_event();
        let wire = value.as_object().unwrap();
        let d = digest(wire).unwrap();
        assert_ne!(d.id, *blake3::hash(&d.header_jcs).as_bytes());
        assert_eq!(d.id, blake3::derive_key(ID_CONTEXT, &d.header_jcs));
    }
}
