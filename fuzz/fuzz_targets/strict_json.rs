//! The strict JSON profile and JCS: parsing never panics, and canonical
//! output is a fixed point (parse(JCS(x)) == x, JCS(JCS(x)) == JCS(x)).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = wats::strict_json::parse(data) else {
        return;
    };
    let canonical = serde_json_canonicalizer::to_vec(&value).expect("canonicalizes");
    let reparsed = wats::strict_json::parse(&canonical).expect("canonical JSON is strict JSON");
    assert_eq!(reparsed, value);
    let again = serde_json_canonicalizer::to_vec(&reparsed).expect("canonicalizes");
    assert_eq!(again, canonical);
});
