#![no_main]
//! Canonical event JSON: any record that deserializes re-serializes to bytes
//! that deserialize to the same event, and serializing twice is identical.
use convolith::model::Event;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(e) = serde_json::from_slice::<Event>(data) else { return };
    let a = serde_json::to_string(&e).unwrap();
    let back: Event = serde_json::from_str(&a).unwrap();
    assert_eq!(back, e);
    assert_eq!(serde_json::to_string(&back).unwrap(), a);
});
