//! Claude (claude.ai) export parser (`claude_web_export`), synthetic content in the
//! structure of a real split data export (verified shape: parent_message_uuid on every
//! message with an all-zero root sentinel, content blocks incl. document/image/
//! injected_prompt_block/token_budget, files[{file_uuid,file_name}], manifest + parts).

mod webexport_common;
use convolith::model::{Event, Part, Role};
use serde_json::{json, Value};
use webexport_common::*;

const CONV: &str = "7c1a0000-0000-4000-8000-0000000000c1";
const M1: &str = "7c1a0000-0000-4000-8000-0000000000b1";
const M2: &str = "7c1a0000-0000-4000-8000-0000000000b2";
const ROOT: &str = "00000000-0000-4000-8000-000000000000";
const M4: &str = "7c1a0000-0000-4000-8000-0000000000b4";
const M3: &str = "7c1a0000-0000-4000-8000-0000000000b3";

fn conversation(id: &str, with_third: bool) -> Value {
    let mut msgs = vec![
        json!({"uuid": M1, "sender": "human", "text": "synthetic question", "created_at": "2026-01-02T03:04:05.123456Z",
               "updated_at": "2026-01-02T03:04:05.123456Z", "parent_message_uuid": ROOT,
               "content": [{"type": "text", "text": "synthetic question"}],
               "attachments": [{"file_name": "doc.txt", "file_type": "text/plain", "file_size": 20, "extracted_content": "synthetic attachment text"}],
               "files": [{"file_uuid": "f-1", "file_name": "pic.png"}]}),
        json!({"uuid": M2, "sender": "assistant", "text": "", "created_at": "2026-01-02T03:04:09.000000Z",
               "content": [{"type": "thinking", "thinking": "synthetic thought"},
                           {"type": "text", "text": "synthetic answer"},
                           {"type": "tool_use", "id": "toolu_synth", "name": "calc", "input": {"x": 1}},
                           {"type": "mystery", "v": 1},
                           {"type": "token_budget"}, {"type": "document", "source": null}],
               "parent_message_uuid": M1}),
    ];
    if with_third {
        msgs.push(json!({"uuid": M3, "sender": "human", "text": "follow-up synthetic", "created_at": "2026-01-02T03:05:00Z",
                         "parent_message_uuid": M2}));
        // a retry of the first answer: same parent as M2 (a real branch)
        msgs.push(json!({"uuid": M4, "sender": "assistant", "text": "retry synthetic", "created_at": "2026-01-02T03:06:00Z",
                         "parent_message_uuid": M1}));
    }
    json!({"uuid": id, "name": "synthetic claude chat", "created_at": "2026-01-02T03:04:00Z",
           "updated_at": "2026-01-02T03:05:00Z", "account": {"uuid": "acc-synthetic"}, "chat_messages": msgs})
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
            ("users.json", br#"[{"uuid":"u"}]"#.to_vec()),
            (
                "manifest-synthetic.json",
                br#"{"version":"1.0","data_files":[]}"#.to_vec(),
            ),
            ("memories/m.json", br#"{"memory_files":[]}"#.to_vec()),
            (
                "projects/p.json",
                br#"{"uuid":"p","prompt_template":"","docs":[]}"#.to_vec(),
            ),
            (
                "reflections/r.json",
                br#"{"reflections":[],"feedback":[]}"#.to_vec(),
            ),
            (
                "artifacts/a/artifact.json",
                br#"{"id":"a","active_version":"v"}"#.to_vec(),
            ),
            ("artifacts/a/v1.html", b"<html></html>".to_vec()),
            ("login_history.json", br#"{"login_events":[]}"#.to_vec()),
        ],
    );
    z
}

fn claude(out: &std::path::Path) -> Vec<Event> {
    events(out)
        .into_iter()
        .filter(|e| e.application == "claude")
        .collect()
}

#[test]
fn zip_is_detected_and_roles_blocks_ids_are_preserved() {
    let t = tmp("cl-basic");
    let z = export_zip(&t, "claude-export.zip", vec![conversation(CONV, false)]);
    let out = t.join("out");
    import(&z, &out);
    let evs = claude(&out);
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].role, Role::User);
    assert_eq!(evs[1].role, Role::Assistant);
    assert_eq!(evs[0].metadata["native_id"], M1);
    assert_eq!(evs[0].provider, "anthropic");
    let a = &evs[1].content;
    assert!(a
        .iter()
        .any(|p| matches!(p, Part::Reasoning { text, .. } if text == "synthetic thought")));
    assert!(a
        .iter()
        .any(|p| matches!(p, Part::ToolCall { name, .. } if name == "calc")));
    assert!(a
        .iter()
        .any(|p| matches!(p, Part::Opaque { kind, .. } if kind == "claude_block:mystery")));
    assert_eq!(
        evs[0].metadata.get("parent_native_id"),
        None,
        "the zero root sentinel is no parent"
    );
    assert_eq!(evs[1].metadata["parent_native_id"], M1);
    assert!(a
        .iter()
        .any(|p| matches!(p, Part::Opaque { kind, .. } if kind == "claude_block:token_budget")));
    for (needle, fmt) in [
        ("users.json", "claude-export-sidecar"),
        ("manifest-synthetic.json", "claude-export-manifest"),
        ("memories/m.json", "claude-export-memories"),
        ("projects/p.json", "claude-export-projects"),
        ("reflections/r.json", "claude-export-feedback"),
        ("artifact.json", "claude-export-frames"),
        ("v1.html", "claude-export-frames"),
        ("login_history.json", "claude-export-metadata"),
    ] {
        assert_eq!(
            inventory(&out, needle),
            [(fmt.into(), "unsupported".into())],
            "{needle}"
        );
    }
    assert_valid(&out);
}

#[test]
fn branches_share_a_parent() {
    let t = tmp("cl-parent");
    let z = export_zip(&t, "e.zip", vec![conversation(CONV, true)]);
    let out = t.join("out");
    import(&z, &out);
    let third = claude(&out)
        .into_iter()
        .find(|e| e.metadata["native_id"] == M3)
        .unwrap();
    assert_eq!(third.metadata["parent_native_id"], M2);
}

#[test]
fn timestamps_keep_original_and_utc() {
    let t = tmp("cl-time");
    let mut c = conversation(CONV, false);
    c["chat_messages"][1]
        .as_object_mut()
        .unwrap()
        .remove("created_at");
    let z = export_zip(&t, "e.zip", vec![c]);
    let out = t.join("out");
    import(&z, &out);
    let evs = claude(&out);
    assert_eq!(
        evs[0].timestamp.as_deref(),
        Some("2026-01-02T03:04:05.123456000Z")
    );
    assert_eq!(
        evs[0].timestamp_original.as_deref(),
        Some("2026-01-02T03:04:05.123456Z")
    );
    assert_eq!(evs[1].timestamp, None, "a missing time is never invented");
}

#[test]
fn attachments_are_references_plus_extracted_text() {
    let t = tmp("cl-att");
    let z = export_zip(&t, "e.zip", vec![conversation(CONV, false)]);
    let out = t.join("out");
    import(&z, &out);
    let u = &claude(&out)[0];
    assert!(u.content.iter().any(|p| matches!(p, Part::FileRef { filename: Some(f), mime: Some(m), .. } if f == "doc.txt" && m == "text/plain")));
    assert!(u
        .content
        .iter()
        .any(|p| matches!(p, Part::FileRef { filename: Some(f), .. } if f == "pic.png")));
    assert!(u.metadata["claude_attachments"].as_array().unwrap().len() == 2);
}

#[test]
fn malformed_conversation_is_counted_and_the_rest_imports() {
    let t = tmp("cl-bad");
    // second record has chat_messages of the wrong type
    let bad = json!({"uuid": "x", "name": "bad", "chat_messages": "nope"});
    let z = export_zip(&t, "e.zip", vec![conversation(CONV, false), bad]);
    let out = t.join("out");
    let log = import(&z, &out);
    assert_eq!(claude(&out).len(), 2);
    assert!(log.contains("parse errors: 1"), "{log}");
}

#[test]
fn unknown_schema_is_inventoried_not_guessed() {
    let t = tmp("cl-unk");
    let z = t.join("e.zip");
    zip_of(
        &z,
        &[(
            "conversations.json",
            br#"[{"uuid":"u","name":"n","messages_v9":[]}]"#.to_vec(),
        )],
    );
    let out = t.join("out");
    import_raw(&z, &out);
    assert!(events(&out).is_empty());
    assert_eq!(
        inventory(&out, "conversations.json")[0].0,
        "web-export-unrecognized-schema"
    );
}

#[test]
fn reimport_is_idempotent_and_a_newer_export_adds_only_new() {
    let t = tmp("cl-idem");
    let out = t.join("out");
    let z1 = export_zip(&t, "a.zip", vec![conversation(CONV, false)]);
    import(&z1, &out);
    let log = import(&z1, &out);
    assert!(log.contains("events: 0 new, 2 duplicate"), "{log}");
    let z2 = export_zip(&t, "b.zip", vec![conversation(CONV, true)]);
    let log = import(&z2, &out);
    assert!(log.contains("events: 2 new, 2 duplicate"), "{log}");
    assert_valid(&out);
}

#[test]
fn provenance_names_the_zip_and_the_member() {
    let t = tmp("cl-prov");
    let z = export_zip(&t, "claude-export.zip", vec![conversation(CONV, false)]);
    let out = t.join("out");
    import(&z, &out);
    let e = &claude(&out)[0];
    let p = provenance(&out, &e.event_id);
    assert!(p.contains("claude-export.zip!/conversations.json"), "{p}");
    assert!(p.contains("claude_web_export") && p.contains(M1), "{p}");
    assert!(e.machine_id.is_some());
}
