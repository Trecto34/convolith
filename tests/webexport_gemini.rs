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

fn make_synthetic_takeout_card(
    app_id: Option<&str>,
    prompt_prefix: &str,
    prompt_text: &str,
    timestamp: &str,
    asst_html: &str,
    attachment_ref: Option<&str>,
) -> String {
    let mut att_block = String::new();
    let mut right_cell = String::new();
    if let Some(att) = attachment_ref {
        att_block = format!("<br>1 attachment.<br>- <a href=\"{att}\">{att}</a>");
        right_cell = format!("<img src=\"{att}\" class=\"image-preview\">");
    }
    let details = match app_id {
        Some(id) => format!(
            "<b>Products:</b><br>Gemini Apps<br><b>Details:</b><br>From: <a href=\"https://gemini.google.com/app/{id}\">https://gemini.google.com/app/{id}</a><br><b>Why is this here?</b><br>..."
        ),
        None => "<b>Products:</b><br>Gemini Apps<br><b>Details:</b><br>From Google<br><b>Why is this here?</b><br>...".to_string(),
    };

    format!(
        r#"<div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
  <div class="mdl-grid">
    <div class="header-cell mdl-cell mdl-cell--12-col">
      <p class="mdl-typography--title">Gemini Apps<br></p>
    </div>
    <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">
      {prompt_prefix}{prompt_text}{att_block}<br>
      {timestamp}<br>
      {asst_html}
    </div>
    <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1 mdl-typography--text-right">
      {right_cell}
    </div>
    <div class="content-cell mdl-cell mdl-cell--12-col mdl-typography--caption">
      {details}
    </div>
  </div>
</div>"#
    )
}

fn make_synthetic_system_action_card(action_text: &str, timestamp: &str) -> String {
    format!(
        r#"<div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
  <div class="mdl-grid">
    <div class="header-cell mdl-cell mdl-cell--12-col">
      <p class="mdl-typography--title">Gemini Apps<br></p>
    </div>
    <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">
      {action_text}<br>
      {timestamp}<br>
    </div>
    <div class="content-cell mdl-cell mdl-cell--12-col mdl-typography--caption">
      <b>Products:</b><br>Gemini Apps<br>
    </div>
  </div>
</div>"#
    )
}

#[test]
fn takeout_html_myactivity_multi_turn_chronology_and_markdown() {
    let t = tmp("gem-html-full");
    let z = t.join("google-takeout.zip");

    let card_sys = make_synthetic_system_action_card(
        "Cleared Gemini Apps history",
        "Oct 6, 2026, 12:30:00\u{202f}PM GMT-03:00",
    );
    // Turn 2 of conv_alpha (newer: 12:00:00, appears first in HTML because Takeout is reverse chronological)
    let card_conv1_turn2 = make_synthetic_takeout_card(
        Some("conv_alpha"),
        "Prompted ",
        "second question in the thread",
        "Oct 6, 2026, 12:00:00\u{202f}PM GMT-03:00",
        "<p>Second answer from assistant.</p>",
        Some("photo.jpeg"),
    );
    // Turn 1 of conv_alpha (older: 11:00:00, appears second in HTML)
    let card_conv1_turn1 = make_synthetic_takeout_card(
        Some("conv_alpha"),
        "Prompted ",
        "first question with attachment",
        "Oct 6, 2026, 11:00:00\u{202f}AM GMT-03:00",
        "<h3>Architecture</h3><p>Here is <b>formatted</b> text with <i>italics</i> and a <a href=\"https://example.com\">link</a>.</p><ul><li>First item</li><li>Second item</li></ul><ol><li>Step one</li><li>Step two</li></ol><table><tr><th>Name</th><th>Role</th></tr><tr><td>Alice</td><td>Admin</td></tr></table><pre><code>let x = 42;</code></pre><blockquote>Note this quote</blockquote>",
        Some("diagram.png"),
    );
    // Portuguese card without app id
    let card_conv2 = make_synthetic_takeout_card(
        None,
        "Fez uma pergunta: ",
        "Como funciona a fotossíntese?",
        "6 de out. de 2026, 10:15:30 BRT",
        "<p>A fotossíntese é o processo biológico...</p>",
        None,
    );
    // Duplicate of Turn 2 to verify dedup within HTML
    let card_conv1_turn2_dup = card_conv1_turn2.clone();

    let full_html = format!(
        "<html><head><style>.some-css {{}}</style></head><body><div class=\"mdl-grid\">{card_sys}\n{card_conv1_turn2}\n{card_conv1_turn1}\n{card_conv2}\n{card_conv1_turn2_dup}</div></body></html>"
    );

    zip_of(
        &z,
        &[
            (
                "Takeout/My Activity/Gemini Apps/MyActivity.html",
                full_html.into_bytes(),
            ),
            (
                "Takeout/My Activity/Gemini Apps/diagram.png",
                b"\x89PNG\r\n\x1a\nsynthetic_diagram".to_vec(),
            ),
            (
                "Takeout/My Activity/Gemini Apps/photo.jpg",
                b"\xff\xd8synthetic_jpg".to_vec(),
            ),
        ],
    );

    let out = t.join("out");
    let log = import(&z, &out);
    assert_valid(&out);
    assert!(log.contains("events: 6 new, 0 duplicate"), "{log}");

    let evs = gem(&out);
    assert_eq!(
        evs.len(),
        6,
        "2 turns in conv1 (4 evs) + 1 turn in conv2 (2 evs)"
    );

    // Find conversation alpha events
    let c1_evs: Vec<_> = evs
        .iter()
        .filter(|e| {
            e.metadata
                .get("native_id")
                .and_then(Value::as_str)
                .map(|s| s.contains("conv_alpha"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(c1_evs.len(), 4);

    // Verify chronological order (Turn 1 before Turn 2)
    assert_eq!(c1_evs[0].role, Role::User);
    assert_eq!(c1_evs[0].seq, 0);
    assert_eq!(
        c1_evs[0].timestamp_original.as_deref(),
        Some("Oct 6, 2026, 11:00:00\u{202f}AM GMT-03:00")
    );
    assert_eq!(c1_evs[0].timestamp.as_deref(), Some("2026-10-06T14:00:00Z"));
    // User content has prompt text without boilerplate
    let u1_text = c1_evs[0]
        .content
        .iter()
        .find_map(|p| match p {
            Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(u1_text, "first question with attachment");
    assert!(!u1_text.contains("1 attachment"));
    assert!(!u1_text.contains("diagram.png"));

    // User content has attachment
    assert!(c1_evs[0].content.iter().any(|p| matches!(
        p,
        Part::Image { filename: Some(fname), source_ref: Some(sref), .. }
            if fname == "diagram.png" && sref == "Takeout/My Activity/Gemini Apps/diagram.png"
    )));

    // Assistant Turn 1
    assert_eq!(c1_evs[1].role, Role::Assistant);
    assert_eq!(c1_evs[1].seq, 1);
    assert_eq!(
        c1_evs[1].timestamp, None,
        "no invented wall-clock timestamp"
    );
    assert_eq!(
        c1_evs[1]
            .metadata
            .get("parent_native_id")
            .and_then(Value::as_str),
        Some("gemini-app:conv_alpha:turn:0:user")
    );
    let a1_text = c1_evs[1]
        .content
        .iter()
        .find_map(|p| match p {
            Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(a1_text.contains("### Architecture"), "{a1_text}");
    assert!(a1_text.contains("**formatted**"), "{a1_text}");
    assert!(a1_text.contains("*italics*"), "{a1_text}");
    assert!(a1_text.contains("[link](https://example.com)"), "{a1_text}");
    assert!(a1_text.contains("- First item"), "{a1_text}");
    assert!(a1_text.contains("1. Step one"), "{a1_text}");
    assert!(a1_text.contains("| Name | Role |"), "{a1_text}");
    assert!(a1_text.contains("```\nlet x = 42;\n```"), "{a1_text}");
    assert!(a1_text.contains("> Note this quote"), "{a1_text}");

    // Turn 2 User
    assert_eq!(c1_evs[2].role, Role::User);
    assert_eq!(c1_evs[2].seq, 2);
    assert_eq!(
        c1_evs[2].timestamp_original.as_deref(),
        Some("Oct 6, 2026, 12:00:00\u{202f}PM GMT-03:00")
    );
    assert_eq!(c1_evs[2].timestamp.as_deref(), Some("2026-10-06T15:00:00Z"));
    assert_eq!(
        c1_evs[2]
            .metadata
            .get("parent_native_id")
            .and_then(Value::as_str),
        Some("gemini-app:conv_alpha:turn:0:model")
    );

    // Turn 2 User attachment normalized from photo.jpeg to photo.jpg
    assert!(c1_evs[2].content.iter().any(|p| matches!(
        p,
        Part::Image { filename: Some(fname), source_ref: Some(sref), .. }
            if fname == "photo.jpg" && sref == "Takeout/My Activity/Gemini Apps/photo.jpg"
    )));

    // Turn 2 Assistant
    assert_eq!(c1_evs[3].role, Role::Assistant);
    assert_eq!(c1_evs[3].seq, 3);
    assert_eq!(c1_evs[3].timestamp, None);
    assert_eq!(
        c1_evs[3]
            .metadata
            .get("parent_native_id")
            .and_then(Value::as_str),
        Some("gemini-app:conv_alpha:turn:1:user")
    );

    // Portuguese conversation without app id
    let c2_evs: Vec<_> = evs
        .iter()
        .filter(|e| {
            e.metadata
                .get("native_id")
                .and_then(Value::as_str)
                .map(|s| !s.contains("conv_alpha"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(c2_evs.len(), 2);
    assert_eq!(c2_evs[0].role, Role::User);
    assert_eq!(c2_evs[0].timestamp.as_deref(), Some("2026-10-06T13:15:30Z"));
    let u2_text = c2_evs[0]
        .content
        .iter()
        .find_map(|p| match p {
            Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(u2_text, "Como funciona a fotossíntese?");

    // Attachment inventoried
    assert_eq!(
        inventory(&out, "diagram.png")[0],
        ("gemini-activity-attachment".into(), "unsupported".into())
    );
    assert_eq!(
        inventory(&out, "photo.jpg")[0],
        ("gemini-activity-attachment".into(), "unsupported".into())
    );

    // Re-import idempotency
    let re_log = import(&z, &out);
    assert!(re_log.contains("events: 0 new, 6 duplicate"), "{re_log}");
    assert_valid(&out);

    // Verify convolith all and convolith all --harness preserve exact user -> assistant turn order
    let (code1, all_out) = cli(&["all", out.to_str().unwrap(), "--stdout"]);
    assert_eq!(code1, 0, "{all_out}");
    assert!(all_out.contains("\"provider\":\"google\""), "{all_out}");
    assert!(all_out.contains("\"application\":\"gemini\""), "{all_out}");

    let target_cid = &c1_evs[0].conversation_id;
    let all_c1: Vec<Value> = all_out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e.get("conversation_id").and_then(Value::as_str) == Some(target_cid))
        .collect();
    assert_eq!(all_c1.len(), 4);
    assert_eq!(
        (all_c1[0]["seq"].as_u64(), all_c1[0]["role"].as_str()),
        (Some(0), Some("user"))
    );
    assert_eq!(
        (all_c1[1]["seq"].as_u64(), all_c1[1]["role"].as_str()),
        (Some(1), Some("assistant"))
    );
    assert_eq!(
        (all_c1[2]["seq"].as_u64(), all_c1[2]["role"].as_str()),
        (Some(2), Some("user"))
    );
    assert_eq!(
        (all_c1[3]["seq"].as_u64(), all_c1[3]["role"].as_str()),
        (Some(3), Some("assistant"))
    );

    let (code2, harness_out) = cli(&["all", "--harness", out.to_str().unwrap(), "--stdout"]);
    assert_eq!(code2, 0, "{harness_out}");
    assert!(
        harness_out.contains("\"provider\":\"google\""),
        "{harness_out}"
    );
    assert!(
        harness_out.contains("\"application\":\"gemini\""),
        "{harness_out}"
    );

    let h_c1: Vec<Value> = harness_out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| {
            e.get("schema") == Some(&json!("convolith.harness/v1"))
                && e.get("conversation_id").and_then(Value::as_str) == Some(target_cid)
        })
        .collect();
    assert_eq!(h_c1.len(), 4);
    assert_eq!(
        (h_c1[0]["seq"].as_u64(), h_c1[0]["role"].as_str()),
        (Some(0), Some("user"))
    );
    assert_eq!(
        (h_c1[1]["seq"].as_u64(), h_c1[1]["role"].as_str()),
        (Some(1), Some("assistant"))
    );
    assert_eq!(
        (h_c1[2]["seq"].as_u64(), h_c1[2]["role"].as_str()),
        (Some(2), Some("user"))
    );
    assert_eq!(
        (h_c1[3]["seq"].as_u64(), h_c1[3]["role"].as_str()),
        (Some(3), Some("assistant"))
    );
}

#[test]
fn takeout_html_empty_or_no_turns_is_valid() {
    let t = tmp("gem-html-empty");
    let z = t.join("takeout.zip");
    zip_of(
        &z,
        &[
            (
                "Takeout/My Activity/Gemini Apps/MyActivity.html",
                b"<html><body><div class=\"mdl-grid\"></div></body></html>".to_vec(),
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
        ("gemini-myactivity-html".into(), "parsed".into())
    );
    assert_valid(&out);
}

#[test]
fn takeout_html_upgrade_from_previously_unsupported_source() {
    let t = tmp("gem-html-upgrade");
    let out = t.join("out");

    // First import: a zip with an existing conversation
    let z1 = t.join("takeout_old.zip");
    zip_of(
        &z1,
        &[(
            &format!("{DIR}/conversation_1.txt"),
            conversation(false).to_string().into_bytes(),
        )],
    );
    import(&z1, &out);
    assert_eq!(gem(&out).len(), 2);

    // Now import a zip containing MyActivity.html with a new conversation
    let z2 = t.join("takeout_new.zip");
    let card = make_synthetic_takeout_card(
        Some("upgraded_conv"),
        "Prompted ",
        "question after upgrade",
        "Oct 6, 2026, 11:00:00\u{202f}AM GMT-03:00",
        "<p>Answer after upgrade</p>",
        None,
    );
    let full_html = format!("<html><body><div class=\"mdl-grid\">{card}</div></body></html>");
    zip_of(
        &z2,
        &[(
            "Takeout/My Activity/Gemini Apps/MyActivity.html",
            full_html.into_bytes(),
        )],
    );
    let log = import(&z2, &out);
    assert!(log.contains("events: 2 new"), "{log}");
    assert_valid(&out);
    assert_eq!(gem(&out).len(), 4);
    assert_eq!(
        inventory(&out, "MyActivity.html")[0],
        ("gemini-myactivity-html".into(), "parsed".into())
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
