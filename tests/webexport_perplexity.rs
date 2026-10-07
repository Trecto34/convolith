//! Perplexity user-data export parser (`perplexity_export`): synthetic content in the
//! structure of a real export (verified): `{"conversations":[{context_uuid,
//! context_title, created_at, updated_at, mode, collection_uuid, entries:[{entry_uuid,
//! query, answer, created_at, engine_mode, label, query_status}]}]}` plus a workbook.

mod webexport_common;
use convolith::model::{Event, Role};
use serde_json::{json, Value};
use webexport_common::*;

const C1: &str = "9d2b0000-0000-4000-8000-0000000000d1";
const E1: &str = "9d2b0000-0000-4000-8000-0000000000e1";
const E2: &str = "9d2b0000-0000-4000-8000-0000000000e2";
const E3: &str = "9d2b0000-0000-4000-8000-0000000000e3";

fn export(extra: bool) -> Value {
    let mut entries = vec![
        json!({"entry_uuid": E1, "query": "synthetic question", "answer": "synthetic answer",
               "created_at": "2026-05-06T07:08:09.123456Z", "engine_mode": "pro", "label": null, "query_status": "COMPLETED"}),
        // a failed entry: empty answer
        json!({"entry_uuid": E2, "query": "failed question", "answer": "",
               "created_at": "2026-05-06T07:09:00.000000Z", "engine_mode": null, "label": null, "query_status": "FAILED"}),
    ];
    if extra {
        entries.push(json!({"entry_uuid": E3, "query": "third question", "answer": "third answer",
               "created_at": "2026-05-06T07:10:00.000000Z", "engine_mode": "reasoning", "label": null, "query_status": "COMPLETED"}));
    }
    json!({"conversations": [{"context_uuid": C1, "context_title": "synthetic thread", "collection_uuid": null,
        "created_at": "2026-05-06T07:08:00.000000Z", "updated_at": "2026-05-06T07:10:00.000000Z", "mode": "COPILOT", "entries": entries}]})
}

fn zip(dir: &std::path::Path, name: &str, doc: Value) -> std::path::PathBuf {
    let z = dir.join(name);
    zip_of(
        &z,
        &[
            (
                "conversations-20261006_102903-f205ff64.json",
                doc.to_string().into_bytes(),
            ),
            ("user-data-20261006_102903-f205ff64.xlsx", {
                let book = dir.join("book.xlsx");
                zip_of(&book, &[("xl/workbook.xml", b"<workbook/>".to_vec())]);
                std::fs::read(&book).unwrap()
            }),
        ],
    );
    z
}

fn px(out: &std::path::Path) -> Vec<Event> {
    events(out)
        .into_iter()
        .filter(|e| e.provider == "perplexity")
        .collect()
}

#[test]
fn zip_is_detected_without_any_perplexity_text_and_entries_become_events() {
    let t = tmp("px-basic");
    let z = zip(&t, "user_data_export_synthetic.zip", export(false));
    let out = t.join("out");
    import(&z, &out);
    let evs = px(&out);
    assert_eq!(
        evs.len(),
        3,
        "query+answer, plus the query of the failed entry (empty answer is not an event)"
    );
    assert_eq!(
        (evs[0].role, evs[1].role, evs[2].role),
        (Role::User, Role::Assistant, Role::User)
    );
    assert_eq!(evs[1].metadata["parent_native_id"], format!("{E1}:query"));
    assert_eq!(evs[0].metadata["perplexity_engine_mode"], "pro");
    assert_eq!(evs[2].metadata["perplexity_query_status"], "FAILED");
    assert_eq!(
        evs[0].timestamp_original.as_deref(),
        Some("2026-05-06T07:08:09.123456Z")
    );
    assert_eq!(
        evs[0].timestamp.as_deref(),
        Some("2026-05-06T07:08:09.123456000Z")
    );
    assert_eq!(
        inventory(&out, "user-data-20261006_102903-f205ff64.xlsx")[0],
        ("perplexity-export-workbook".into(), "unsupported".into())
    );
    assert_valid(&out);
}

#[test]
fn unknown_schema_is_not_claimed() {
    let t = tmp("px-unk");
    let z = t.join("e.zip");
    zip_of(
        &z,
        &[(
            "conversations-1.json",
            br#"{"conversations":[{"thread":"x","turns":[]}]}"#.to_vec(),
        )],
    );
    let out = t.join("out");
    import_raw(&z, &out);
    assert!(px(&out).is_empty());
    assert_eq!(
        inventory(&out, "conversations-1.json")[0].0,
        "web-export-unrecognized-schema"
    );
}

#[test]
fn malformed_conversation_is_counted_and_the_rest_imports() {
    let t = tmp("px-bad");
    let mut d = export(false);
    d["conversations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"context_uuid": "x", "context_title": "no entries"}));
    let z = zip(&t, "e.zip", d);
    let out = t.join("out");
    let log = import(&z, &out);
    assert_eq!(px(&out).len(), 3);
    assert!(log.contains("parse errors: 1"), "{log}");
}

#[test]
fn reimport_is_idempotent_and_a_newer_export_adds_only_new() {
    let t = tmp("px-idem");
    let out = t.join("out");
    let z1 = zip(&t, "a.zip", export(false));
    import(&z1, &out);
    let log = import(&z1, &out);
    assert!(log.contains("events: 0 new, 3 duplicate"), "{log}");
    let z2 = zip(&t, "b.zip", export(true));
    let log = import(&z2, &out);
    assert!(log.contains("events: 2 new, 3 duplicate"), "{log}");
    assert_valid(&out);
}

#[test]
fn provenance_names_the_zip_and_the_member() {
    let t = tmp("px-prov");
    let z = zip(&t, "user_data_export_synthetic.zip", export(false));
    let out = t.join("out");
    import(&z, &out);
    let e = &px(&out)[0];
    let p = provenance(&out, &e.event_id);
    assert!(
        p.contains("user_data_export_synthetic.zip!/conversations-20261006_102903-f205ff64.json"),
        "{p}"
    );
    assert!(p.contains("perplexity_export") && p.contains(E1), "{p}");
    assert!(e.machine_id.is_some());
}
