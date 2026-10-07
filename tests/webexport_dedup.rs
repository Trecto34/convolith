//! Local-vs-web identity and zip safety. Dedup needs strong identity (the same
//! provider ids under the same provider+application); text alone never merges.

mod webexport_common;
use serde_json::{json, Value};
use webexport_common::*;

const CONV: &str = "5e55105e-0000-4000-8000-0000000000f1";
const MSG: &str = "5e55105e-0000-4000-8000-0000000000f2";

fn gpt(conv: &str, msg: &str) -> Value {
    json!([{"id": conv, "title": "t", "current_node": msg, "mapping": {
        msg: {"id": msg, "parent": null, "children": [], "message": {"id": msg, "author": {"role": "user"},
              "create_time": 1700000000.0, "content": {"content_type": "text", "parts": ["identical synthetic text"]}}}}}])
}

#[test]
fn same_provider_ids_from_a_local_copy_and_a_web_zip_dedup() {
    let t = tmp("dd-ids");
    let out = t.join("out");
    write(
        &t.join("local/conversations.json"),
        gpt(CONV, MSG).to_string(),
    );
    import(&t.join("local"), &out);
    let z = t.join("web.zip");
    zip_of(
        &z,
        &[(
            "conversations.json",
            gpt(CONV, MSG).to_string().into_bytes(),
        )],
    );
    let log = import(&z, &out);
    assert!(log.contains("events: 0 new, 1 duplicate"), "{log}");
    assert_eq!(events(&out).len(), 1);
    // both observations are kept as provenance of the one event
    // Provenance keeps the original platform path; normalize separators for
    // this cross-platform assertion without changing the stored value.
    let p = provenance(&out, &events(&out)[0].event_id).replace('\\', "/");
    assert!(
        p.contains("local/conversations.json") && p.contains("web.zip!/conversations.json"),
        "{p}"
    );
}

#[test]
fn identical_text_with_different_ids_is_never_merged() {
    let t = tmp("dd-text");
    let out = t.join("out");
    write(
        &t.join("in/a/conversations.json"),
        gpt(CONV, MSG).to_string(),
    );
    write(
        &t.join("in/b/conversations.json"),
        gpt(
            "5e55105e-0000-4000-8000-0000000000f3",
            "5e55105e-0000-4000-8000-0000000000f4",
        )
        .to_string(),
    );
    // and a local generic store carrying the very same text, no ids at all
    write(
        &t.join("in/c/chat.jsonl"),
        "{\"role\":\"user\",\"content\":\"identical synthetic text\"}\n",
    );
    import(&t.join("in"), &out);
    assert_eq!(
        events(&out).len(),
        3,
        "text-only similarity must keep every event"
    );
}

#[test]
fn claude_code_and_claude_web_with_same_text_stay_separate() {
    let t = tmp("dd-claude");
    let out = t.join("out");
    // "Hello Claude" is the text of the first message in the Claude Code fixture.
    let web = json!([{"uuid": CONV, "name": "n", "chat_messages": [
        {"uuid": MSG, "sender": "human", "text": "Hello Claude", "created_at": "2026-03-01T10:00:01Z"}]}]);
    write(&t.join("in/export/conversations.json"), web.to_string());
    std::fs::create_dir_all(t.join("in/.claude/projects/p")).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/claude-code/simple.jsonl"
        ),
        t.join("in/.claude/projects/p/simple.jsonl"),
    )
    .unwrap();
    import(&t.join("in"), &out);
    let evs = events(&out);
    let web_ev = evs.iter().filter(|e| e.application == "claude").count();
    let local = evs
        .iter()
        .filter(|e| e.application == "claude-code")
        .count();
    assert_eq!(web_ev, 1);
    assert!(
        local >= 1,
        "the local session must also be imported: {} events",
        evs.len()
    );
    assert_eq!(
        evs.len(),
        web_ev + local,
        "same text, different identity: nothing collapsed"
    );
}

#[test]
fn path_traversal_members_are_rejected_and_never_imported() {
    let t = tmp("dd-trav");
    let z = t.join("evil.zip");
    zip_of(
        &z,
        &[
            (
                traversal_name(),
                gpt(
                    "5e55105e-0000-4000-8000-0000000000e1",
                    "5e55105e-0000-4000-8000-0000000000e2",
                )
                .to_string()
                .into_bytes(),
            ),
            (
                "conversations.json",
                gpt(CONV, MSG).to_string().into_bytes(),
            ),
        ],
    );
    let out = t.join("out");
    import(&z, &out);
    assert_eq!(events(&out).len(), 1, "only the safe member is imported");
    assert!(!t.join("escape").exists() && !t.parent().unwrap().join("escape").exists());
    assert_valid(&out);
}
