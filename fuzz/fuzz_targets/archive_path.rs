#![no_main]
//! Archive entry names: whatever comes out of `sanitize_entry_path` must stay
//! inside the extraction directory.
use libfuzzer_sys::fuzz_target;
use std::path::{Component, Path};

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data);
    if let Some(p) = convolith::archive::sanitize_entry_path(&name, 4096) {
        assert!(p.is_relative());
        assert!(p.components().all(|c| matches!(c, Component::Normal(_))));
        assert!(Path::new("/stage").join(&p).starts_with("/stage"));
    }
});
