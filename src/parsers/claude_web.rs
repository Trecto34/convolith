//! Claude (claude.ai) data export: `conversations.json` with `chat_messages`.
//!
//! Status: **verified against a real export** (split data-export format with
//! `manifest-*.json`; schema inspected, tests use synthetic content). Shape (`uuid`, `name`, `created_at`/`updated_at`, `account.uuid`,
//! `chat_messages[{uuid, sender, text, content[], created_at, updated_at,
//! attachments[], files[], parent_message_uuid}]`). Content blocks seen: `text`,
//! `thinking`, `tool_use`, `tool_result`, `injected_prompt_block`, `document`,
//! `image`, `token_budget` (the last four are kept opaque). A conversation without a
//! `chat_messages` array is a counted failure, never a guess. `projects.json`
//! and `users.json` are inventoried as unsupported (see `webexport`).
//!
//! Every message carries `parent_message_uuid` (real edit/retry branches exist);
//! the all-zero root sentinel `00000000-0000-4000-8000-000000000000` means "no
//! parent". Linear order is never turned into a parent link.

use super::webexport::{iso_stamp, str_of, stream_array, unique_id};
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Value};

pub struct ClaudeWebParser;

impl SourceParser for ClaudeWebParser {
    fn id(&self) -> &'static str {
        "claude_web_export"
    }
    fn provider(&self) -> &'static str {
        "anthropic"
    }
    fn application(&self) -> &'static str {
        "claude"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: true,
            reasoning: true,
            streaming: true,
            partial: true,
        }
    }
    fn is_web_export(&self) -> bool {
        true
    }
    fn description(&self) -> &'static str {
        "Claude (claude.ai) data export conversations.json (verified against a real export)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        if super::webexport::is_conversations_name(p.filename())
            && p.head.contains("\"chat_messages\"")
        {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "claude-web-export",
                Confidence::Strong,
                "claude.ai conversations with chat_messages",
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

fn blocks(msg: &Value) -> Vec<Part> {
    let mut out = Vec::new();
    let mut has_text = false;
    for b in msg
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                has_text = true;
                out.push(Part::text(
                    b.get("text").and_then(Value::as_str).unwrap_or(""),
                ));
            }
            Some("thinking") => out.push(Part::Reasoning {
                text: b
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                visibility: ReasoningVisibility::Public,
            }),
            Some("tool_use") => out.push(Part::ToolCall {
                id: str_of(b, "id"),
                name: str_of(b, "name").unwrap_or_default(),
                arguments: b.get("input").cloned().unwrap_or(Value::Null),
            }),
            Some("tool_result") => out.push(Part::ToolResult {
                tool_call_id: str_of(b, "tool_use_id"),
                output: b.get("content").cloned().unwrap_or(Value::Null),
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            }),
            other => out.push(Part::Opaque {
                kind: format!("claude_block:{}", other.unwrap_or("?")),
                note: None,
                raw: Some(b.clone()),
            }),
        }
    }
    if !has_text {
        if let Some(t) = msg
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            out.insert(0, Part::text(t));
        }
    }
    out
}

fn parse_conversation(
    _ctx: &mut dyn ParseContext,
    v: &Value,
    index: u64,
    sink: &mut dyn EventSink,
    report: &mut ParseReport,
) -> Result<()> {
    let msgs = v
        .get("chat_messages")
        .and_then(Value::as_array)
        .context("missing chat_messages array (unrecognized Claude export shape)")?;
    let uuid = v.get("uuid").and_then(Value::as_str);
    let mut meta = ConversationMeta {
        provider: Some("anthropic".into()),
        application: Some("claude".into()),
        native_id: unique_id(uuid),
        title: str_of(v, "name"),
        started_at: Some(iso_stamp(v.get("created_at"))).filter(|s| s.utc.is_some()),
        ended_at: Some(iso_stamp(v.get("updated_at"))).filter(|s| s.utc.is_some()),
        identity_hint: IdentityHint::Native,
        ..Default::default()
    };
    meta.metadata
        .insert("claude_conversation_uuid".into(), json!(uuid));
    meta.metadata
        .insert("conversation_index".into(), json!(index));
    if let Some(a) = v.pointer("/account/uuid") {
        meta.metadata
            .insert("claude_account_uuid".into(), a.clone());
    }
    if let Some(s) = v.get("summary").filter(|s| !s.is_null()) {
        meta.metadata.insert("claude_summary".into(), s.clone());
    }
    sink.begin(meta)?;
    report.conversations += 1;
    for m in msgs {
        let sender = m.get("sender").and_then(Value::as_str).unwrap_or("other");
        let mut content = blocks(m);
        let mut att_meta = Vec::new();
        for a in m
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            content.push(Part::FileRef {
                path: None,
                artifact: None,
                mime: str_of(a, "file_type"),
                filename: str_of(a, "file_name"),
            });
            if let Some(t) = a
                .get("extracted_content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                content.push(Part::text(t));
            }
            att_meta.push(json!({"file_name": a.get("file_name"), "file_type": a.get("file_type"), "file_size": a.get("file_size")}));
        }
        for f in m
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            content.push(Part::FileRef {
                path: None,
                artifact: None,
                mime: None,
                filename: str_of(f, "file_name"),
            });
            att_meta.push(f.clone());
        }
        let mut d = EventDraft::with_content(Role::parse(sender), EventType::Message, content);
        d.native_id = str_of(m, "uuid");
        d.parent_native_id = str_of(m, "parent_message_uuid")
            .filter(|p| p != "00000000-0000-4000-8000-000000000000");
        d.timestamp = iso_stamp(m.get("created_at"));
        d.model = str_of(m, "model");
        d.metadata.insert("claude_sender".into(), json!(sender));
        if let Some(u) = m.get("updated_at") {
            d.metadata.insert("claude_updated_at".into(), u.clone());
        }
        if !att_meta.is_empty() {
            d.metadata
                .insert("claude_attachments".into(), Value::Array(att_meta));
        }
        sink.emit(d)?;
        report.events += 1;
    }
    sink.end()?;
    Ok(())
}
