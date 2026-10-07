//! Antigravity CLI conversation transcripts
//! (`~/.gemini/antigravity-cli/brain/<conversation-uuid>/.system_generated/logs/transcript_full.jsonl`).
//!
//! One JSON object per trajectory step: `{step_index, source, type, status,
//! created_at, ...}` where `source` is `USER_EXPLICIT`, `MODEL` or `SYSTEM`.
//! `transcript_full.jsonl` is the untruncated form; `transcript.jsonl` and
//! `logs/chunks/**` are derived copies and are inventoried as unsupported
//! (see [`known_unsupported`]) so the same step is never imported twice.
//!
//! Mapping, in short:
//! * `USER_INPUT` is a user message; `PLANNER_RESPONSE` is an assistant event
//!   (`thinking`, `content`, `tool_calls`);
//! * every other `MODEL` step is the output of a tool the planner ran and
//!   becomes a `tool` event. The transcript carries no call ids, so no link to
//!   the call is invented;
//! * `SYSTEM` steps and errors become system events; unknown shapes stay opaque;
//! * `step_index` is not unique inside a conversation (rewound runs repeat
//!   it), so it is metadata only; identity is position + content fingerprint
//!   under the conversation id taken from the `brain/<uuid>` directory.

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct AntigravityParser;

const TRANSCRIPT: &str = "transcript_full.jsonl";

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn is_step(o: &Map<String, Value>) -> bool {
    o.get("step_index").is_some_and(Value::is_number)
        && s(o, "source").is_some()
        && s(o, "type").is_some()
        && s(o, "created_at").is_some()
}

/// `<uuid>` of `.../brain/<uuid>/.system_generated/logs/transcript_full.jsonl`.
fn conversation_id(path: &Path) -> Option<String> {
    let comps: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    let i = comps.iter().position(|c| *c == ".system_generated")?;
    let id = comps.get(i.checked_sub(1)?)?.to_str()?;
    is_globally_unique(id).then(|| id.to_string())
}

impl SourceParser for AntigravityParser {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn provider(&self) -> &'static str {
        "google"
    }
    fn application(&self) -> &'static str {
        "antigravity-cli"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: false,
            reasoning: true,
            streaming: true,
            partial: false,
        }
    }
    fn description(&self) -> &'static str {
        "Antigravity CLI transcripts (~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript_full.jsonl)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir || probe.ext() != "jsonl" || probe.filename() != TRANSCRIPT {
            return Detection::none(self.id());
        }
        let records = probe_records(probe, 5);
        // The first step (the user prompt) can be longer than the probe window;
        // the step envelope keys come first on the line, so they are still seen.
        let oversized_first = records.is_empty()
            && probe.head.starts_with('{')
            && ["\"step_index\"", "\"source\"", "\"type\"", "\"created_at\""]
                .iter()
                .all(|k| probe.head.contains(k));
        if !records.first().is_some_and(is_step) && !oversized_first {
            return Detection::none(self.id());
        }
        let in_store = probe.has_component(".system_generated");
        Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            "antigravity-transcript-jsonl",
            if in_store {
                Confidence::Certain
            } else {
                Confidence::Strong
            },
            "trajectory step records (step_index/source/type/created_at)",
        )
    }

    fn parse(
        &self,
        ctx: &mut dyn ParseContext,
        source: &Source,
        sink: &mut dyn EventSink,
    ) -> Result<ParseReport> {
        let file = File::open(&source.read_path)
            .with_context(|| format!("open {}", source.read_path.display()))?;
        let mut reader = LineReader::new(BufReader::new(file), ctx.max_record_bytes());
        let mut tally = Tally::default();
        let mut stream = Stream::new(sink);
        let id = conversation_id(&source.read_path).or_else(|| {
            // A staged copy keeps its in-archive path only in `inner_path`.
            source
                .inner_path
                .as_deref()
                .and_then(|p| conversation_id(Path::new(p)))
        });
        let meta = ConversationMeta {
            native_id: id.clone(),
            native_session_id: id,
            ..Default::default()
        };
        stream.begin(meta.clone())?;
        while let Some(line) = reader.next_line().context("read transcript")? {
            if let Some(n) = line.oversize {
                tally.fail(
                    line.number,
                    format!("record of {n} bytes exceeds the record limit"),
                );
                continue;
            }
            match serde_json::from_slice::<Value>(reader.bytes()) {
                Ok(Value::Object(o)) => match convert(&o, line.number) {
                    Ok(d) => stream.push(d)?,
                    Err(why) => tally.fail(line.number, why),
                },
                Ok(_) => tally.fail(line.number, "record is not a JSON object"),
                Err(e) => tally.fail(line.number, format!("invalid JSON: {e}")),
            }
        }
        stream.finish(meta)?;
        Ok(tally.into_report(&stream))
    }
}

fn opaque(kind: &str, raw: Value) -> Part {
    Part::Opaque {
        kind: kind.to_string(),
        note: None,
        raw: Some(raw),
    }
}

fn convert(o: &Map<String, Value>, line: u64) -> Result<EventDraft, String> {
    if !o.get("step_index").is_some_and(Value::is_number) {
        return Err("record has no numeric `step_index`".into());
    }
    let ty = s(o, "type").ok_or("record has no string `type`")?;
    let src = s(o, "source").unwrap_or("");
    let mut d = EventDraft {
        timestamp: stamp(o.get("created_at")),
        ..Default::default()
    };
    d.metadata.insert("source_line".into(), json!(line));
    for (k, dst) in [
        ("step_index", "step_index"),
        ("type", "step_type"),
        ("source", "step_source"),
        ("status", "step_status"),
        ("exit_code", "exit_code"),
        ("error_code", "error_code"),
        ("truncated_fields", "truncated_fields"),
    ] {
        if let Some(v) = o.get(k).filter(|v| !v.is_null()) {
            d.metadata.insert(dst.into(), v.clone());
        }
    }
    let unmapped: Map<String, Value> = o
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "step_index"
                    | "source"
                    | "type"
                    | "status"
                    | "created_at"
                    | "content"
                    | "thinking"
                    | "tool_calls"
                    | "error"
                    | "error_code"
                    | "exit_code"
                    | "media"
                    | "truncated_fields"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !unmapped.is_empty() {
        d.metadata
            .insert("unmapped".into(), Value::Object(unmapped));
    }
    let content = o
        .get("content")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty());
    let whole = || Value::Object(o.clone());
    match (src, ty) {
        ("USER_EXPLICIT", "USER_INPUT") => {
            d.role = Role::User;
            d.content = vec![content.map_or_else(|| opaque(ty, whole()), Part::text)];
        }
        ("MODEL", "PLANNER_RESPONSE") => {
            d.role = Role::Assistant;
            if let Some(t) = s(o, "thinking") {
                d.content.push(Part::Reasoning {
                    text: t.into(),
                    visibility: ReasoningVisibility::Public,
                });
            }
            if let Some(t) = content {
                d.content.push(Part::text(t));
            }
            match o.get("tool_calls") {
                Some(Value::Array(calls)) => {
                    for c in calls {
                        d.content.push(match c.get("name").and_then(Value::as_str) {
                            Some(name) => Part::ToolCall {
                                id: None,
                                name: name.into(),
                                arguments: c.get("args").cloned().unwrap_or(Value::Null),
                            },
                            None => opaque("tool_call", c.clone()),
                        });
                    }
                }
                Some(Value::Null) | None => {}
                Some(other) => d.content.push(opaque("tool_calls", other.clone())),
            }
            if d.content.is_empty() {
                d.content.push(opaque(ty, whole()));
            }
            let has = |f: fn(&Part) -> bool| d.content.iter().any(f);
            if !has(|p| matches!(p, Part::Text { .. })) {
                if has(|p| matches!(p, Part::ToolCall { .. })) {
                    d.event_type = EventType::ToolCall;
                } else if has(|p| matches!(p, Part::Reasoning { .. })) {
                    d.event_type = EventType::Reasoning;
                }
            }
        }
        (_, "ERROR_MESSAGE") => {
            d.role = Role::System;
            d.event_type = EventType::Error;
            d.content = [s(o, "error"), content]
                .into_iter()
                .flatten()
                .map(Part::text)
                .collect();
            if d.content.is_empty() {
                d.content.push(opaque(ty, whole()));
            }
        }
        ("SYSTEM", _) => {
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            d.content = vec![content.map_or_else(|| opaque(ty, whole()), Part::text)];
        }
        // Output of a tool the planner ran (RUN_COMMAND, VIEW_FILE, GREP_SEARCH,
        // CODE_ACTION, GENERIC, ...). No call id exists in the source.
        ("MODEL", _) => {
            d.role = Role::Tool;
            d.event_type = EventType::ToolResult;
            d.content = vec![Part::ToolResult {
                tool_call_id: None,
                output: content.map_or(Value::Null, |t| json!(t)),
                is_error: o
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|c| c != 0),
            }];
            if let Some(Value::Array(media)) = o.get("media") {
                d.content.extend(media.iter().map(|m| {
                    match m.get("uri").and_then(Value::as_str) {
                        Some(uri) => Part::Image {
                            artifact: None,
                            mime: m
                                .get("mime_type")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            filename: None,
                            source_ref: Some(uri.to_string()),
                        },
                        None => opaque("media", m.clone()),
                    }
                }));
            }
        }
        _ => {
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![opaque(ty, whole())];
        }
    }
    Ok(d)
}

/// Why a file under `~/.gemini` that no parser claims is not a conversation
/// we can import: `(format label, reason)`. Path-based on purpose: these are
/// stores whose bytes either are not conversation content or cannot be
/// decoded faithfully, and the inventory must say which.
pub fn known_unsupported(probe: &Probe) -> Option<(&'static str, &'static str)> {
    let p = Path::new(&probe.full_path);
    let comps: Vec<&str> = p
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let at = comps.iter().position(|c| *c == ".gemini")?;
    let rest = &comps[at + 1..];
    let name = *rest.last()?;
    let under = |d: &str| rest.contains(&d);
    Some(match rest.first().copied()? {
        "antigravity-cli" if under("conversations") => (
            "antigravity-sqlite-protobuf",
            "SQLite trajectory store whose step/metadata columns are schemaless protobuf blobs with no published schema; decoding would be guessing. The readable transcript_full.jsonl of the same conversation is imported when it exists",
        ),
        "antigravity-cli" if name.starts_with("conversation_summaries.db") => (
            "antigravity-sqlite-protobuf",
            "derived conversation index (titles/previews) with a protobuf summary column; the conversations themselves are imported from their transcripts",
        ),
        "antigravity-cli" if name.ends_with(".pb") || name.ends_with(".pbtxt") => (
            "antigravity-protobuf",
            "protobuf state/annotation file with no published schema and no conversation messages",
        ),
        "antigravity-cli" if under("logs") && name == "transcript.jsonl" => (
            "antigravity-transcript-jsonl",
            "truncated duplicate of transcript_full.jsonl (long fields are cut); the full transcript is imported instead",
        ),
        "antigravity-cli" if under("chunks") => (
            "antigravity-transcript-jsonl",
            "chunked copy of transcript(_full).jsonl; every step is already imported from transcript_full.jsonl",
        ),
        "antigravity-cli" if name == "history.jsonl" => (
            "antigravity-prompt-history",
            "prompt-only index (display text, workspace, timestamp); user prompts are imported from the transcripts, so importing it would duplicate them",
        ),
        "antigravity-cli" if under("brain") => (
            "antigravity-agent-workfile",
            "file the agent created or read while working (code, logs, images, notes); not a conversation record",
        ),
        "antigravity-cli" if under("log") => (
            "antigravity-diagnostic-log",
            "CLI diagnostic log; not conversation content",
        ),
        "antigravity-cli" | "antigravity" | "config" | "history" => (
            "gemini-tool-state",
            "tool configuration, credentials, cache or runtime state; no conversation content",
        ),
        "tmp" if name == "logs.json" => (
            "gemini-cli-prompt-log",
            "prompt-only log (sessionId/messageId/message); user prompts are imported from the chat sessions, so importing it would duplicate them",
        ),
        "tmp" if name == ".project_root" => (
            "gemini-tool-state",
            "project path marker, used as the working directory of the sibling chat sessions",
        ),
        _ if rest.len() == 1 => (
            "gemini-tool-state",
            "tool configuration, credentials or runtime state; no conversation content",
        ),
        _ => return None,
    })
}
