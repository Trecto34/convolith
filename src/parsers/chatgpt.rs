use crate::model::{EventDraft, EventType, Part, Role};
use crate::parser::{ConversationMeta, EventSink, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use crate::timeutil::{Stamp, TimestampConfidence, Utc};
use anyhow::{Context, Result};
use serde::de::{SeqAccess, Visitor};
use serde_json::{json, Value};
use std::fmt;
use std::fs::File;
use std::io::BufReader;

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
    fn description(&self) -> &'static str {
        "OpenAI ChatGPT conversations.json export"
    }
    fn detect(&self, p: &Probe) -> Detection {
        if p.filename() == "conversations.json"
            && (p.head.contains("mapping") || p.head.contains("current_node"))
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
        let file = File::open(&source.read_path)
            .with_context(|| format!("opening {}", source.display_path))?;
        let mut report = ParseReport::default();
        let mut visitor = ConversationArray {
            ctx,
            sink,
            report: &mut report,
            index: 0,
        };
        serde::Deserializer::deserialize_seq(
            &mut serde_json::Deserializer::from_reader(BufReader::new(file)),
            &mut visitor,
        )
        .context("reading top-level conversations array")?;
        Ok(report)
    }
}

struct ConversationArray<'a> {
    ctx: &'a mut dyn ParseContext,
    sink: &'a mut dyn EventSink,
    report: &'a mut ParseReport,
    index: u64,
}
impl<'de> Visitor<'de> for &mut ConversationArray<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a ChatGPT conversations array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while let Some(conv) = seq.next_element::<Value>()? {
            self.report.records_examined += 1;
            match parse_conversation(self.ctx, &conv, self.index, self.sink, self.report) {
                Ok(()) => {}
                Err(e) => {
                    self.report.records_failed += 1;
                    self.report
                        .notes
                        .push(format!("conversation {}: {e:#}", self.index));
                }
            }
            self.index += 1;
        }
        Ok(())
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
    let mut meta = ConversationMeta {
        provider: Some("openai".into()),
        application: Some("chatgpt".into()),
        native_id: obj.get("id").and_then(Value::as_str).map(str::to_owned),
        title: obj.get("title").and_then(Value::as_str).map(str::to_owned),
        identity_hint: crate::parser::IdentityHint::Fingerprint,
        ..Default::default()
    };
    meta.metadata.insert(
        "current_node".into(),
        obj.get("current_node").cloned().unwrap_or(Value::Null),
    );
    meta.metadata
        .insert("conversation_index".into(), json!(index));
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
            for part in parts {
                if let Some(s) = part.as_str() {
                    content.push(Part::text(s));
                } else {
                    content.push(Part::Opaque {
                        kind: "chatgpt_content_part".into(),
                        note: None,
                        raw: Some(part.clone()),
                    });
                }
            }
        } else if let Some(text) = message.pointer("/content/text").and_then(Value::as_str) {
            content.push(Part::text(text));
        }
        if let Some(attachments) = message.get("attachments").and_then(Value::as_array) {
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
        }
        let mut draft = EventDraft::with_content(role, EventType::Message, content);
        draft.native_id = Some(key.clone());
        draft.parent_native_id = node
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_owned);
        draft.model = message
            .get("metadata")
            .and_then(|m| m.get("model_slug"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(t) = message.get("create_time").and_then(Value::as_f64) {
            if t.is_finite() {
                draft.timestamp = Stamp::from_utc(
                    Utc((t * 1e9) as i64),
                    TimestampConfidence::ProviderDerived,
                    Some(t.to_string()),
                );
            }
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
