//! Perplexity export parser (`perplexity_export`), synthetic fixtures only.
//! Status: unverified-against-real-export (no documented bulk schema exists).

mod webexport_common;
use convolith::model::{Event, Part, Role};
use serde_json::{json, Value};
use webexport_common::*;

const T1: &str = "9d2b0000-0000-4000-8000-0000000000d1";
const E1: &str = "9d2b0000-0000-4000-8000-0000000000e1";
const E2: &str = "9d2b0000-0000-4000-8000-0000000000e2";

fn export(version: Value, two: bool) -> Value {
    let mut entries = vec![
        json!({"id": E1, "query": "synthetic question", "answer": "synthetic answer",
        "created_at": "2026-05-06T07:08:09Z", "model": "sonar-test",
        "sources": [{"url": "https://example.invalid/a", "title": "A"}]}),
    ];
    if two {
        entries.push(json!({"id": E2, "query": "second question", "answer": "second answer", "created_at": "2026-05-06T07:09:00Z"}));
    }
    json!({"version": version, "threads": [{"id": T1, "title": "synthetic thread", "created_at": "2026-05-06T07:08:00Z", "entries": entries}]})
}

fn zip(dir: &std::path::Path, name: &str, doc: Value) -> std::path::PathBuf {
    let z = dir.join(name);
    zip_of(
        &z,
        &[
            ("threads.json", doc.to_string().into_bytes()),
            ("thread-1.md", b"# synthetic\n".to_vec()),
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
fn zip_is_detected_and_entries_become_query_and_answer() {
    let t = tmp("px-basic");
    let z = zip(&t, "perplexity-export.zip", export(json!(1), false));
    let out = t.join("out");
    import(&z, &out);
    let evs = px(&out);
    assert_eq!(evs.len(), 2);
    assert_eq!((evs[0].role, evs[1].role), (Role::User, Role::Assistant));
    assert_eq!(evs[1].model.as_deref(), Some("sonar-test"));
    assert!(evs[1].content.iter().any(|p| matches!(p, Part::Opaque { kind, note: Some(n), .. } if kind == "perplexity_source" && n.contains("example.invalid"))));
    assert_eq!(evs[1].metadata["parent_native_id"], format!("{E1}:query"));
    assert_eq!(
        evs[0].timestamp_original.as_deref(),
        Some("2026-05-06T07:08:09Z")
    );
    assert_eq!(
        inventory(&out, "thread-1.md")[0].0,
        "perplexity-thread-export"
    );
    assert_valid(&out);
}

#[test]
fn unknown_version_is_inventoried_not_guessed() {
    let t = tmp("px-ver");
    let z = zip(&t, "perplexity-export.zip", export(json!(7), false));
    let out = t.join("out");
    import_raw(&z, &out);
    assert!(px(&out).is_empty());
    assert_eq!(
        inventory(&out, "threads.json")[0],
        (
            "perplexity-export-unknown-version".into(),
            "unsupported".into()
        )
    );
}

#[test]
fn malformed_thread_is_counted_and_the_rest_imports() {
    let t = tmp("px-bad");
    let mut d = export(json!(1), false);
    d["threads"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "x", "title": "no entries"}));
    let z = zip(&t, "perplexity-export.zip", d);
    let out = t.join("out");
    let log = import(&z, &out);
    assert_eq!(px(&out).len(), 2);
    assert!(log.contains("parse errors: 1"), "{log}");
}

#[test]
fn unrelated_threads_json_is_not_claimed() {
    let t = tmp("px-other");
    let z = t.join("other.zip");
    zip_of(
        &z,
        &[(
            "threads.json",
            export(json!(1), false).to_string().into_bytes(),
        )],
    );
    let out = t.join("out");
    import_raw(&z, &out);
    assert!(
        px(&out).is_empty(),
        "no Perplexity evidence in path or head"
    );
}

#[test]
fn reimport_is_idempotent_and_a_newer_export_adds_only_new() {
    let t = tmp("px-idem");
    let out = t.join("out");
    let z1 = zip(&t, "perplexity-a.zip", export(json!(1), false));
    import(&z1, &out);
    let log = import(&z1, &out);
    assert!(log.contains("events: 0 new, 2 duplicate"), "{log}");
    let z2 = zip(&t, "perplexity-b.zip", export(json!(1), true));
    let log = import(&z2, &out);
    assert!(log.contains("events: 2 new, 2 duplicate"), "{log}");
    assert_valid(&out);
}

#[test]
fn provenance_names_the_zip_and_the_member() {
    let t = tmp("px-prov");
    let z = zip(&t, "perplexity-export.zip", export(json!(1), false));
    let out = t.join("out");
    import(&z, &out);
    let p = provenance(&out, &px(&out)[0].event_id);
    assert!(p.contains("perplexity-export.zip!/threads.json"), "{p}");
    assert!(px(&out)[0].machine_id.is_some());
}
