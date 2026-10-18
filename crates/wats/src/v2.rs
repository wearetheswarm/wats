//! Event v2 validity (ADR 0015, §2 and §8): the rules every node, member of
//! a private network or not, checks without decrypting anything.
//!
//! v2 has two kinds in clear: `sealed` (an MLS message inside a private
//! network) and `submit` (a sealed-inbox submission on a public net). The real
//! content sits in `ct`, so a v2 event is stored as an opaque row and never
//! indexed or shown. Group signatures, MLS and HPKE are member-side policy,
//! not validity: only the shape of the group signature is checked here.

// Network input must never panic the node (security review, item D).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Map, Value};

use crate::event::{EventError, MAX_EVENT_BYTES, MAX_GREASE_BYTES, MAX_SAFE_INTEGER, parse_hex};
use crate::wire::{
    EXT_KEY, REFS_KEY, RawEvent, body_object, check_v1_key, is_grease, mine, prefixed_message,
    verify_prefixed,
};

/// The event version of private networks.
pub const VERSION: u64 = 2;
/// The author signature covers `"wats/event/v2" || id`.
pub const SIG_PREFIX_V2: &[u8] = b"wats/event/v2";
/// The epoch's group signature (`sealed` only) covers `"wats/private/v2" || id`.
pub const GROUP_SIG_PREFIX: &[u8] = b"wats/private/v2";
/// A message inside a private network.
pub const KIND_SEALED: &str = "sealed";
/// A submission to a sealed inbox, on a public net.
pub const KIND_SUBMIT: &str = "submit";
/// `net` of a private network: this prefix and 32 lowercase hex characters.
pub const PRIVATE_NET_PREFIX: &str = "private:";
/// `ext.to` of a submission: this prefix and 32 lowercase hex characters.
pub const INBOX_PREFIX: &str = "inbox:";
/// Largest decoded `ct`.
pub const MAX_CT_BYTES: usize = 12_288;
/// Smallest decoded `ct` of a submission (an AEAD tag).
pub const MIN_SUBMIT_CT_BYTES: usize = 16;
/// Decoded size of `enc`, the X-Wing encapsulation of a submission.
pub const ENC_BYTES: usize = 1120;
/// Hex length of `sig`: author signature, then group signature for
/// `sealed`; author signature alone for `submit`.
pub const SEALED_SIG_HEX: usize = 256;
pub const SUBMIT_SIG_HEX: usize = 128;

fn invalid(msg: impl Into<String>) -> EventError {
    EventError::Invalid(msg.into())
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_value(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c.wrapping_sub(b'A'),
        b'a'..=b'z' => c.wrapping_sub(b'a').wrapping_add(26),
        b'0'..=b'9' => c.wrapping_sub(b'0').wrapping_add(52),
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

/// Decode canonical base64url without padding: alphabet `A-Z a-z 0-9 - _`,
/// no `=`, a length that is not 1 modulo 4, and no non-zero bit after the
/// last whole byte. `None` for anything that does not round-trip.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() / 4).saturating_mul(3).saturating_add(2));
    let (mut acc, mut nbits) = (0u32, 0u32);
    for &c in bytes {
        acc = (acc.wrapping_shl(6)) | b64url_value(c)?;
        nbits = nbits.wrapping_add(6);
        if nbits >= 8 {
            nbits = nbits.wrapping_sub(8);
            out.push((acc.wrapping_shr(nbits) & 0xff) as u8);
            acc &= (1u32.wrapping_shl(nbits)).wrapping_sub(1);
        }
    }
    // What is left are the unused trailing bits: they must be zero.
    (acc == 0).then_some(out)
}

/// Encode as canonical base64url without padding.
pub fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = byte(0).wrapping_shl(16) | byte(1).wrapping_shl(8) | byte(2);
        for i in 0..=chunk.len() {
            let shift = 18u32.wrapping_sub(6u32.wrapping_mul(i as u32));
            let index = (n.wrapping_shr(shift) & 63) as usize;
            if let Some(&c) = B64URL.get(index) {
                out.push(char::from(c));
            }
        }
    }
    out
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `net` names a private network: `private:` and 32 lowercase hex characters.
pub fn is_private_net(net: &str) -> bool {
    net.strip_prefix(PRIVATE_NET_PREFIX)
        .is_some_and(|rest| is_lower_hex(rest, 32))
}

/// Whether `to` names an inbox: `inbox:` and 32 lowercase hex characters.
pub fn is_inbox_id(to: &str) -> bool {
    to.strip_prefix(INBOX_PREFIX)
        .is_some_and(|rest| is_lower_hex(rest, 32))
}

/// Check the keys of an object (`ext` or the body): exactly `required`,
/// plus `_grease*` keys that take at most [`MAX_GREASE_BYTES`] of JCS
/// together, keys included.
fn check_keys(obj: &Map<String, Value>, required: &[&str], what: &str) -> Result<(), EventError> {
    for key in required {
        if !obj.contains_key(*key) {
            return Err(invalid(format!("{what} must carry {key}")));
        }
    }
    let mut grease = Map::new();
    for (key, value) in obj {
        if required.contains(&key.as_str()) {
            continue;
        }
        if !is_grease(key) {
            return Err(invalid(format!("unknown {what} key {key:?}")));
        }
        grease.insert(key.clone(), value.clone());
    }
    if !grease.is_empty() && serde_json_canonicalizer::to_vec(&grease)?.len() > MAX_GREASE_BYTES {
        return Err(invalid(format!(
            "GREASE {what} keys exceed {MAX_GREASE_BYTES} bytes canonicalized"
        )));
    }
    Ok(())
}

/// A binary field: canonical base64url decoding to `min..=max` bytes.
fn check_binary(
    body: &Map<String, Value>,
    key: &str,
    min: usize,
    max: usize,
) -> Result<(), EventError> {
    let text = body
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{key} must be a string")))?;
    let bytes = b64url_decode(text)
        .ok_or_else(|| invalid(format!("{key} is not canonical base64url without padding")))?;
    if !(min..=max).contains(&bytes.len()) {
        return Err(invalid(if min == max {
            format!("{key} must decode to {min} bytes, not {}", bytes.len())
        } else {
            format!(
                "{key} must decode to {min}-{max} bytes, not {}",
                bytes.len()
            )
        }));
    }
    Ok(())
}

/// The v2 rules of an event that passed the invariant core (ADR 0015 §8):
/// kind, header, `ext`, body (unless tombstoned) and size. The signature
/// is [`verify_signature`].
pub fn check_rules(raw: &RawEvent) -> Result<(), EventError> {
    let sealed = match raw.kind.as_str() {
        KIND_SEALED => true,
        KIND_SUBMIT => false,
        other => {
            return Err(invalid(format!(
                "v2 kind {other:?} is not sealed or submit"
            )));
        }
    };
    if raw.thread.is_some() || raw.parent.is_some() {
        return Err(invalid("v2 thread and parent must be null"));
    }
    match raw.wire.get(REFS_KEY) {
        Some(Value::Array(refs)) if refs.is_empty() => {}
        Some(_) => return Err(invalid("v2 refs must be []")),
        None => return Err(invalid("v2 refs must be present, as []")),
    }
    check_v1_key(&parse_hex::<32>(&raw.author, "author")?, "author")?;
    let ext = match raw.wire.get(EXT_KEY) {
        Some(Value::Object(ext)) => ext,
        _ => return Err(invalid("v2 ext is required")),
    };
    if sealed {
        if !is_private_net(&raw.net) {
            return Err(invalid("sealed net must be private:<32 hex>"));
        }
        check_keys(ext, &["epoch"], "ext")?;
        if !ext
            .get("epoch")
            .and_then(Value::as_u64)
            .is_some_and(|n| n <= MAX_SAFE_INTEGER)
        {
            return Err(invalid("ext.epoch must be an integer from 0 to 2^53 - 1"));
        }
    } else {
        if raw.net.starts_with(PRIVATE_NET_PREFIX) {
            return Err(invalid("submit net must not be a private network"));
        }
        check_keys(ext, &["to"], "ext")?;
        if !ext
            .get("to")
            .and_then(Value::as_str)
            .is_some_and(is_inbox_id)
        {
            return Err(invalid("ext.to must be inbox:<32 hex>"));
        }
    }
    if !raw.tombstoned {
        let body = body_object(&raw.wire);
        if sealed {
            check_keys(&body, &["ct"], "body")?;
            check_binary(&body, "ct", 1, MAX_CT_BYTES)?;
        } else {
            check_keys(&body, &["enc", "ct"], "body")?;
            check_binary(&body, "enc", ENC_BYTES, ENC_BYTES)?;
            check_binary(&body, "ct", MIN_SUBMIT_CT_BYTES, MAX_CT_BYTES)?;
        }
    }
    let size = serde_json_canonicalizer::to_vec(&raw.wire)?.len();
    if size > MAX_EVENT_BYTES {
        return Err(invalid(format!(
            "event is {size} bytes canonicalized, above {MAX_EVENT_BYTES}"
        )));
    }
    Ok(())
}

/// The v2 signature suite: `sig` is the author's Ed25519 signature over
/// `"wats/event/v2" || id` (v1 key checks, ZIP 215), followed for `sealed`
/// by the group signature, checked as shape only: its scalar `S` must be
/// canonical (< l), which needs no group key.
pub fn verify_signature(raw: &RawEvent) -> Result<(), EventError> {
    let sig = raw
        .wire
        .get("sig")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = match raw.kind.as_str() {
        KIND_SEALED => SEALED_SIG_HEX,
        KIND_SUBMIT => SUBMIT_SIG_HEX,
        other => {
            return Err(invalid(format!(
                "v2 kind {other:?} is not sealed or submit"
            )));
        }
    };
    if sig.len() != expected {
        return Err(invalid(format!(
            "{} sig must be {expected} hex characters",
            raw.kind
        )));
    }
    let (author_sig, group_sig) = sig.split_at(SUBMIT_SIG_HEX);
    let author = parse_hex::<32>(&raw.author, "author")?;
    let id = parse_hex::<32>(&raw.id, "id")?;
    verify_prefixed(
        SIG_PREFIX_V2,
        &author,
        &id,
        &parse_hex::<64>(author_sig, "sig")?,
    )?;
    if !group_sig.is_empty() {
        let group = parse_hex::<64>(group_sig, "group signature")?;
        let mut s = [0u8; 32];
        s.copy_from_slice(group.get(32..).unwrap_or(&[0u8; 32]));
        let canonical: Option<curve25519_dalek::Scalar> =
            curve25519_dalek::Scalar::from_canonical_bytes(s).into();
        if canonical.is_none() {
            return Err(invalid("group signature scalar is not canonical (S >= l)"));
        }
    }
    Ok(())
}

/// The author signature of a v2 event.
pub fn sign_v2(key: &SigningKey, id: &[u8; 32]) -> [u8; 64] {
    key.sign(&prefixed_message(SIG_PREFIX_V2, id)).to_bytes()
}

/// The group signature of a `sealed` event, with the epoch's group key.
pub fn sign_group(group: &SigningKey, id: &[u8; 32]) -> [u8; 64] {
    group
        .sign(&prefixed_message(GROUP_SIG_PREFIX, id))
        .to_bytes()
}

/// `sig` for a v2 event: the author signature, then the group signature
/// when `group` is given (`sealed`).
pub fn v2_sig(author: &SigningKey, group: Option<&SigningKey>, id: &[u8; 32]) -> String {
    let mut sig = hex::encode(sign_v2(author, id));
    if let Some(group) = group {
        sig.push_str(&hex::encode(sign_group(group, id)));
    }
    sig
}

/// Sign a v2 event, mining `pow_bits`: for tooling and test vectors. The
/// header holds `v`, `ts`, `net`, `thread`, `parent`, `refs`, `kind` and
/// `ext`; `author`, `pow`, `id` and `sig` are filled in. Nothing is
/// validated, so invalid events can be built on purpose.
pub fn sign_v2_value(
    header: Map<String, Value>,
    body: Map<String, Value>,
    author: &SigningKey,
    group: Option<&SigningKey>,
    pow_bits: u32,
) -> Result<Value, EventError> {
    let mut wire = body;
    for (k, v) in header {
        wire.insert(k, v);
    }
    wire.insert(
        "author".into(),
        Value::String(hex::encode(author.verifying_key().as_bytes())),
    );
    let id = mine(&mut wire, pow_bits)?;
    wire.insert("id".into(), Value::String(hex::encode(id)));
    wire.insert("sig".into(), Value::String(v2_sig(author, group, &id)));
    Ok(Value::Object(wire))
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
    use crate::wire::{AnyEvent, digest};
    use serde_json::json;

    fn obj(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn author() -> SigningKey {
        SigningKey::from_bytes(&[21; 32])
    }

    fn group() -> SigningKey {
        SigningKey::from_bytes(&[22; 32])
    }

    fn submit_parts() -> (Map<String, Value>, Map<String, Value>) {
        (
            obj(
                json!({"v": 2, "ts": 1_760_800_000u64, "net": "public", "thread": null,
                       "parent": null, "refs": [], "kind": "submit",
                       "ext": {"to": format!("inbox:{}", "ab".repeat(16))}}),
            ),
            obj(json!({"enc": b64url_encode(&[7; ENC_BYTES]),
                       "ct": b64url_encode(&[9; 40])})),
        )
    }

    fn sealed_parts() -> (Map<String, Value>, Map<String, Value>) {
        (
            obj(
                json!({"v": 2, "ts": 1_760_800_000u64, "net": format!("private:{}", "cd".repeat(16)),
                       "thread": null, "parent": null, "refs": [], "kind": "sealed",
                       "ext": {"epoch": 3}}),
            ),
            obj(json!({"ct": b64url_encode(b"an MLS PrivateMessage")})),
        )
    }

    fn submit() -> Value {
        let (h, b) = submit_parts();
        sign_v2_value(h, b, &author(), None, 0).unwrap()
    }

    fn sealed() -> Value {
        let (h, b) = sealed_parts();
        sign_v2_value(h, b, &author(), Some(&group()), 0).unwrap()
    }

    /// Build a submit (or sealed) after `f` changed its header or body.
    fn with(
        sealed: bool,
        f: impl Fn(&mut Map<String, Value>, &mut Map<String, Value>),
    ) -> Result<AnyEvent, EventError> {
        let (mut h, mut b) = if sealed {
            sealed_parts()
        } else {
            submit_parts()
        };
        f(&mut h, &mut b);
        let g = group();
        let value = sign_v2_value(h, b, &author(), sealed.then_some(&g), 0).unwrap();
        AnyEvent::from_value(value)
    }

    fn rejected(sealed: bool, f: impl Fn(&mut Map<String, Value>, &mut Map<String, Value>)) {
        assert!(with(sealed, f).is_err());
    }

    #[test]
    fn base64url_is_canonical() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let text = b64url_encode(&bytes);
            assert_eq!(b64url_decode(&text).unwrap(), bytes, "{text}");
            assert!(!text.contains('='));
        }
        assert_eq!(b64url_encode(b"\xfb\xff"), "-_8");
        assert_eq!(b64url_decode("-_8").unwrap(), b"\xfb\xff");
        assert_eq!(b64url_decode("").unwrap(), b"");
        for bad in [
            "QQ==", // padding
            "QR",   // non-zero trailing bits (QQ is canonical)
            "QUF",  // QUE is canonical
            "Q",    // length 1 mod 4
            "QUJDR", "+/8", // standard alphabet
            "QU J", "QUé=",
        ] {
            assert!(b64url_decode(bad).is_none(), "{bad}");
        }
        assert_eq!(b64url_decode("QQ").unwrap(), b"A");
        assert_eq!(b64url_decode("QUE").unwrap(), b"AA");
    }

    #[test]
    fn valid_v2_events_are_checked_and_opaque() {
        for value in [submit(), sealed()] {
            let AnyEvent::Opaque(raw) = AnyEvent::from_value(value).unwrap() else {
                panic!("v2 events are opaque");
            };
            assert_eq!(raw.v, 2);
            assert!(raw.signature_checked);
        }
        // GREASE in ext and body, within the cap.
        assert!(
            with(true, |h, b| {
                h["ext"]["_grease_1"] = json!("x");
                b.insert("_grease_2".into(), json!([1, 2]));
            })
            .is_ok()
        );
    }

    #[test]
    fn tombstones_keep_header_and_signature_rules() {
        let full = submit();
        let mut tomb: Map<String, Value> = full
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, _)| crate::wire::is_structural_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        tomb.insert(
            "body_hash".into(),
            json!(digest(full.as_object().unwrap()).unwrap().body_hash),
        );
        let AnyEvent::Opaque(raw) = AnyEvent::from_value(Value::Object(tomb.clone())).unwrap()
        else {
            panic!("opaque");
        };
        assert!(raw.tombstoned && raw.signature_checked);
        let mut bad = tomb.clone();
        bad.insert("sig".into(), json!("00".repeat(64)));
        assert!(AnyEvent::from_value(Value::Object(bad)).is_err());
    }

    #[test]
    fn header_rules() {
        for s in [true, false] {
            rejected(s, |h, _| h["kind"] = json!("poll"));
            rejected(s, |h, _| {
                h["thread"] = json!("aa".repeat(32));
                h["parent"] = json!("aa".repeat(32));
            });
            rejected(s, |h, _| h["refs"] = json!(["aa".repeat(32)]));
            rejected(s, |h, _| {
                h.remove("refs");
            });
            rejected(s, |h, _| {
                h.remove("ext");
            });
            rejected(s, |h, _| h["ext"]["extra"] = json!(1));
            rejected(s, |h, _| h["ext"]["_grease_x"] = json!("g".repeat(60)));
        }
        rejected(true, |h, _| h["net"] = json!("public"));
        rejected(true, |h, _| {
            h["net"] = json!(format!("private:{}", "cd".repeat(15)))
        });
        rejected(true, |h, _| h["ext"] = json!({}));
        rejected(true, |h, _| h["ext"] = json!({"epoch": -1}));
        rejected(true, |h, _| h["ext"] = json!({"epoch": "3"}));
        rejected(false, |h, _| {
            h["net"] = json!(format!("private:{}", "cd".repeat(16)))
        });
        rejected(false, |h, _| h["ext"] = json!({"to": "inbox:ABAB"}));
        rejected(false, |h, _| {
            h["ext"] = json!({"to": format!("inbox:{}", "AB".repeat(16))})
        });
        rejected(false, |h, _| {
            h["ext"] = json!({"to": format!("private:{}", "ab".repeat(16))})
        });
        // epoch at its bounds.
        assert!(with(true, |h, _| h["ext"] = json!({"epoch": 0})).is_ok());
        assert!(with(true, |h, _| h["ext"] = json!({"epoch": MAX_SAFE_INTEGER})).is_ok());
    }

    #[test]
    fn body_rules() {
        rejected(true, |_, b| {
            b.insert("text".into(), json!("in clear"));
        });
        rejected(true, |_, b| b["ct"] = json!(""));
        rejected(true, |_, b| b["ct"] = json!("QQ=="));
        rejected(true, |_, b| b["ct"] = json!("QR"));
        rejected(true, |_, b| b["ct"] = json!(1));
        rejected(true, |_, b| {
            b.remove("ct");
        });
        rejected(true, |_, b| {
            b["ct"] = json!(b64url_encode(&[1; MAX_CT_BYTES + 1]))
        });
        assert!(with(true, |_, b| b["ct"] = json!(b64url_encode(&[1; 11_000]))).is_ok());
        rejected(false, |_, b| {
            b["enc"] = json!(b64url_encode(&[7; ENC_BYTES - 1]))
        });
        rejected(false, |_, b| {
            b.remove("enc");
        });
        rejected(false, |_, b| {
            b["ct"] = json!(b64url_encode(&[1; MIN_SUBMIT_CT_BYTES - 1]))
        });
        rejected(false, |_, b| {
            b.insert("_grease_big".into(), json!("g".repeat(60)));
        });
    }

    #[test]
    fn signature_rules() {
        let check = |value: &Value, sig: String| {
            let mut v = value.clone();
            v["sig"] = json!(sig);
            AnyEvent::from_value(v)
        };
        let s = sealed();
        let id: [u8; 32] = hex::decode(s["id"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        // The author signature alone is not enough for sealed.
        assert!(check(&s, hex::encode(sign_v2(&author(), &id))).is_err());
        // The group signature is shape only: any key, any message, S < l.
        let other = hex::encode(sign_group(&author(), &[0; 32]));
        assert!(
            check(
                &s,
                format!("{}{other}", hex::encode(sign_v2(&author(), &id)))
            )
            .is_ok()
        );
        let high_s = format!(
            "{}{}{}",
            hex::encode(sign_v2(&author(), &id)),
            "00".repeat(32),
            "ff".repeat(32)
        );
        assert!(check(&s, high_s).is_err());
        // A v1 signature never passes as a v2 one.
        let sub = submit();
        let id: [u8; 32] = hex::decode(sub["id"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert!(check(&sub, hex::encode(crate::wire::sign_v1(&author(), &id))).is_err());
        assert!(check(&sub, v2_sig(&author(), Some(&group()), &id)).is_err());
        assert!(check(&sub, hex::encode(sign_v2(&group(), &id))).is_err());
    }

    #[test]
    fn size_is_capped() {
        // 12,288 bytes of ct are 16,384 base64url characters: the 16 KiB
        // cap on the canonical event binds before the ct cap does.
        assert!(with(false, |_, b| b["ct"] = json!(b64url_encode(&[1; 10_000]))).is_ok());
        let err = with(false, |_, b| {
            b["ct"] = json!(b64url_encode(&[1; MAX_CT_BYTES]))
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("canonicalized"), "{err}");
    }
}
