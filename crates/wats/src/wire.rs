//! The version-invariant event core (ADR 0007), frozen for every `v`.
//!
//! A node must be able to verify, store and relay an event of a version it
//! does not understand; otherwise every format upgrade would split the
//! network and the log would stop being whole. These rules never change:
//!
//! - A wire event is a JSON object. `id` and `sig` are not hashed.
//! - Header keys: `v`, `author`, `ts`, `net`, `thread`, `parent`, `kind`,
//!   `pow`, and from `v` = 2 on an optional `ext` object (header
//!   extensions). Every other key is a body key.
//! - `body` = BLAKE3(JCS(object of the body keys)), as lowercase hex. A
//!   tombstoned event carries `body_hash` instead of any body key.
//! - `id` = BLAKE3(JCS(header keys + `body`)), `sig` = Ed25519 by `author`
//!   over the 32 raw bytes of `id`.
//!
//! What a version may change: the body keys and their rules, the meaning of
//! `ext`, the `kind` vocabulary, limits. What it may not: anything above.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{Map, Value};

use crate::event::{
    Event, EventError, Kind, MAX_SAFE_INTEGER, VERSION, is_valid_net, leading_zero_bits, parse_hex,
};

/// Event versions this software understands (indexes, searches, shows).
pub const UNDERSTOOD_VERSIONS: (u64, u64) = (VERSION as u64, VERSION as u64);
/// Highest event version this node stores and relays as opaque data.
pub const MAX_RELAY_VERSION: u64 = 65_535;

/// Header keys, frozen. `ext` is only allowed when `v` ≥ 2.
pub const HEADER_KEYS: [&str; 8] = [
    "v", "author", "ts", "net", "thread", "parent", "kind", "pow",
];
pub const EXT_KEY: &str = "ext";
/// Keys that are neither header nor body.
pub const UNHASHED_KEYS: [&str; 2] = ["id", "sig"];
/// Present instead of the body keys on a tombstoned event.
pub const BODY_HASH_KEY: &str = "body_hash";

fn invalid(msg: impl Into<String>) -> EventError {
    EventError::Invalid(msg.into())
}

fn jcs(value: &impl serde::Serialize) -> Result<Vec<u8>, EventError> {
    Ok(serde_json_canonicalizer::to_vec(value)?)
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

/// Split a wire object into header and body and hash them. Checks the
/// frozen shape rules, not the signature.
pub fn digest(wire: &Map<String, Value>) -> Result<Digest, EventError> {
    let v = wire
        .get("v")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("v must be a positive integer"))?;
    if v == 0 {
        return Err(invalid("v must be a positive integer"));
    }
    let mut header = Map::new();
    for key in HEADER_KEYS {
        let value = wire
            .get(key)
            .ok_or_else(|| invalid(format!("missing header field {key}")))?;
        header.insert(key.to_string(), value.clone());
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
    let mut body = Map::new();
    for (key, value) in wire {
        let structural = HEADER_KEYS.contains(&key.as_str())
            || key == EXT_KEY
            || UNHASHED_KEYS.contains(&key.as_str())
            || key == BODY_HASH_KEY;
        if !structural {
            body.insert(key.clone(), value.clone());
        }
    }
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
            let hash = blake3::hash(&bytes).to_hex().to_string();
            (Some(bytes), hash)
        }
    };
    header.insert("body".to_string(), Value::String(body_hash.clone()));
    let header_jcs = jcs(&header)?;
    let id = blake3::hash(&header_jcs).into();
    Ok(Digest {
        body_jcs,
        body_hash,
        header_jcs,
        id,
    })
}

fn kind_token_ok(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 32
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// An event verified against the invariant core only. Used for versions
/// this node does not understand and for tombstoned events.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEvent {
    pub v: u64,
    pub id: String,
    pub author: String,
    pub ts: u64,
    pub net: String,
    pub thread: Option<String>,
    pub parent: Option<String>,
    pub kind: String,
    pub tombstoned: bool,
    /// The whole wire object, as received.
    pub wire: Map<String, Value>,
}

impl RawEvent {
    /// Verify the invariant core of an untrusted wire object: shape, id and
    /// signature.
    pub fn from_value(value: Value) -> Result<RawEvent, EventError> {
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
            return Err(invalid("kind must be 1-32 chars of [a-z0-9_-]"));
        }
        if pow.len() > 20 || !pow.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("pow must be a decimal nonce"));
        }
        let digest = digest(&wire)?;
        if parse_hex::<32>(&id, "id")? != digest.id {
            return Err(EventError::IdMismatch);
        }
        let key = VerifyingKey::from_bytes(&parse_hex::<32>(&author, "author")?)
            .map_err(|_| invalid("bad author key"))?;
        let sig = Signature::from_bytes(&parse_hex::<64>(&sig, "sig")?);
        key.verify(&digest.id, &sig)
            .map_err(|_| EventError::BadSignature)?;
        Ok(RawEvent {
            v,
            id,
            author,
            ts,
            net,
            thread,
            parent,
            kind,
            tombstoned: digest.body_jcs.is_none(),
            wire,
        })
    }

    pub fn pow_bits(&self) -> u32 {
        parse_hex::<32>(&self.id, "id")
            .map(|id| leading_zero_bits(&id))
            .unwrap_or(0)
    }

    pub fn thread_root(&self) -> &str {
        self.thread.as_deref().unwrap_or(&self.id)
    }

    /// How to show the kind: known kinds as is, unknown ones as `note`.
    pub fn display_kind(&self) -> Kind {
        self.kind.parse().unwrap_or(Kind::Note)
    }

    pub fn to_value(&self) -> Value {
        Value::Object(self.wire.clone())
    }
}

/// A verified event: either one this node understands, or opaque.
#[derive(Debug, Clone)]
pub enum AnyEvent {
    Known(Event),
    Opaque(RawEvent),
}

impl AnyEvent {
    /// Verify an untrusted wire event. Versions this node understands get
    /// their full rules; others only the invariant core.
    pub fn from_value(value: Value) -> Result<AnyEvent, EventError> {
        let raw = RawEvent::from_value(value)?;
        let (min, max) = UNDERSTOOD_VERSIONS;
        if raw.tombstoned || raw.v < min || raw.v > max {
            return Ok(AnyEvent::Opaque(raw));
        }
        let event: Event = serde_json::from_value(raw.to_value())?;
        event.verify()?;
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

    pub fn ts(&self) -> u64 {
        match self {
            AnyEvent::Known(e) => e.ts,
            AnyEvent::Opaque(r) => r.ts,
        }
    }

    pub fn pow_bits(&self) -> u32 {
        match self {
            AnyEvent::Known(e) => e.pow_bits(),
            AnyEvent::Opaque(r) => r.pow_bits(),
        }
    }
}

/// Sign any header/body pair under the invariant core, mining `pow_bits`.
/// `header` must hold `v`, `ts`, `net`, `thread`, `parent`, `kind` and
/// optionally `ext`; `author` and `pow` are filled in. For v1 posts use
/// `Draft::sign`, which also enforces the v1 rules; this function is for
/// tooling, test vectors and simulating other versions.
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
    let mut nonce: u64 = 0;
    let id = loop {
        wire.insert("pow".into(), Value::String(nonce.to_string()));
        let digest = digest(&wire)?;
        if leading_zero_bits(&digest.id) >= pow_bits {
            break digest.id;
        }
        nonce += 1;
    };
    wire.insert("id".into(), Value::String(hex::encode(id)));
    wire.insert(
        "sig".into(),
        Value::String(hex::encode(key.sign(&id).to_bytes())),
    );
    Ok(Value::Object(wire))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use serde_json::json;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[9; 32])
    }

    fn v2_event() -> Value {
        sign_value(
            json!({"v": 2, "ts": 1_760_600_000u64, "net": "public", "thread": null,
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
    fn future_versions_are_verified_and_kept_opaque() {
        let value = v2_event();
        let any = AnyEvent::from_value(value.clone()).unwrap();
        let AnyEvent::Opaque(raw) = any else {
            panic!("v2 must be opaque to a v1 node");
        };
        assert_eq!(raw.v, 2);
        assert_eq!(raw.display_kind(), Kind::Note);
        assert!(raw.pow_bits() >= 4);

        // Tampering with a body field, a header extension or the header
        // is detected, even though this node cannot read v2.
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
    }

    #[test]
    fn tombstones_keep_the_id() {
        let value = v2_event();
        let digest = digest(value.as_object().unwrap()).unwrap();
        let mut tomb = Map::new();
        for (k, v) in value.as_object().unwrap() {
            if HEADER_KEYS.contains(&k.as_str())
                || k == EXT_KEY
                || UNHASHED_KEYS.contains(&k.as_str())
            {
                tomb.insert(k.clone(), v.clone());
            }
        }
        tomb.insert(BODY_HASH_KEY.into(), Value::String(digest.body_hash));
        let raw = RawEvent::from_value(Value::Object(tomb.clone())).unwrap();
        assert!(raw.tombstoned);
        assert_eq!(raw.id, value["id"]);
        // A body field next to body_hash is rejected.
        tomb.insert("text".into(), json!("sneaky"));
        assert!(RawEvent::from_value(Value::Object(tomb)).is_err());
    }

    #[test]
    fn ext_is_reserved_for_future_versions() {
        let event = Draft {
            net: "public".into(),
            kind: Some(Kind::Note),
            text: "v1".into(),
            ..Draft::default()
        }
        .sign(&key(), 1, 0)
        .unwrap();
        let mut value = serde_json::to_value(&event).unwrap();
        value["ext"] = json!({});
        assert!(RawEvent::from_value(value).is_err());
    }
}
