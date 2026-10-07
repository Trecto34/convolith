//! Claude Design chats (`claude_design_export`): synthetic content in the structure of
//! the real `design_chats/<uuid>.json` files (verified).

mod webexport_common;
use convolith::model::{Event, Part, Role};
use serde_json::{json, Value};
use webexport_common::*;

const CHAT: &str = "3a1b0000-0000-4000-8000-0000000000c1";
const M1: &str = "3a1b0000-0000-4000-8000-0000000000b1";
const M2: &str = "3a1b0000-0000-4000-8000-0000000000b2";
const M3: &str = "3a1b0000-0000-4000-8000-0000000000b3";

fn chat(extra: bool) -> Value {
    let mut messages = vec![
        json!({"uuid": M1, "role": "user", "created_at": "2026-07-01T10:00:00.000000+00:00",
               "content": {"id": M1, "role": "user", "kind": "chat", "timestamp": "2026-07-01T10:00:00+00:00",
                           "content": "synthetic design request",
                           "attachments": [{"id": "a1", "name": "ref.png", "type": "image", "path": "uploads/ref.png"},
                                           {"id": "a2", "name": "notes", "type": "text", "content": "synthetic attachment text"}]}}),
        json!({"uuid": M2, "role": "assistant", "created_at": "2026-07-01T10:00:05.000000+00:00",
               "content": {"id": M2, "role": "assistant", "kind": "chat", "content": "synthetic reply",
                           "contentBlocks": [{"type": "thinking", "text": "synthetic thought"},
                                             {"type": "text", "text": "synthetic reply"},
                                             {"type": "tool_call", "toolCall": {"id": "tc1", "type": "edit", "name": "write_file", "input": {"path": "a.html"}, "output": "ok"}},
                                             {"type": "user_interjection", "message": {"id": "i1", "role": "user", "content": "x"}}],
                           "turnInputTokens": 12}}),
        json!({"uuid": M3, "role": "assistant", "created_at": "2026-07-01T10:00:06.000000+00:00",
               "content": {"id": M3, "role": "assistant", "kind": "question-record", "content": "",
                           "questionRecord": {"event": "asked", "questionId": "q1"}}}),
    ];
    if !extra {
        messages.pop();
    }
    json!({"uuid": CHAT, "title": "synthetic design chat", "project": {"uuid": "p1", "name": "synthetic project"},
           "created_at": "2026-07-01T10:00:00.000000+00:00", "updated_at": "2026-07-01T10:01:00.000000+00:00", "messages": messages})
}

fn zip(dir: &std::path::Path, name: &str, extra: bool) -> std::path::PathBuf {
    let z = dir.join(name);
    zip_of(
        &z,
        &[
            (&format!("design_chats/{CHAT}.json"), chat(extra).to_string().into_bytes()),
            ("design_chats/empty.json", json!({"uuid": "e", "title": "t", "project": {}, "created_at": "2026-07-01T10:00:00+00:00", "updated_at": "2026-07-01T10:00:00+00:00", "messages": []}).to_string().into_bytes()),
        ],
    );
    z
}

fn design(out: &std::path::Path) -> Vec<Event> {
    events(out)
        .into_iter()
        .filter(|e| e.application == "claude-design")
        .collect()
}

#[test]
fn design_chats_are_imported_with_blocks_attachments_and_kinds() {
    let t = tmp("cd-basic");
    let z = zip(&t, "design_chats-000.zip", true);
    let out = t.join("out");
    import(&z, &out);
    let evs = design(&out);
    assert_eq!(evs.len(), 3);
    assert_eq!((evs[0].role, evs[1].role), (Role::User, Role::Assistant));
    assert_eq!(evs[0].metadata["native_id"], M1);
    assert!(evs[0]
        .content
        .iter()
        .any(|p| matches!(p, Part::FileRef { filename: Some(f), .. } if f == "ref.png")));
    assert!(evs[0]
        .content
        .iter()
        .any(|p| matches!(p, Part::Text { text, .. } if text == "synthetic attachment text")));
    let a = &evs[1].content;
    assert!(a.iter().any(|p| matches!(p, Part::Reasoning { .. })));
    assert!(a
        .iter()
        .any(|p| matches!(p, Part::ToolCall { name, .. } if name == "write_file")));
    assert!(a.iter().any(|p| matches!(p, Part::ToolResult { .. })));
    assert!(a.iter().any(|p| matches!(p, Part::Opaque { kind, .. } if kind == "claude_design_block:user_interjection")));
    assert_eq!(evs[2].metadata["claude_design_kind"], "question-record");
    assert_eq!(
        evs[2].metadata["claude_design_questionRecord"]["questionId"],
        "q1"
    );
    assert_eq!(
        evs[0].timestamp_original.as_deref(),
        Some("2026-07-01T10:00:00.000000+00:00")
    );
    assert!(evs[0].timestamp.is_some());
    // an empty chat is skipped and counted, not an error
    assert_eq!(inventory(&out, "empty.json")[0].1, "parsed");
    assert_valid(&out);
}

#[test]
fn reimport_is_idempotent_and_newer_adds_only_new() {
    let t = tmp("cd-idem");
    let out = t.join("out");
    let z1 = zip(&t, "a.zip", false);
    import(&z1, &out);
    let log = import(&z1, &out);
    assert!(log.contains("events: 0 new, 2 duplicate"), "{log}");
    let log = import(&zip(&t, "b.zip", true), &out);
    assert!(log.contains("events: 1 new, 2 duplicate"), "{log}");
}

#[test]
fn provenance_names_the_zip_and_the_member() {
    let t = tmp("cd-prov");
    let z = zip(&t, "design_chats-000.zip", true);
    let out = t.join("out");
    import(&z, &out);
    let e = &design(&out)[0];
    let p = provenance(&out, &e.event_id);
    assert!(
        p.contains(&format!("design_chats-000.zip!/design_chats/{CHAT}.json")),
        "{p}"
    );
    assert!(e.machine_id.is_some());
}
