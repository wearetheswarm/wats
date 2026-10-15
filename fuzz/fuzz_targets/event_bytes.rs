//! Untrusted event bytes: parsing and checking never panic, and an accepted
//! event re-serializes to an event with the same id that is accepted again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use wats::wire::AnyEvent;

fuzz_target!(|data: &[u8]| {
    let Ok(event) = AnyEvent::from_bytes(data) else {
        return;
    };
    let value = match &event {
        AnyEvent::Known(e) => serde_json::to_value(e).expect("serializes"),
        AnyEvent::Opaque(r) => r.to_value(),
    };
    let bytes = serde_json::to_vec(&value).expect("serializes");
    let again = AnyEvent::from_bytes(&bytes).expect("an accepted event is accepted again");
    assert_eq!(event.id(), again.id());
});
