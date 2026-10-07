//! ChatGPT export parser (`chatgpt_export`), synthetic fixtures only.
//! Status of the parser: unverified-against-real-export.

mod webexport_common;
use convolith::model::{Event, Part};
use serde_json::{json, Value};
use webexport_common::*;

const CONV: &str = "5e55105e-0000-4000-8000-0000000000c1";
const U1: &str = "5e55105e-0000-4000-8000-0000000000a1";
const A1: &str = "5e55105e-0000-4000-8000-0000000000a2";
const A2: &str = "5e55105e-0000-4000-8000-0000000000a3";

fn msg(id: &str, role: &str, t: Option<f64>, parts: Value) -> Value {
    let mut m = json!({"id": id, "author": {"role": role}, "content": {"content_type": "text", "parts": parts},
        "metadata": {"model_slug": "gpt-test"}});
    if let Some(t) = t {
        m["create_time"] = json!(t);
    }
    m
}

/// root(no message) -> u1 -> {a1 (older answer), a2 (regeneration, current)}.
fn conversation(id: &str, extra_leaf: bool) -> Value {
    let mut a2_children = json!([]);
    let mut mapping = json!({
        "client-created-root": {"id": "client-created-root", "parent": null, "children": [U1], "message": null},
        U1: {"id": U1, "parent": "client-created-root", "children": [A1, A2],
             "message": msg(U1, "user", Some(1700000000.25), json!(["synthetic question"]))},
        A1: {"id": A1, "parent": U1, "children": [],
             "message": msg(A1, "assistant", Some(1700000001.0), json!(["first synthetic answer"]))},
        A2: {"id": A2, "parent": U1, "children": a2_children.clone(),
             "message": msg(A2, "assistant", Some(1700000002.0), json!(["regenerated synthetic answer",
                {"content_type": "image_asset_pointer", "asset_pointer": "file-service://file-synthetic1"}]))}
    });
    mapping[A2]["message"]["metadata"]["attachments"] = json!([{"id": "file-synthetic1", "name": "notes.txt", "mime_type": "text/plain", "size": 12}]);
    let mut current = A2;
    if extra_leaf {
        let a3 = "5e55105e-0000-4000-8000-0000000000a4";
        a2_children = json!([a3]);
        mapping[A2]["children"] = a2_children;
        mapping[a3] = json!({"id": a3, "parent": A2, "children": [],
            "message": msg(a3, "user", Some(1700000003.0), json!(["follow-up synthetic"]))});
        current = a3;
    }
    json!({"id": id, "conversation_id": id, "title": "synthetic chat", "create_time": 1700000000.0,
           "update_time": 1700000003.0, "current_node": current, "default_model_slug": "gpt-test", "mapping": mapping})
}

fn export_zip(dir: &std::path::Path, name: &str, convs: Vec<Value>) -> std::path::PathBuf {
    let z = dir.join(name);
    zip_of(
        &z,
        &[
            (
                "conversations.json",
                Value::Array(convs).to_string().into_bytes(),
            ),
            ("chat.html", b"<html>synthetic</html>".to_vec()),
            ("user.json", br#"{"id":"user-synthetic"}"#.to_vec()),
            ("message_feedback.json", b"[]".to_vec()),
            ("file-synthetic1-notes.txt", b"synthetic bytes".to_vec()),
        ],
    );
    z
}

fn chat(out: &std::path::Path) -> Vec<Event> {
    events(out)
        .into_iter()
        .filter(|e| e.provider == "openai")
        .collect()
}

#[test]
fn zip_is_detected_branches_and_ids_are_preserved() {
    let t = tmp("gpt-basic");
    let z = export_zip(&t, "chatgpt-export.zip", vec![conversation(CONV, false)]);
    let out = t.join("out");
    import(&z, &out);
    let evs = chat(&out);
    assert_eq!(
        evs.len(),
        3,
        "both regenerations are kept, not only the current path"
    );
    let by = |id: &str| evs.iter().find(|e| e.metadata["native_id"] == id).unwrap();
    assert_eq!(by(A1).metadata["chatgpt_on_current_path"], false);
    assert_eq!(by(A2).metadata["chatgpt_on_current_path"], true);
    // the empty root node is skipped; both answers hang off the user message
    assert_eq!(by(A1).metadata["parent_native_id"], U1);
    assert_eq!(by(A2).metadata["parent_native_id"], U1);
    assert_eq!(by(A2).model.as_deref(), Some("gpt-test"));
    assert_eq!(by(U1).role, convolith::model::Role::User);
    assert!(evs.iter().all(|e| e.application == "chatgpt"));
    assert_eq!(evs[0].metadata["native_id"], U1, "oldest first");
    // sidecars are inventoried with a reason, not parsed
    assert_eq!(
        inventory(&out, "chat.html"),
        [("chatgpt-export-sidecar".into(), "unsupported".into())]
    );
    assert_eq!(inventory(&out, "user.json")[0].0, "chatgpt-export-sidecar");
    assert_eq!(
        inventory(&out, "file-synthetic1-notes.txt")[0].0,
        "chatgpt-export-attachment"
    );
    assert_valid(&out);
}

#[test]
fn timestamps_keep_original_and_utc_and_never_invent() {
    let t = tmp("gpt-time");
    let mut c = conversation(CONV, false);
    c["mapping"][A1]["message"]
        .as_object_mut()
        .unwrap()
        .remove("create_time");
    let z = export_zip(&t, "e.zip", vec![c]);
    let out = t.join("out");
    import(&z, &out);
    let evs = chat(&out);
    let u1 = evs.iter().find(|e| e.metadata["native_id"] == U1).unwrap();
    assert_eq!(
        u1.timestamp.as_deref(),
        Some("2023-11-14T22:13:20.250000000Z")
    );
    assert_eq!(u1.timestamp_original.as_deref(), Some("1700000000.25"));
    let a1 = evs.iter().find(|e| e.metadata["native_id"] == A1).unwrap();
    assert_eq!(a1.timestamp, None);
    assert_eq!(
        a1.timestamp_confidence,
        convolith::timeutil::TimestampConfidence::Unknown
    );
}

#[test]
fn attachments_are_kept_as_references() {
    let t = tmp("gpt-att");
    let z = export_zip(&t, "e.zip", vec![conversation(CONV, false)]);
    let out = t.join("out");
    import(&z, &out);
    let a2 = chat(&out)
        .into_iter()
        .find(|e| e.metadata["native_id"] == A2)
        .unwrap();
    assert!(a2.content.iter().any(|p| matches!(p, Part::FileRef { filename: Some(f), mime: Some(m), .. } if f == "notes.txt" && m == "text/plain")));
    assert!(a2.content.iter().any(|p| matches!(p, Part::Image { source_ref: Some(r), .. } if r == "file-service://file-synthetic1")));
}

#[test]
fn malformed_conversation_is_counted_and_the_rest_imports() {
    let t = tmp("gpt-bad");
    let z = export_zip(
        &t,
        "e.zip",
        vec![
            conversation(CONV, false),
            json!({"id": "x", "title": "no mapping"}),
        ],
    );
    let out = t.join("out");
    let log = import(&z, &out);
    assert_eq!(chat(&out).len(), 3);
    assert!(log.contains("parse errors: 1"), "{log}");
    assert_eq!(
        inventory(&out, "conversations.json")[0].1,
        "partially_parsed"
    );
    assert_valid(&out);
}

#[test]
fn unknown_export_version_is_inventoried_not_guessed() {
    let t = tmp("gpt-unk");
    let z = t.join("e.zip");
    zip_of(
        &z,
        &[(
            "conversations.json",
            br#"[{"thread":"v9","turns":[{"who":"me"}]}]"#.to_vec(),
        )],
    );
    let out = t.join("out");
    let (_, log) = import_raw(&z, &out); // no events: validate rightly fails
    assert!(events(&out).is_empty(), "{log}");
    assert_eq!(
        inventory(&out, "conversations.json"),
        [(
            "web-export-unrecognized-schema".into(),
            "unsupported".into()
        )]
    );
}

#[test]
fn reimport_is_idempotent_and_a_newer_export_adds_only_new_events() {
    let t = tmp("gpt-idem");
    let out = t.join("out");
    let z1 = export_zip(&t, "old.zip", vec![conversation(CONV, false)]);
    let log = import(&z1, &out);
    assert_eq!(counts(&log, "events"), 3);
    let log = import(&z1, &out);
    assert!(log.contains("events: 0 new, 3 duplicate"), "{log}");
    // newer export of the same account: one more message in the same conversation
    let z2 = export_zip(&t, "new.zip", vec![conversation(CONV, true)]);
    let log = import(&z2, &out);
    assert!(log.contains("events: 1 new, 3 duplicate"), "{log}");
    assert_eq!(chat(&out).len(), 4);
    assert_valid(&out);
}

#[test]
fn provenance_names_the_zip_and_the_member() {
    let t = tmp("gpt-prov");
    let z = export_zip(&t, "chatgpt-export.zip", vec![conversation(CONV, false)]);
    let out = t.join("out");
    import(&z, &out);
    let e = &chat(&out)[0];
    let p = provenance(&out, &e.event_id);
    assert!(p.contains("chatgpt-export.zip!/conversations.json"), "{p}");
    assert!(p.contains("chatgpt_export"), "{p}");
    assert!(
        p.contains(e.metadata["native_id"].as_str().unwrap()),
        "record id: {p}"
    );
    assert!(e.machine_id.is_some());
}

#[test]
fn extracted_directory_imports_like_the_zip() {
    let t = tmp("gpt-dir");
    write(
        &t.join("export/conversations.json"),
        Value::Array(vec![conversation(CONV, false)]).to_string(),
    );
    write(&t.join("export/chat.html"), "<html></html>");
    let out = t.join("out");
    import(&t.join("export"), &out);
    assert_eq!(chat(&out).len(), 3);
    assert_eq!(inventory(&out, "chat.html")[0].0, "chatgpt-export-sidecar");
}

#[test]
fn sharded_conversation_files_are_read() {
    let t = tmp("gpt-shard");
    write(
        &t.join("export/conversations-000.json"),
        Value::Array(vec![conversation(CONV, false)]).to_string(),
    );
    let out = t.join("out");
    import(&t.join("export"), &out);
    assert_eq!(chat(&out).len(), 3);
}
