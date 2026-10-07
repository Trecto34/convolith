//! v0.1.0 -> web-export parsers: ChatGPT event and conversation ids must not change.
//! The golden ids below were produced by the v0.1.0 binary (tag `v0.1.0`) from the
//! same fixtures; this test fails if a parser change would make a re-import of an
//! existing v0.1.0 ChatGPT archive create new events or conversations.

mod webexport_common;
use std::collections::BTreeSet;
use webexport_common::*;

const UUID_FIXTURE_EVENTS: [&str; 6] = [
    "ev_2e066180d32e038dee042943",
    "ev_304363d2b981a83876f1f2bc",
    "ev_4658692760315adf07d31141",
    "ev_7a95fc745cc81cf70e4e96e0",
    "ev_a7072c392b09d07aad056908",
    "ev_e5054034c06a8eb22b82f625",
];
const FIXTURE_EVENTS: [&str; 3] = [
    "ev_e0d7cde0b0b9501284e88734",
    "ev_e37a20d2cd0d51b6b602e025",
    "ev_f20bcf59334463c24df8f579",
];

fn ids(
    fixture: &str,
    tag: &str,
) -> (
    BTreeSet<String>,
    BTreeSet<String>,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let t = tmp(tag);
    let src = t.join("in/conversations.json");
    write(
        &src,
        std::fs::read(format!(
            "{}/fixtures/chatgpt/{fixture}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    );
    let out = t.join("out");
    import(&t.join("in"), &out);
    let evs = events(&out);
    (
        evs.iter().map(|e| e.event_id.clone()).collect(),
        evs.iter().map(|e| e.conversation_id.clone()).collect(),
        t.join("in"),
        out,
    )
}

#[test]
fn uuid_export_keeps_its_v010_ids_including_every_branch_node() {
    let (e, c, input, out) = ids("uuid-branches.json", "gpt-compat-a");
    assert_eq!(
        e,
        UUID_FIXTURE_EVENTS.iter().map(|s| s.to_string()).collect()
    );
    assert_eq!(
        c.iter().collect::<Vec<_>>(),
        ["cv_5a74a11616531de8d7d62107"]
    );
    // and re-importing is a no-op (what an existing v0.1.0 archive would see)
    let log = import(&input, &out);
    assert!(log.contains("events: 0 new, 6 duplicate"), "{log}");
}

#[test]
fn short_id_fixture_keeps_its_v010_ids_too() {
    let (e, c, _, _) = ids("conversations.json", "gpt-compat-b");
    assert_eq!(e, FIXTURE_EVENTS.iter().map(|s| s.to_string()).collect());
    assert_eq!(
        c.iter().collect::<Vec<_>>(),
        ["cv_a01bbc899728b50416060391"]
    );
}
