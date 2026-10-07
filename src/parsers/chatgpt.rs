//! ChatGPT data export (`conversations.json`, or sharded `conversations-NNN.json`).
//!
//! Status: **unverified-against-real-export**. Built from the documented export
//! layout (a `mapping` tree of nodes with `parent`/`children`, `current_node`,
//! `author.role`, `content.parts`, epoch-float `create_time`/`update_time`,
//! `metadata.model_slug`, `metadata.attachments`); no real export was available
//! to check it against. Unrecognised content types are kept as opaque parts and
//! a conversation without a `mapping` object is a counted failure, never a guess.
//!
//! Every node that carries a message is emitted, so regenerations and edited
//! branches survive, not only the path ending at `current_node`
//! (`chatgpt_on_current_path` marks that path). Identity comes from the
//! provider's conversation id and node key, so re-importing the same or a newer
//! export of the account adds only new events. Event ids, conversation ids and
//! content parts are deliberately identical to v0.1.0's (golden ids in
//! `tests/webexport_chatgpt_compat.rs`); anything newer lives in event metadata.

use super::webexport::{epoch_stamp, is_conversations_name, stream_array};
use crate::model::{EventDraft, EventType, Part, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

pub struct ChatGptParser;

impl SourceParser for ChatGptParser {
    fn id(&self) -> &'static str {
        "chatgpt_export"
    }
    fn provider(&self) -> &'static str {
        "openai"
    }
    fn application(&self) -> &'static str {
        "chatgpt"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: false,
            attachments: true,
            reasoning: false,
            streaming: true,
            partial: true,
        }
    }
    fn is_web_export(&self) -> bool {
        true
    }
    fn description(&self) -> &'static str {
        "OpenAI ChatGPT data export conversations.json (unverified against a real export)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        if is_conversations_name(p.filename())
            && (p.head.contains("\"mapping\"") || p.head.contains("\"current_node\""))
        {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "chatgpt-export",
                Confidence::Strong,
                "ChatGPT mapping/current_node schema",
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
        let mut report = ParseReport::default();
        let mut index = 0u64;
        stream_array(&source.read_path, "conversations", &mut |conv| {
            report.records_examined += 1;
            if let Err(e) = parse_conversation(ctx, &conv, index, sink, &mut report) {
                report.records_failed += 1;
                report.notes.push(format!("conversation {index}: {e:#}"));
            }
            index += 1;
            Ok(())
        })?;
        Ok(report)
    }
}

/// Nearest ancestor of `key` that carries a message (empty root nodes skipped).
/// Node keys, not `message.id`, are the event ids: that is what v0.1.0 used, so
/// archives that already hold ChatGPT events keep their event ids.
fn message_parent(mapping: &Map<String, Value>, key: &str) -> Option<String> {
    let has_message = |k: &str| {
        mapping
            .get(k)
            .and_then(|n| n.get("message"))
            .is_some_and(|m| !m.is_null())
    };
    let mut seen = HashSet::new();
    let mut cur = mapping.get(key)?.get("parent")?.as_str()?;
    while seen.insert(cur) {
        if has_message(cur) {
            return Some(cur.to_owned());
        }
        cur = mapping.get(cur)?.get("parent")?.as_str()?;
    }
    None
}

fn map_part(part: &Value) -> Part {
    match part.as_str() {
        Some(s) => Part::text(s),
        None => Part::Opaque {
            kind: "chatgpt_content_part".into(),
            note: None,
            raw: Some(part.clone()),
        },
    }
}

fn parse_conversation(
    _ctx: &mut dyn ParseContext,
    v: &Value,
    index: u64,
    sink: &mut dyn EventSink,
    report: &mut ParseReport,
) -> Result<()> {
    let obj = v.as_object().context("record is not an object")?;
    let mapping = obj
        .get("mapping")
        .and_then(Value::as_object)
        .context("missing mapping object")?;
    let mut keys: Vec<&String> = mapping.keys().collect();
    keys.sort_by(|left, right| {
        let time = |key: &String| {
            mapping
                .get(key)
                .and_then(|n| n.get("message"))
                .and_then(|m| m.get("create_time"))
                .and_then(Value::as_f64)
                .unwrap_or(f64::INFINITY)
        };
        time(left).total_cmp(&time(right))
    });
    let mut current_path = HashSet::new();
    let mut cur = obj.get("current_node").and_then(Value::as_str);
    while let Some(k) = cur {
        if !current_path.insert(k) {
            break;
        }
        cur = mapping
            .get(k)
            .and_then(|n| n.get("parent"))
            .and_then(Value::as_str);
    }
    // `id` first, as v0.1.0 did, so conversation ids of existing archives stay.
    let conv_id = obj
        .get("id")
        .or_else(|| obj.get("conversation_id"))
        .and_then(Value::as_str);
    let mut meta = ConversationMeta {
        provider: Some("openai".into()),
        application: Some("chatgpt".into()),
        native_id: conv_id.map(str::to_owned),
        title: obj.get("title").and_then(Value::as_str).map(str::to_owned),
        model: obj
            .get("default_model_slug")
            .and_then(Value::as_str)
            .map(str::to_owned),
        started_at: Some(epoch_stamp(obj.get("create_time"))).filter(|s| s.utc.is_some()),
        ended_at: Some(epoch_stamp(obj.get("update_time"))).filter(|s| s.utc.is_some()),
        identity_hint: IdentityHint::Native,
        ..Default::default()
    };
    meta.metadata.insert(
        "current_node".into(),
        obj.get("current_node").cloned().unwrap_or(Value::Null),
    );
    meta.metadata
        .insert("conversation_index".into(), json!(index));
    meta.metadata
        .insert("chatgpt_conversation_id".into(), json!(conv_id));
    for (k, val) in obj {
        if val.is_string() || val.is_number() || val.is_boolean() {
            meta.metadata
                .entry(format!("chatgpt_{k}"))
                .or_insert_with(|| val.clone());
        }
    }
    sink.begin(meta)?;
    report.conversations += 1;
    for key in keys.into_iter() {
        let node = &mapping[key];
        let Some(message) = node.get("message").filter(|m| !m.is_null()) else {
            report.records_skipped += 1;
            continue;
        };
        let author = message
            .pointer("/author/role")
            .and_then(Value::as_str)
            .unwrap_or("other");
        let role = Role::parse(author);
        let mut content = Vec::new();
        if let Some(parts) = message.pointer("/content/parts").and_then(Value::as_array) {
            content.extend(parts.iter().map(map_part));
        } else if let Some(text) = message.pointer("/content/text").and_then(Value::as_str) {
            content.push(Part::text(text));
        }
        // Parts must stay what v0.1.0 produced (they feed the content fingerprint, so
        // changing them would turn re-imports of old archives into conflicts).
        // Everything newer goes to metadata.
        let attachments = message
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for a in attachments {
            let filename = a
                .get("name")
                .or_else(|| a.get("filename"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let reference = a
                .get("id")
                .or_else(|| a.get("file_id"))
                .or_else(|| a.get("url"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            content.push(Part::FileRef {
                path: a.get("path").and_then(Value::as_str).map(str::to_owned),
                artifact: None,
                mime: a
                    .get("mime_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                filename,
            });
            if let Some(r) = reference {
                content.push(Part::Opaque {
                    kind: "chatgpt_attachment_ref".into(),
                    note: Some(r),
                    raw: Some(a.clone()),
                });
            }
        }
        let mut draft = EventDraft::with_content(role, EventType::Message, content);
        draft.native_id = Some(key.clone());
        draft.parent_native_id = message_parent(mapping, key);
        draft.model = message
            .get("metadata")
            .and_then(|m| m.get("model_slug"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        draft.timestamp = epoch_stamp(message.get("create_time"));
        if let Some(mm) = message.get("metadata").and_then(Value::as_object) {
            for k in ["end_turn", "status", "is_visually_hidden_from_conversation"] {
                if let Some(x) = message.get(k).or_else(|| mm.get(k)) {
                    draft.metadata.insert(format!("chatgpt_{k}"), x.clone());
                }
            }
        }
        for k in ["update_time", "recipient", "weight", "end_turn", "status"] {
            if let Some(x) = message.get(k).filter(|x| !x.is_null()) {
                draft.metadata.insert(format!("chatgpt_{k}"), x.clone());
            }
        }
        if let Some(a) = message
            .pointer("/metadata/attachments")
            .filter(|a| !a.is_null())
        {
            draft
                .metadata
                .insert("chatgpt_attachments".into(), a.clone());
        }
        if message.pointer("/content/parts").is_none() && message.pointer("/content/text").is_none()
        {
            if let Some(c) = message.get("content").filter(|c| !c.is_null()) {
                draft.metadata.insert("chatgpt_content".into(), c.clone());
            }
        }
        if let Some(id) = message
            .get("id")
            .filter(|i| i.as_str() != Some(key.as_str()))
        {
            draft
                .metadata
                .insert("chatgpt_message_id".into(), id.clone());
        }
        if let Some(ct) = message.pointer("/content/content_type") {
            draft
                .metadata
                .insert("chatgpt_content_type".into(), ct.clone());
        }
        draft.metadata.insert("chatgpt_node_id".into(), json!(key));
        draft.metadata.insert(
            "chatgpt_parent".into(),
            node.get("parent").cloned().unwrap_or(Value::Null),
        );
        draft.metadata.insert(
            "chatgpt_children".into(),
            node.get("children").cloned().unwrap_or(json!([])),
        );
        draft.metadata.insert(
            "chatgpt_current_node".into(),
            obj.get("current_node").cloned().unwrap_or(Value::Null),
        );
        draft.metadata.insert(
            "chatgpt_on_current_path".into(),
            json!(current_path.contains(key.as_str())),
        );
        draft
            .metadata
            .insert("chatgpt_author".into(), json!(author));
        sink.emit(draft)?;
        report.events += 1;
    }
    sink.end()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParseContext;
    use crate::secrets::RedactionHit;
    use crate::timeutil::TimestampConfidence;
    use std::path::Path;
    struct Context;
    impl ParseContext for Context {
        fn staging_dir(&self) -> &Path {
            Path::new(".")
        }
        fn machine_id(&self) -> Option<&str> {
            None
        }
        fn platform(&self) -> Option<&str> {
            None
        }
        fn import_run(&self) -> &str {
            "test"
        }
        fn provider(&self) -> &str {
            "openai"
        }
        fn application(&self) -> &str {
            "chatgpt"
        }
        fn parser_id(&self) -> &str {
            "chatgpt_export"
        }
        fn parser_version(&self) -> &str {
            "1"
        }
        fn max_inline_text_bytes(&self) -> usize {
            1024
        }
        fn store_artifact(
            &mut self,
            _: &[u8],
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String> {
            Ok(String::new())
        }
        fn apply_secret_policy(&self, _: &str, text: &str) -> (String, Option<RedactionHit>) {
            (text.into(), None)
        }
        fn max_record_bytes(&self) -> usize {
            1024 * 1024
        }
        fn note(&mut self, _: String) {}
    }
    #[derive(Default)]
    struct Sink {
        drafts: Vec<EventDraft>,
    }
    impl EventSink for Sink {
        fn begin(&mut self, _: ConversationMeta) -> Result<()> {
            Ok(())
        }
        fn emit(&mut self, d: EventDraft) -> Result<()> {
            self.drafts.push(d);
            Ok(())
        }
        fn end(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn retains_branch_nodes_attachments_and_missing_timestamps() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../fixtures/chatgpt/conversations.json"))
                .unwrap();
        let conv = &fixture[0];
        let mut ctx = Context;
        let mut sink = Sink::default();
        let mut report = ParseReport::default();
        parse_conversation(&mut ctx, conv, 0, &mut sink, &mut report).unwrap();
        assert_eq!(sink.drafts.len(), 3);
        assert_eq!(sink.drafts.iter().filter(|d| d.metadata.get("chatgpt_parent").and_then(Value::as_str)==Some("root")).count(),2);
        assert!(sink.drafts.iter().any(|d| d
            .content
            .iter()
            .any(|p| matches!(p,Part::FileRef{filename:Some(f),..} if f=="notes.txt"))));
        assert!(sink.drafts.iter().all(|d| d.timestamp.utc.is_some()));
        let missing = json!({"mapping":{"n":{"message":{"author":{"role":"user"},"content":{"parts":["x"]}}}}});
        let mut sink = Sink::default();
        parse_conversation(&mut ctx, &missing, 1, &mut sink, &mut report).unwrap();
        assert_eq!(
            sink.drafts[0].timestamp.confidence,
            TimestampConfidence::Unknown
        );
    }
    #[test]
    fn generic_fixture_contains_corrupt_and_unclear_records() {
        let lines = include_str!("../../fixtures/generic/messages.jsonl");
        assert!(serde_json::from_str::<Value>(lines.lines().nth(1).unwrap()).is_err());
        let missing: Value = serde_json::from_str(lines.lines().nth(3).unwrap()).unwrap();
        assert!(crate::parsers::generic::clear_message(&missing).is_none());
    }
}
