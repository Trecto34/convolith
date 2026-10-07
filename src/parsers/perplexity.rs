//! Perplexity thread export (JSON).
//!
//! Status: **unverified-against-real-export**, and the least certain parser:
//! Perplexity documents no bulk JSON export schema, and no real sample was
//! available. This reads only a narrowly defined shape and refuses the rest:
//!
//! ```json
//! {"version": 1, "threads": [{"id"|"uuid"|"thread_id": "...", "title": "...",
//!   "created_at": "<RFC 3339>", "entries": [{"id"|"uuid": "...",
//!   "query": "...", "answer": "...", "created_at": "<RFC 3339>",
//!   "sources": [{"url": "...", "title": "..."}]}]}]}
//! ```
//!
//! (a bare array of threads is accepted too). A file is claimed only when the
//! path or its head says `perplexity` and it looks like threads of entries. A
//! declared `version` other than 1 is **not** guessed at: the file is
//! inventoried as unsupported with the version in the reason. Per-thread
//! Markdown/HTML/PDF exports have no ids or timestamps and are inventoried too.

use super::webexport::{iso_stamp, str_of, unique_id};
use crate::model::{EventDraft, EventType, Part, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub struct PerplexityParser;

/// Value of a top-level-looking `"version"` key in the probe window, if any.
fn declared_version(head: &str) -> Option<String> {
    let rest = head.split_once("\"version\"")?.1.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let end = rest.find([',', '}', '\n']).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"').to_owned())
}

fn candidate(p: &Probe) -> bool {
    p.ext() == "json"
        && (p.full_path.to_ascii_lowercase().contains("perplexity")
            || p.head.to_ascii_lowercase().contains("perplexity"))
        && (p.head.contains("\"entries\"") || p.head.contains("\"threads\""))
}

pub fn known_unsupported(p: &Probe) -> Option<(&'static str, &'static str)> {
    if !candidate(p) {
        return None;
    }
    // `candidate` + a version this parser does not know (detect declined it).
    Some((
        "perplexity-export-unknown-version",
        "Perplexity export declares a version this parser does not know (only version 1 is read); not guessed at",
    ))
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
        "Perplexity thread export JSON, version 1 only (unverified against a real export)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        let known = declared_version(&p.head).map_or(true, |v| matches!(v.as_str(), "1" | "1.0"));
        if candidate(p) && known {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "perplexity-export-v1",
                Confidence::Weak,
                "Perplexity threads/entries JSON; unverified against a real export",
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
        )?))?;
        let threads = match &doc {
            Value::Array(a) => a,
            d => match d.get("threads").and_then(Value::as_array) {
                Some(a) => a,
                None => bail!("no threads array (unrecognized Perplexity export shape)"),
            },
        };
        let mut report = ParseReport::default();
        for (i, t) in threads.iter().enumerate() {
            report.records_examined += 1;
            if let Err(e) = parse_thread(t, i as u64, sink, &mut report) {
                report.records_failed += 1;
                report.notes.push(format!("thread {i}: {e:#}"));
            }
        }
        Ok(report)
    }
}

fn first_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))
}

fn parse_thread(
    t: &Value,
    index: u64,
    sink: &mut dyn EventSink,
    report: &mut ParseReport,
) -> Result<()> {
    let Some(entries) = t.get("entries").and_then(Value::as_array) else {
        bail!("missing entries array (unrecognized Perplexity thread shape)");
    };
    let id = first_str(t, &["id", "uuid", "thread_id"]);
    sink.begin(ConversationMeta {
        provider: Some("perplexity".into()),
        application: Some("perplexity".into()),
        native_id: unique_id(id),
        title: first_str(t, &["title", "name"]).map(str::to_owned),
        started_at: Some(iso_stamp(t.get("created_at"))).filter(|s| s.utc.is_some()),
        identity_hint: IdentityHint::Native,
        metadata: [
            ("perplexity_thread_id".to_string(), json!(id)),
            ("conversation_index".to_string(), json!(index)),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    })?;
    report.conversations += 1;
    for e in entries {
        let eid = first_str(e, &["id", "uuid"]).map(str::to_owned);
        let ts = iso_stamp(e.get("created_at"));
        let q = first_str(e, &["query", "question", "prompt"]);
        let a = first_str(e, &["answer", "response"]);
        if q.is_none() && a.is_none() {
            report.records_failed += 1;
            report.notes.push("entry without query or answer".into());
            continue;
        }
        let mut uid = None;
        if let Some(q) = q {
            let mut d =
                EventDraft::with_content(Role::User, EventType::Message, vec![Part::text(q)]);
            d.native_id = eid.as_ref().map(|i| format!("{i}:query"));
            d.timestamp = ts.clone();
            uid = d.native_id.clone();
            sink.emit(d)?;
            report.events += 1;
        }
        if let Some(a) = a {
            let mut content = vec![Part::text(a)];
            for s in e
                .get("sources")
                .or_else(|| e.get("citations"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                content.push(Part::Opaque {
                    kind: "perplexity_source".into(),
                    note: str_of(s, "url").or_else(|| s.as_str().map(str::to_owned)),
                    raw: Some(s.clone()),
                });
            }
            let mut d = EventDraft::with_content(Role::Assistant, EventType::Message, content);
            d.native_id = eid.as_ref().map(|i| format!("{i}:answer"));
            d.parent_native_id = uid;
            d.model = str_of(e, "model");
            d.timestamp = ts;
            sink.emit(d)?;
            report.events += 1;
        }
    }
    sink.end()?;
    Ok(())
}
