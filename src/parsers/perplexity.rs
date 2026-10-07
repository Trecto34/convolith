//! Perplexity "user data export" (`conversations-<timestamp>-<hash>.json` next to a
//! `user-data-*.xlsx` workbook).
//!
//! Status: **verified against real exports** (two real account exports; schema
//! only was inspected, tests use synthetic content). Shape:
//!
//! ```json
//! {"conversations": [{"context_uuid", "context_title", "created_at", "updated_at",
//!   "mode", "collection_uuid", "entries": [{"entry_uuid", "query", "answer",
//!   "created_at", "engine_mode", "label", "query_status"}]}]}
//! ```
//!
//! There is no `version` field, no citations/sources and no per-entry model. Each
//! entry becomes a user event (`query`) and, when the answer is non-empty, an
//! assistant event (`answer`); `engine_mode`, `query_status` and `label` are kept
//! as metadata. Identity: conversation = `context_uuid`; events =
//! `<entry_uuid>:query` / `<entry_uuid>:answer`. The xlsx workbook (profile,
//! preferences, memory, subscription) is inventoried as unsupported. A JSON whose
//! `conversations` entries lack `context_uuid`/`entries` is not claimed.

use super::webexport::{iso_stamp, str_of, unique_id};
use crate::model::{EventDraft, EventType, Part, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct PerplexityParser;

fn candidate(p: &Probe) -> bool {
    p.ext() == "json"
        && p.head.contains("\"conversations\"")
        && p.head.contains("\"context_uuid\"")
        && p.head.contains("\"entries\"")
}

pub fn known_unsupported(p: &Probe) -> Option<(&'static str, &'static str)> {
    let name = p.filename().to_ascii_lowercase();
    if name.starts_with("user-data-") && name.ends_with(".xlsx") {
        return Some((
            "perplexity-export-workbook",
            "Perplexity account workbook (profile, preferences, memory, subscription); not conversation history",
        ));
    }
    None
}

impl SourceParser for PerplexityParser {
    fn id(&self) -> &'static str {
        "perplexity_export"
    }
    fn provider(&self) -> &'static str {
        "perplexity"
    }
    fn application(&self) -> &'static str {
        "perplexity"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: false,
            attachments: false,
            reasoning: false,
            streaming: false,
            partial: true,
        }
    }
    fn is_web_export(&self) -> bool {
        true
    }
    fn description(&self) -> &'static str {
        "Perplexity user data export conversations-*.json (verified against real exports)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        if candidate(p) {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "perplexity-user-data-export",
                Confidence::Strong,
                "conversations[] of context_uuid/entries[]",
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
        .context("reading export document")?;
        let Some(convs) = doc.get("conversations").and_then(Value::as_array) else {
            bail!("no conversations array (unrecognized Perplexity export shape)");
        };
        let mut report = ParseReport::default();
        for (i, c) in convs.iter().enumerate() {
            report.records_examined += 1;
            if let Err(e) = parse_conversation(c, i as u64, sink, &mut report) {
                report.records_failed += 1;
                report.notes.push(format!("conversation {i}: {e:#}"));
            }
        }
        Ok(report)
    }
}

fn parse_conversation(
    c: &Value,
    index: u64,
    sink: &mut dyn EventSink,
    report: &mut ParseReport,
) -> Result<()> {
    let Some(entries) = c.get("entries").and_then(Value::as_array) else {
        bail!("missing entries array (unrecognized Perplexity conversation shape)");
    };
    let id = c.get("context_uuid").and_then(Value::as_str);
    let mut meta = ConversationMeta {
        provider: Some("perplexity".into()),
        application: Some("perplexity".into()),
        native_id: unique_id(id),
        title: str_of(c, "context_title"),
        started_at: Some(iso_stamp(c.get("created_at"))).filter(|s| s.utc.is_some()),
        ended_at: Some(iso_stamp(c.get("updated_at"))).filter(|s| s.utc.is_some()),
        identity_hint: IdentityHint::Native,
        ..Default::default()
    };
    meta.metadata
        .insert("perplexity_context_uuid".into(), json!(id));
    meta.metadata
        .insert("conversation_index".into(), json!(index));
    for k in ["mode", "collection_uuid"] {
        if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
            meta.metadata.insert(format!("perplexity_{k}"), v.clone());
        }
    }
    sink.begin(meta)?;
    report.conversations += 1;
    for e in entries {
        let eid = str_of(e, "entry_uuid");
        let ts = iso_stamp(e.get("created_at"));
        let (q, a) = (
            str_of(e, "query"),
            str_of(e, "answer").filter(|a| !a.is_empty()),
        );
        if q.is_none() && a.is_none() {
            report.records_failed += 1;
            report.notes.push("entry without query or answer".into());
            continue;
        }
        let tag = |d: &mut EventDraft| {
            for k in ["engine_mode", "query_status", "label"] {
                if let Some(v) = e.get(k).filter(|v| !v.is_null()) {
                    d.metadata.insert(format!("perplexity_{k}"), v.clone());
                }
            }
        };
        let mut qid = None;
        if let Some(q) = q {
            let mut d =
                EventDraft::with_content(Role::User, EventType::Message, vec![Part::text(q)]);
            d.native_id = eid.as_ref().map(|i| format!("{i}:query"));
            d.timestamp = ts.clone();
            qid = d.native_id.clone();
            tag(&mut d);
            sink.emit(d)?;
            report.events += 1;
        }
        if let Some(a) = a {
            let mut d =
                EventDraft::with_content(Role::Assistant, EventType::Message, vec![Part::text(a)]);
            d.native_id = eid.as_ref().map(|i| format!("{i}:answer"));
            d.parent_native_id = qid;
            d.timestamp = ts;
            tag(&mut d);
            sink.emit(d)?;
            report.events += 1;
        }
    }
    sink.end()?;
    Ok(())
}
