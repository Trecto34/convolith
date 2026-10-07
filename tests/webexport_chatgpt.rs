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
    mapping[A2]["message"]["attachments"] = json!([{"id": "file-synthetic2", "name": "n.txt"}]);
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
    assert!(a2
        .content
        .iter()
        .any(|p| matches!(p, Part::FileRef { filename: Some(f), .. } if f == "n.txt")));
    // the image pointer stays an opaque part (v0.1.0-compatible fingerprint); the
    // documented metadata.attachments list is kept in event metadata
    assert!(a2.content.iter().any(|p| matches!(p, Part::Opaque { raw: Some(r), .. } if r["asset_pointer"] == "file-service://file-synthetic1")));
    assert_eq!(a2.metadata["chatgpt_attachments"][0]["name"], "notes.txt");
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

/// Structure of a real export: nodes are `{id, message, parent}` (no `children`), one
/// `conversations.json`, roles user/assistant only, `thoughts`/`reasoning_recap`
/// content without `parts`, `sediment://file_<h>` pointers with `file_<h>.dat` members,
/// and the account/index sidecars. Synthetic content only.
fn real_shape_zip(dir: &std::path::Path, name: &str, with_new_leaf: bool) -> std::path::PathBuf {
    let n = |i: u32| format!("5e55105e-0000-4000-8000-0000000001{i:02}");
    let msg = |i: u32, role: &str, t: f64, content: Value, meta: Value| json!({"id": n(i), "author": {"role": role, "name": null}, "create_time": t, "content": content, "metadata": meta});
    let mut mapping = json!({
        "client-created-root": {"id": "client-created-root", "message": null, "parent": null},
        n(1): {"id": n(1), "parent": "client-created-root", "message": msg(1, "user", 1.7e9, json!({"content_type": "multimodal_text", "parts": ["synthetic ask",
                {"content_type": "image_asset_pointer", "asset_pointer": "sediment://file_00000000aaaa", "size_bytes": 3, "width": 1, "height": 1, "fovea": 1, "metadata": {}}]}),
                json!({"attachments": [{"id": "file_00000000bbbb", "name": "n.txt", "mime_type": "text/plain", "size": 3}]}))},
        n(2): {"id": n(2), "parent": n(1), "message": msg(2, "assistant", 1.7e9 + 1.0, json!({"content_type": "thoughts", "thoughts": [{"summary": "s", "content": "c", "chunks": [], "finished": true}], "source_analysis_msg_id": "x"}), json!({"model_slug": "gpt-test"}))},
        n(3): {"id": n(3), "parent": n(2), "message": msg(3, "assistant", 1.7e9 + 2.0, json!({"content_type": "reasoning_recap", "content": "Thought for 2s"}), json!({"model_slug": "gpt-test"}))},
        n(4): {"id": n(4), "parent": n(3), "message": msg(4, "assistant", 1.7e9 + 3.0, json!({"content_type": "text", "parts": ["synthetic answer"]}), json!({"model_slug": "gpt-test"}))},
        // regeneration: a second answer under the same user message
        n(5): {"id": n(5), "parent": n(1), "message": msg(5, "assistant", 1.7e9 + 4.0, json!({"content_type": "text", "parts": ["regenerated answer"]}), json!({"model_slug": "gpt-test"}))},
    });
    let mut current = n(4);
    if with_new_leaf {
        mapping[n(6)] = json!({"id": n(6), "parent": n(4), "message": msg(6, "user", 1.7e9 + 5.0, json!({"content_type": "text", "parts": ["follow-up"]}), json!({}))});
        current = n(6);
    }
    let conv = json!([{"id": n(99), "conversation_id": n(99), "title": "t", "create_time": 1.7e9, "update_time": 1.7e9 + 9.0,
        "current_node": current, "default_model_slug": "gpt-test", "is_archived": false, "memory_scope": "global_enabled", "mapping": mapping}]);
    let z = dir.join(name);
    zip_of(
        &z,
        &[
            ("conversations.json", conv.to_string().into_bytes()),
            ("export_manifest.json", br#"{"export_files":[{"path":"conversations.json","size_bytes":1}],"logical_files":{}}"#.to_vec()),
            ("sites/export_manifest.json", br#"{"artifacts":[],"requested_at":"x"}"#.to_vec()),
            ("ads.json", br#"{"ads_profile":[],"ad_hides":[]}"#.to_vec()),
            ("user_settings.json", b"[{}]".to_vec()),
            ("library_files.json", br#"[{"file_id":"f","library_file_category":"c"}]"#.to_vec()),
            ("conversation_asset_file_names.json", br#"{"file_00000000aaaa.dat":"x.png"}"#.to_vec()),
            ("sectioned_conversations.json", b"[]".to_vec()),
            ("file_00000000aaaa.dat", b"\x89PNGsynthetic".to_vec()),
        ],
    );
    z
}

#[test]
fn real_shape_without_children_keeps_branches_thoughts_and_asset_members() {
    let t = tmp("gpt-real");
    let z = real_shape_zip(&t, "e.zip", false);
    let out = t.join("out");
    import(&z, &out);
    let evs = chat(&out);
    assert_eq!(evs.len(), 5);
    let by = |i: u32| {
        let id = format!("5e55105e-0000-4000-8000-0000000001{i:02}");
        evs.iter()
            .find(|e| e.metadata["native_id"] == id.as_str())
            .unwrap()
    };
    // branches come from `parent`; the regeneration is off the current path
    assert_eq!(
        by(5).metadata["parent_native_id"],
        by(2).metadata["parent_native_id"]
            .as_str()
            .map(|_| by(1).metadata["native_id"].clone())
            .unwrap()
    );
    assert_eq!(by(5).metadata["chatgpt_on_current_path"], false);
    assert_eq!(by(4).metadata["chatgpt_on_current_path"], true);
    assert!(
        by(4).metadata.get("chatgpt_children").is_none(),
        "absent in source, absent here"
    );
    // thoughts/recap have no parts: kept as metadata, not parts (v0.1.0 fingerprints)
    assert!(
        by(2).content.is_empty() && by(2).metadata["chatgpt_content"]["content_type"] == "thoughts"
    );
    assert!(by(3).content.is_empty());
    // image pointer and attachment id resolve to the shipped .dat member names
    assert_eq!(
        by(1).metadata["chatgpt_asset_members"],
        json!(["file_00000000bbbb.dat", "file_00000000aaaa.dat"])
    );
    // sidecars: reasons, not parsing
    for (needle, fmt) in [
        ("export_manifest.json", "chatgpt-export-manifest"),
        ("ads.json", "chatgpt-export-sidecar"),
        ("user_settings.json", "chatgpt-export-sidecar"),
        ("library_files.json", "chatgpt-export-sidecar"),
        (
            "conversation_asset_file_names.json",
            "chatgpt-export-sidecar",
        ),
        ("sectioned_conversations.json", "chatgpt-export-sidecar"),
        ("file_00000000aaaa.dat", "chatgpt-export-attachment"),
    ] {
        assert!(
            inventory(&out, needle)
                .iter()
                .all(|(f, s)| f == fmt && s == "unsupported"),
            "{needle}"
        );
    }
    assert_valid(&out);
}

#[test]
fn real_shape_reimport_is_idempotent_and_newer_adds_only_new() {
    let t = tmp("gpt-real-idem");
    let out = t.join("out");
    let log = import(&real_shape_zip(&t, "a.zip", false), &out);
    assert!(log.contains("events: 5 new, 0 duplicate"), "{log}");
    let log = import(&real_shape_zip(&t, "a.zip", false), &out);
    assert!(log.contains("events: 0 new, 5 duplicate"), "{log}");
    let log = import(&real_shape_zip(&t, "b.zip", true), &out);
    assert!(log.contains("events: 1 new, 5 duplicate"), "{log}");
    assert_valid(&out);
}
