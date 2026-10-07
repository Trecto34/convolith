//! Claude Design chats from the Claude split data export: `design_chats/<uuid>.json`.
//!
//! Status: **verified against a real export** (schema inspected; tests use
//! synthetic content). One file per chat:
//! `{uuid, title, project{uuid,name}, created_at, updated_at, messages[{uuid, role,
//! created_at, content{id, role, kind, timestamp, content, contentBlocks[], attachments[]}}]}`.
//! Content blocks: `text`, `thinking`, `tool_call` (name/input/output) and
//! `user_interjection` (kept opaque). `content.kind` is `chat`, `question-record`
//! or `question-receipt`; it is kept in metadata. Chats with no messages are
//! counted as skipped. Identity: chat `uuid` / message `uuid`; application
//! `claude-design`.

use super::webexport::{iso_stamp, str_of};
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct ClaudeDesignParser;

impl SourceParser for ClaudeDesignParser {
    fn id(&self) -> &'static str {
        "claude_design_export"
    }
    fn provider(&self) -> &'static str {
        "anthropic"
    }
    fn application(&self) -> &'static str {
        "claude-design"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
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
        "Claude Design chats design_chats/*.json from the Claude data export (verified against a real export)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        let in_dir = p.rel_path.replace('\\', "/").starts_with("design_chats/")
            || p.has_component("design_chats");
        if p.ext() == "json"
            && in_dir
            && p.head.contains("\"messages\"")
            && p.head.contains("\"uuid\"")
        {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "claude-design-chat",
                Confidence::Strong,
                "design_chats/<uuid>.json with messages[]",
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
        let doc: Value = serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(
            &source.read_path,
        )?))
        .context("reading design chat")?;
        let msgs = doc
            .get("messages")
            .and_then(Value::as_array)
            .context("missing messages array (unrecognized Claude Design shape)")?;
        let mut report = ParseReport {
            records_examined: 1,
            ..Default::default()
        };
        if msgs.is_empty() {
            report.records_skipped = 1;
            report.notes.push("design chat without messages".into());
            return Ok(report);
        }
        let mut meta = ConversationMeta {
            provider: Some("anthropic".into()),
            application: Some("claude-design".into()),
            native_id: doc
                .get("uuid")
                .and_then(Value::as_str)
                .and_then(|u| super::webexport::unique_id(Some(u))),
            title: str_of(&doc, "title"),
            started_at: Some(iso_stamp(doc.get("created_at"))).filter(|s| s.utc.is_some()),
            ended_at: Some(iso_stamp(doc.get("updated_at"))).filter(|s| s.utc.is_some()),
            identity_hint: IdentityHint::Native,
            ..Default::default()
        };
        if let Some(p) = doc.get("project") {
            meta.metadata
                .insert("claude_design_project".into(), p.clone());
        }
        sink.begin(meta)?;
        report.conversations = 1;
        for m in msgs {
            let c = m.get("content").unwrap_or(&Value::Null);
            let role = Role::parse(m.get("role").and_then(Value::as_str).unwrap_or("other"));
            let mut parts = Vec::new();
            let blocks = c.get("contentBlocks").and_then(Value::as_array);
            for b in blocks.into_iter().flatten() {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(Part::text(
                        b.get("text").and_then(Value::as_str).unwrap_or(""),
                    )),
                    Some("thinking") => parts.push(Part::Reasoning {
                        text: b
                            .get("text")
                            .or_else(|| b.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        visibility: ReasoningVisibility::Public,
                    }),
                    Some("tool_call") => {
                        let t = b.get("toolCall").unwrap_or(&Value::Null);
                        let id = str_of(t, "id");
                        parts.push(Part::ToolCall {
                            id: id.clone(),
                            name: str_of(t, "name").unwrap_or_default(),
                            arguments: t.get("input").cloned().unwrap_or(Value::Null),
                        });
                        if let Some(o) = t.get("output") {
                            parts.push(Part::ToolResult {
                                tool_call_id: id,
                                output: o.clone(),
                                is_error: false,
                            });
                        }
                    }
                    other => parts.push(Part::Opaque {
                        kind: format!("claude_design_block:{}", other.unwrap_or("?")),
                        note: None,
                        raw: Some(b.clone()),
                    }),
                }
            }
            if !parts.iter().any(|p| matches!(p, Part::Text { .. })) {
                if let Some(t) = c
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    parts.insert(0, Part::text(t));
                }
            }
            for a in c
                .get("attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                parts.push(Part::FileRef {
                    path: str_of(a, "path"),
                    artifact: None,
                    mime: None,
                    filename: str_of(a, "name"),
                });
                if let Some(t) = a
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    parts.push(Part::text(t));
                }
            }
            let mut d = EventDraft::with_content(role, EventType::Message, parts);
            d.native_id = str_of(m, "uuid");
            d.timestamp = iso_stamp(m.get("created_at"));
            if let Some(k) = c.get("kind") {
                d.metadata.insert("claude_design_kind".into(), k.clone());
            }
            if let Some(t) = c.get("turnInputTokens") {
                d.metadata
                    .insert("claude_design_turn_input_tokens".into(), t.clone());
            }
            for k in ["questionRecord", "questionReceipt", "turnChanges"] {
                if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
                    d.metadata.insert(format!("claude_design_{k}"), v.clone());
                }
            }
            d.metadata.insert(
                "claude_design_attachments".into(),
                json!(c
                    .get("attachments")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0)),
            );
            sink.emit(d)?;
            report.events += 1;
        }
        sink.end()?;
        Ok(report)
    }
}
