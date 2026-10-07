//! Gemini Takeout parser (`gemini_takeout`), synthetic content in the structure of a
//! real Takeout (verified: one user_turn OR system_turn per element, global turn_index,
//! repeated indices). The
//! the My Activity JSON shape is unverified-against-real-export.

mod webexport_common;
use convolith::model::{Event, Part, ReasoningVisibility, Role};
use serde_json::{json, Value};
use webexport_common::*;

const CREATED: &str = "2026-08-09T17:29:41.209311+00:00";

fn conversation(extra_turn: bool) -> Value {
    // Real shape: each array element holds ONE of user_turn / system_turn; turn_index
    // runs over both; the same index can repeat (edited/regenerated turns).
    let mut turns = vec![
        json!({"user_turn": {"prompt": "synthetic prompt", "turn_index": 0, "turn_last_modified": "2026-08-09T17:29:41.209311+00:00"}}),
        json!({"system_turn": {"text": [{"data": "synthetic answer"}, {"cards": [{"content": "synthetic card"}]}],
                        "images": ["conversation_1786296581_turn_1_images_0"],
                        "model_thoughts": [{"headline": "synthetic headline", "description": "synthetic description"}],
                        "turn_index": 1, "turn_last_modified": "2026-08-09T17:29:42+00:00",
                        "turn_deleted_time": "2026-08-10T00:00:00+00:00"}}),
    ];
    if extra_turn {
        turns.push(json!({"user_turn": {"prompt": "second prompt", "turn_index": 2, "turn_last_modified": "2026-08-09T17:34:00+00:00"}}));
        turns.push(json!({"system_turn": {"text": [{"data": "second answer"}], "turn_index": 3, "turn_last_modified": "2026-08-09T17:34:01+00:00"}}));
    }
    json!({"title": "synthetic gemini chat", "creation_time": CREATED,
           "last_modification_time": "2026-08-09T17:34:58.153871+00:00", "conversation_turns": turns})
}

const DIR: &str = "Takeout/Gemini in Workspace/Conversation History";

fn takeout(dir: &std::path::Path, name: &str, doc: Value) -> std::path::PathBuf {
    let z = dir.join(name);
    zip_of(
        &z,
        &[
            (
                &format!("{DIR}/conversation_1786296581.txt"),
                doc.to_string().into_bytes(),
            ),
            (
                &format!("{DIR}/conversation_1786296581_turn_0_images_0.jpg"),
                b"\xff\xd8synthetic".to_vec(),
            ),
            (
                "Takeout/Gemini/gemini_gems_data.html",
                b"<div></div>".to_vec(),
            ),
            ("Takeout/NotebookLM/nb/nb metadata.json", b"{}".to_vec()),
        ],
    );
    z
}

fn gem(out: &std::path::Path) -> Vec<Event> {
    events(out)
        .into_iter()
        .filter(|e| e.application == "gemini")
        .collect()
}

#[test]
fn takeout_zip_turns_become_user_and_assistant_events() {
    let t = tmp("gem-basic");
    let z = takeout(&t, "takeout.zip", conversation(false));
    let out = t.join("out");
    import(&z, &out);
    let evs = gem(&out);
    assert_eq!(evs.len(), 2);
    assert_eq!((evs[0].role, evs[1].role), (Role::User, Role::Assistant));
    let a = &evs[1];
    assert!(a.content.iter().any(|p| matches!(
        p,
        Part::Reasoning {
            visibility: ReasoningVisibility::Summary,
            ..
        }
    )));
    assert!(a
        .content
        .iter()
        .any(|p| matches!(p, Part::Text { text, .. } if text == "synthetic card")));
    assert!(a
        .content
        .iter()
        .any(|p| matches!(p, Part::Image { source_ref: Some(r), .. } if r.ends_with("images_0"))));
    assert_eq!(
        a.metadata.get("parent_native_id"),
        None,
        "no invented user->model link"
    );
    assert_eq!(
        a.metadata["gemini_turn_deleted_time"],
        "2026-08-10T00:00:00+00:00"
    );
    // sidecars and images are inventoried with reasons
    assert_eq!(
        inventory(&out, "images_0.jpg")[0],
        ("gemini-takeout-image".into(), "unsupported".into())
    );
    assert_eq!(
        inventory(&out, "gemini_gems_data.html")[0].0,
        "takeout-gemini-sidecar"
    );
    assert_eq!(inventory(&out, "NotebookLM")[0].0, "takeout-notebooklm");
    assert_valid(&out);
}

#[test]
fn timestamps_are_original_plus_utc_and_marked_last_modified() {
    let t = tmp("gem-time");
    let z = takeout(&t, "takeout.zip", conversation(false));
    let out = t.join("out");
    import(&z, &out);
    let u = &gem(&out)[0];
    assert_eq!(
        u.timestamp.as_deref(),
        Some("2026-08-09T17:29:41.209311000Z")
    );
    assert_eq!(
        u.timestamp_original.as_deref(),
        Some("2026-08-09T17:29:41.209311+00:00")
    );
    assert_eq!(u.metadata["gemini_timestamp_kind"], "turn_last_modified");
}

#[test]
fn malformed_turn_is_counted_and_the_rest_imports() {
    let t = tmp("gem-bad");
    let mut c = conversation(true);
    c["conversation_turns"][2] = json!({"unexpected": true});
    let z = takeout(&t, "takeout.zip", c);
    let out = t.join("out");
    let log = import(&z, &out);
    assert_eq!(gem(&out).len(), 3);
    assert!(log.contains("parse errors: 1"), "{log}");
}

#[test]
fn repeated_turn_indices_keep_distinct_events() {
    let t = tmp("gem-rep");
    let mut c = conversation(false);
    // a regenerated model turn: same role and turn_index as the first answer
    let again = json!({"system_turn": {"text": [{"data": "regenerated synthetic answer"}], "turn_index": 1,
        "turn_last_modified": "2026-08-09T17:30:00+00:00"}});
    c["conversation_turns"].as_array_mut().unwrap().push(again);
    let z = takeout(&t, "takeout.zip", c);
    let out = t.join("out");
    let log = import(&z, &out);
    assert!(!log.contains("conflict"), "{log}");
    let evs = gem(&out);
    assert_eq!(evs.len(), 3);
    let ids: std::collections::BTreeSet<_> = evs.iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids.len(), 3);
    assert!(
        evs.iter().all(|e| e.metadata.get("variant_of").is_none()),
        "no conflict variants"
    );
}

#[test]
fn truncated_document_fails_that_source_only() {
    let t = tmp("gem-trunc");
    let z = t.join("takeout.zip");
    zip_of(
        &z,
        &[
            (
                &format!("{DIR}/conversation_1.txt"),
                br#"{"title":"x","conversation_turns":[{"user_turn":"#.to_vec(),
            ),
            (
                &format!("{DIR}/conversation_2.txt"),
                conversation(false).to_string().into_bytes(),
            ),
        ],
    );
    let out = t.join("out");
    import_raw(&z, &out);
    assert_eq!(gem(&out).len(), 2);
    assert_eq!(inventory(&out, "conversation_1.txt")[0].1, "failed");
}

#[test]
fn unknown_version_my_activity_html_is_inventoried() {
    let t = tmp("gem-unk");
    let z = t.join("takeout.zip");
    zip_of(
        &z,
        &[
            (
                "Takeout/My Activity/Gemini Apps/MyActivity.html",
                b"<html></html>".to_vec(),
            ),
            (
                &format!("{DIR}/conversation_9.txt"),
                conversation(false).to_string().into_bytes(),
            ),
        ],
    );
    let out = t.join("out");
    import(&z, &out);
    assert_eq!(
        inventory(&out, "MyActivity.html")[0],
        ("gemini-myactivity-html".into(), "unsupported".into())
    );
}

#[test]
fn reimport_is_idempotent_and_a_newer_takeout_adds_only_new_turns() {
    let t = tmp("gem-idem");
    let out = t.join("out");
    let z1 = takeout(&t, "a.zip", conversation(false));
    import(&z1, &out);
    let log = import(&z1, &out);
    assert!(log.contains("events: 0 new, 2 duplicate"), "{log}");
    let z2 = takeout(&t, "b.zip", conversation(true));
    let log = import(&z2, &out);
    assert!(log.contains("events: 2 new, 2 duplicate"), "{log}");
    assert_valid(&out);
}

#[test]
fn provenance_names_the_zip_and_the_member() {
    let t = tmp("gem-prov");
    let z = takeout(&t, "takeout.zip", conversation(false));
    let out = t.join("out");
    import(&z, &out);
    let e = &gem(&out)[0];
    let p = provenance(&out, &e.event_id);
    assert!(p.contains("takeout.zip!/Takeout/Gemini in Workspace/Conversation History/conversation_1786296581.txt"), "{p}");
    assert!(e.machine_id.is_some());
}

#[test]
fn my_activity_json_prompts_become_conversations() {
    let t = tmp("gem-act");
    let acts = json!([
        {"header": "Gemini Apps", "title": "Prompted synthetic activity prompt", "time": "2026-03-04T05:06:07.890Z",
         "products": ["Gemini Apps"], "safeHtmlItem": [{"html": "<p>synthetic <b>html</b> answer</p>"}]},
        {"header": "Gemini Apps", "title": "Used Gemini Apps", "time": "2026-03-04T05:07:00Z"}]);
    let z = t.join("takeout.zip");
    zip_of(
        &z,
        &[(
            "Takeout/My Activity/Gemini Apps/MyActivity.json",
            acts.to_string().into_bytes(),
        )],
    );
    let out = t.join("out");
    import(&z, &out);
    let evs = gem(&out);
    assert_eq!(
        evs.len(),
        2,
        "prompt + response; non-prompt activity skipped"
    );
    assert!(evs[1]
        .content
        .iter()
        .any(|p| matches!(p, Part::Text { text, .. } if text == "synthetic html answer")));
    assert_eq!(
        evs[0].timestamp_original.as_deref(),
        Some("2026-03-04T05:06:07.890Z")
    );
    let log = import(&z, &out);
    assert!(log.contains("events: 0 new, 2 duplicate"), "{log}");
}
