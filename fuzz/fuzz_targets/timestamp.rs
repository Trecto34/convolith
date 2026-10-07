#![no_main]
//! Timestamp parsing never panics, and anything it accepts renders to an
//! RFC 3339 string that parses back to the same instant.
use convolith::timeutil::{parse_epoch_like, parse_json_timestamp, parse_rfc3339};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    if let Some(u) = parse_rfc3339(&s) {
        assert_eq!(parse_rfc3339(&u.to_rfc3339()), Some(u));
    }
    let _ = parse_json_timestamp(&serde_json::Value::String(s.into_owned()));
    if data.len() >= 8 {
        let n = i64::from_le_bytes(data[..8].try_into().unwrap());
        let _ = parse_epoch_like(n);
        let _ = parse_json_timestamp(&serde_json::json!(n));
    }
});
