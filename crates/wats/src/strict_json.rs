//! Strict JSON input profile (ADR 0010, A3): RFC 8259 restricted along the
//! lines of I-JSON (RFC 7493), so that every implementation parses an event
//! to the same value before JCS (RFC 8785) canonicalizes it.
//!
//! On top of what `serde_json` already rejects (invalid UTF-8, unpaired
//! surrogate escapes, trailing data), this profile rejects:
//! - duplicate object keys (`serde_json::Value` silently keeps the last);
//! - any number that is not an integer within ±(2^53 − 1): no fraction, no
//!   exponent, no large integers;
//! - nesting deeper than [`MAX_DEPTH`].
//!
//! Strings are hashed as received: no Unicode normalization.

// Network input must never panic the node (security review, item D).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::HashSet;
use std::fmt;

use serde::de::{DeserializeOwned, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::event::MAX_SAFE_INTEGER;

/// Deepest object or array nesting accepted in any message (events are
/// further limited by the event rules).
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StrictJsonError(String);

/// Parse bytes under the strict profile.
pub fn parse(bytes: &[u8]) -> Result<Value, StrictJsonError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = Strict { depth: 0 }
        .deserialize(&mut de)
        .map_err(|e| StrictJsonError(e.to_string()))?;
    de.end().map_err(|e| StrictJsonError(e.to_string()))?;
    Ok(value)
}

/// Parse bytes under the strict profile, then deserialize into `T`.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StrictJsonError> {
    serde_json::from_value(parse(bytes)?).map_err(|e| StrictJsonError(e.to_string()))
}

/// Nesting depth of a value: 0 for scalars, 1 for a flat object or array.
pub fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.iter().map(depth).max().unwrap_or(0).saturating_add(1),
        Value::Object(map) => map.values().map(depth).max().unwrap_or(0).saturating_add(1),
        _ => 0,
    }
}

/// Whether every number in `value` is an integer within ±(2^53 − 1).
pub fn integers_only(value: &Value) -> bool {
    match value {
        Value::Number(n) => safe_integer(n),
        Value::Array(items) => items.iter().all(integers_only),
        Value::Object(map) => map.values().all(integers_only),
        _ => true,
    }
}

fn safe_integer(n: &Number) -> bool {
    if let Some(i) = n.as_i64() {
        i.unsigned_abs() <= MAX_SAFE_INTEGER
    } else if let Some(u) = n.as_u64() {
        u <= MAX_SAFE_INTEGER
    } else {
        false
    }
}

struct Strict {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for Strict {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value under the strict profile")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        if v.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(E::custom("integer outside ±(2^53 - 1)"));
        }
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        if v > MAX_SAFE_INTEGER {
            return Err(E::custom("integer outside ±(2^53 - 1)"));
        }
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Value, E> {
        Err(E::custom(
            "non-integer number (fraction, exponent or out of range): only integers are allowed",
        ))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let depth = self.depth.saturating_add(1);
        if depth > MAX_DEPTH {
            return Err(serde::de::Error::custom("JSON nested too deeply"));
        }
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Strict { depth })? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let depth = self.depth.saturating_add(1);
        if depth > MAX_DEPTH {
            return Err(serde::de::Error::custom("JSON nested too deeply"));
        }
        let mut map = Map::new();
        let mut seen = HashSet::new();
        while let Some(key) = access.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
            }
            let value = access.next_value_seed(Strict { depth })?;
            map.insert(key, value);
        }
        Ok(Value::Object(map))
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

    #[test]
    fn accepts_plain_json() {
        let v = parse(br#"{"a":[1,-2,"x",null,true,{"b":9007199254740991}]}"#).unwrap();
        assert_eq!(depth(&v), 3);
        assert!(integers_only(&v));
    }

    #[test]
    fn rejects_what_the_profile_forbids() {
        for (bad, why) in [
            (&br#"{"a":1,"a":2}"#[..], "duplicate"),
            (br#"{"a":{"b":1,"b":1}}"#, "duplicate"),
            (br#"{"a":1.5}"#, "non-integer"),
            (br#"{"a":1e3}"#, "non-integer"),
            (br#"{"a":1.0}"#, "non-integer"),
            (br#"{"a":9007199254740992}"#, "outside"),
            (br#"{"a":-9007199254740992}"#, "outside"),
            (br#"{"a":123456789012345678901234567890}"#, "non-integer"),
            (br#"{"a":"\ud800"}"#, "surrogate"),
            (b"{\"a\":\"\xff\"}", "UTF-8"),
            (br#"{"a":1} x"#, "trailing"),
        ] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(
                err.contains(why) || why == "UTF-8" || why == "surrogate",
                "{:?}: {err}",
                String::from_utf8_lossy(bad)
            );
        }
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(
            parse(deep.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("deep")
        );
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        parse(ok.as_bytes()).unwrap();
    }

    #[test]
    fn no_unicode_normalization() {
        // "é" precomposed (U+00E9) and decomposed (e + U+0301) stay distinct.
        let a = parse("{\"t\":\"\u{e9}\"}".as_bytes()).unwrap();
        let b = parse("{\"t\":\"e\u{301}\"}".as_bytes()).unwrap();
        assert_ne!(a, b);
    }
}
