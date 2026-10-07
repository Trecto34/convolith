//! Gemini web history from Google Takeout.
//!
//! Two shapes:
//!
//! * `gemini-takeout-conversation` — **verified against a real export** (a
//!   Takeout "Gemini in Workspace / Conversation History" folder): one JSON
//!   document per conversation, stored as `conversation_<N>.txt`, with `title`,
//!   `creation_time`, `last_modification_time` and `conversation_turns[]` of
//!   `{user_turn:{prompt,turn_index,turn_last_modified,turn_deleted_time?},
//!   system_turn:{text[{data|cards[{content}]}],images[],model_thoughts[]…}}`.
//!   Images are referenced by name (`conversation_<N>_turn_<T>_images_<I>`);
//!   their bytes are not imported. Per-turn times are *last-modified* times,
//!   recorded as such in `gemini_timestamp_kind`. There is no per-message id, so
//!   identity is `<conversation file id>:<creation_time>` plus the turn index.
//! * `gemini-myactivity` — **unverified-against-real-export**: Takeout
//!   `My Activity/Gemini Apps/MyActivity.json`, entries
//!   `{header,title:"Prompted …",time,safeHtmlItem[{html}]}`. Entries have no
//!   ids, so identity is the provider `time` plus a hash of the entry title;
//!   other activity kinds are counted as skipped. `MyActivity.html` is
//!   inventoried as unsupported.

use super::webexport::{iso_stamp, str_of, stream_array};
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct GeminiWebParser;

fn is_myactivity(p: &Probe) -> bool {
    p.filename().eq_ignore_ascii_case("MyActivity.json")
        && (p.head.contains("Gemini Apps") || p.full_path.contains("Gemini"))
}

impl SourceParser for GeminiWebParser {
    fn id(&self) -> &'static str {
        "gemini_takeout"
    }
    fn provider(&self) -> &'static str {
        "google"
    }
    fn application(&self) -> &'static str {
        "gemini"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: false,
            attachments: true,
            reasoning: true,
            streaming: false,
            partial: true,
        }
    }
    fn is_web_export(&self) -> bool {
        true
    }
    fn description(&self) -> &'static str {
        "Gemini web history from Google Takeout (Conversation History verified; My Activity JSON unverified)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        let ext = p.ext();
        if (ext == "txt" || ext == "json") && p.head.contains("\"conversation_turns\"") {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "gemini-takeout-conversation",
                Confidence::Strong,
                "Takeout conversation document with conversation_turns",
            )
        } else if is_myactivity(p) {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "gemini-myactivity",
                Confidence::Weak,
                "Takeout My Activity (Gemini Apps) JSON; unverified against a real export",
            )
        } else {
            Detection::none(self.id())
        }
    }
    fn parse(
        &self,
        ctx: &mut dyn ParseContext,
        source: &Source,
        sink: &mut dyn EventSink,
    ) -> Result<ParseReport> {
        if source.size > ctx.max_file_bytes() {
            bail!("{} bytes exceeds the file size limit", source.size);
        }
        if source.read_path.extension().and_then(|e| e.to_str()) == Some("json")
            && source
                .inner_path
                .as_deref()
                .unwrap_or(&source.display_path)
                .to_ascii_lowercase()
                .ends_with("myactivity.json")
        {
            return parse_myactivity(source, sink);
        }
        let doc: Value = serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(
            &source.read_path,
        )?))
        .context("reading conversation document")?;
        let mut report = ParseReport {
            records_examined: 1,
            ..Default::default()
        };
        parse_conversation(source, &doc, sink, &mut report)?;
        Ok(report)
    }
}

fn file_id(source: &Source) -> String {
    let p = source.inner_path.as_deref().unwrap_or(&source.display_path);
    let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
    name.rsplit_once('.').map_or(name, |(s, _)| s).to_owned()
}

fn parse_conversation(
    source: &Source,
    doc: &Value,
    sink: &mut dyn EventSink,
    report: &mut ParseReport,
) -> Result<()> {
    let turns = doc
        .get("conversation_turns")
        .and_then(Value::as_array)
        .context("missing conversation_turns array (unrecognized Gemini export shape)")?;
    let fid = file_id(source);
    let created = doc.get("creation_time").and_then(Value::as_str);
    // `<file id>:<creation_time>` is account-scoped but stable across Takeouts.
    let key = created.map(|c| format!("gemini-takeout:{fid}:{c}"));
    let mut meta = ConversationMeta {
        provider: Some("google".into()),
        application: Some("gemini".into()),
        native_id: key.clone(),
        title: str_of(doc, "title"),
        started_at: Some(iso_stamp(doc.get("creation_time"))).filter(|s| s.utc.is_some()),
        ended_at: Some(iso_stamp(doc.get("last_modification_time"))).filter(|s| s.utc.is_some()),
        identity_hint: if key.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    };
    meta.metadata
        .insert("gemini_conversation_file".into(), json!(fid));
    sink.begin(meta)?;
    report.conversations += 1;
    for (i, t) in turns.iter().enumerate() {
        let (u, s) = (t.get("user_turn"), t.get("system_turn"));
        if u.is_none() && s.is_none() {
            report.records_failed += 1;
            report
                .notes
                .push(format!("turn {i}: neither user_turn nor system_turn"));
            continue;
        }
        let idx = |x: &Value| {
            x.get("turn_index")
                .and_then(Value::as_i64)
                .unwrap_or(i as i64)
        };
        let nid = |role: &str, x: &Value| key.as_ref().map(|k| format!("{k}:{role}:{}", idx(x)));
        let mut user_nid = None;
        if let Some(u) = u {
            let mut d = EventDraft::with_content(
                Role::User,
                EventType::Message,
                vec![Part::text(
                    u.get("prompt").and_then(Value::as_str).unwrap_or(""),
                )],
            );
            d.native_id = nid("user", u);
            user_nid = d.native_id.clone();
            stamp_turn(&mut d, u);
            sink.emit(d)?;
            report.events += 1;
        }
        if let Some(s) = s {
            let mut content = Vec::new();
            for th in s
                .get("model_thoughts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let g = |k| th.get(k).and_then(Value::as_str).unwrap_or("");
                content.push(Part::Reasoning {
                    text: format!("{}\n\n{}", g("headline"), g("description")),
                    visibility: ReasoningVisibility::Summary,
                });
            }
            for item in s
                .get("text")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(x) = item.get("data").and_then(Value::as_str) {
                    content.push(Part::text(x));
                }
                for c in item
                    .get("cards")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(x) = c.get("content").and_then(Value::as_str) {
                        content.push(Part::text(x));
                    }
                }
            }
            for img in s
                .get("images")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                content.push(Part::Image {
                    artifact: None,
                    mime: None,
                    filename: None,
                    source_ref: img.as_str().map(str::to_owned),
                });
            }
            let mut d = EventDraft::with_content(Role::Assistant, EventType::Message, content);
            d.native_id = nid("model", s);
            d.parent_native_id = user_nid;
            stamp_turn(&mut d, s);
            sink.emit(d)?;
            report.events += 1;
        }
    }
    sink.end()?;
    Ok(())
}

fn stamp_turn(d: &mut EventDraft, turn: &Value) {
    d.timestamp = iso_stamp(turn.get("turn_last_modified"));
    d.metadata
        .insert("gemini_timestamp_kind".into(), json!("turn_last_modified"));
    if let Some(x) = turn.get("turn_index") {
        d.metadata.insert("gemini_turn_index".into(), x.clone());
    }
    if let Some(x) = turn.get("turn_deleted_time").filter(|x| !x.is_null()) {
        d.metadata
            .insert("gemini_turn_deleted_time".into(), x.clone());
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn parse_myactivity(source: &Source, sink: &mut dyn EventSink) -> Result<ParseReport> {
    let mut report = ParseReport::default();
    stream_array(&source.read_path, "My Activity", &mut |e| {
        report.records_examined += 1;
        let title = e.get("title").and_then(Value::as_str).unwrap_or("");
        let Some(prompt) = title.strip_prefix("Prompted ") else {
            report.records_skipped += 1;
            return Ok(());
        };
        let Some(time) = e.get("time").and_then(Value::as_str) else {
            report.records_failed += 1;
            report
                .notes
                .push("activity entry without time: no stable identity".into());
            return Ok(());
        };
        let key = format!("gemini-myactivity:{}", crate::id::id("", &[time, title]));
        sink.begin(ConversationMeta {
            provider: Some("google".into()),
            application: Some("gemini".into()),
            native_id: Some(key.clone()),
            started_at: Some(iso_stamp(e.get("time"))).filter(|s| s.utc.is_some()),
            identity_hint: IdentityHint::Native,
            ..Default::default()
        })?;
        report.conversations += 1;
        let mut u =
            EventDraft::with_content(Role::User, EventType::Message, vec![Part::text(prompt)]);
        u.native_id = Some(format!("{key}:user"));
        u.timestamp = iso_stamp(e.get("time"));
        sink.emit(u)?;
        report.events += 1;
        let html: Vec<&str> = e
            .get("safeHtmlItem")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|i| i.get("html").and_then(Value::as_str))
            .collect();
        if !html.is_empty() {
            let raw = html.join("\n");
            let mut a = EventDraft::with_content(
                Role::Assistant,
                EventType::Message,
                vec![
                    Part::text(strip_tags(&raw)),
                    Part::Opaque {
                        kind: "gemini_safe_html".into(),
                        note: None,
                        raw: Some(Value::String(raw)),
                    },
                ],
            );
            a.native_id = Some(format!("{key}:model"));
            a.parent_native_id = Some(format!("{key}:user"));
            a.timestamp = iso_stamp(e.get("time"));
            sink.emit(a)?;
            report.events += 1;
        }
        sink.end()?;
        Ok(())
    })?;
    Ok(report)
}
