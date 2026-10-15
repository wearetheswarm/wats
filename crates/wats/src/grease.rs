//! GREASE (ADR 0010, A8; after RFC 8701 and RFC 9170): names starting with
//! `_grease` are reserved, every node ignores them, and our nodes emit them
//! at a low rate so that extension points stay exercised and nobody starts
//! rejecting unknown names.
//!
//! Emitted in gossip envelope fields, `hello.features`, `/wats` request and
//! response fields, and as whole request types. Never in events: they are
//! permanent (GREASE events only exist in tests and vectors).

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use crate::wire::{GREASE_PREFIX, is_grease};

/// One emission in `RATE` opportunities, by default.
pub const DEFAULT_RATE: u64 = 8;

static RATE: AtomicU64 = AtomicU64::new(DEFAULT_RATE);

/// Change the emission rate (1 = always, 0 = never). For tests.
pub fn set_rate(rate: u64) {
    RATE.store(rate, Ordering::Relaxed);
}

fn random() -> u64 {
    getrandom::u64().unwrap_or(0)
}

/// Whether to emit GREASE at this opportunity.
pub fn roll() -> bool {
    match RATE.load(Ordering::Relaxed) {
        0 => false,
        rate => random().is_multiple_of(rate),
    }
}

/// A random reserved name.
pub fn name() -> String {
    format!("{GREASE_PREFIX}_{:04x}", random() & 0xffff)
}

/// A random small value of any JSON type (integers only).
pub fn value() -> Value {
    let r = random();
    match r % 5 {
        0 => json!(r & 0xffff),
        1 => json!(name()),
        2 => json!(r & 1 == 0),
        3 => json!([name(), r & 0xff]),
        _ => json!({ name(): null }),
    }
}

/// Maybe add one GREASE field to a JSON object.
pub fn maybe_add_field(object: &mut Map<String, Value>) {
    if roll() {
        object.insert(name(), value());
    }
}

/// Maybe add a GREASE field to a serialized message (an object).
pub fn maybe_add_to(value: &mut Value) {
    if let Value::Object(object) = value {
        maybe_add_field(object);
    }
}

/// Drop GREASE entries from a list of names (e.g. received features).
pub fn strip(names: &mut Vec<String>) {
    names.retain(|n| !is_grease(n));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_reserved_and_strippable() {
        let n = name();
        assert!(is_grease(&n));
        let mut features = vec!["sync".to_string(), n];
        strip(&mut features);
        assert_eq!(features, vec!["sync".to_string()]);
        // Any value is valid under the strict JSON profile.
        for _ in 0..50 {
            let bytes = serde_json::to_vec(&json!({ name(): value() })).unwrap();
            crate::strict_json::parse(&bytes).unwrap();
        }
    }
}
