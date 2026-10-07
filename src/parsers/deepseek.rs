//! DeepSeek DSH (DeepSeek desktop/CLI harness) session parser:
//! `~/.dsh/sessions/<workspace>/session-<uuid>/session.v4.jsonl.zstd`.
//!
//! The store compresses the log with zstd; discovery already expands `.zstd`
//! members, so this parser reads the plain `session.v4.jsonl` (and also accepts
//! one that was never compressed). Line 1 is a header
//! `{"type":"session","version":4,"id","createdAt","cwd","agentPreset",..}`;
//! every later line is an event `{"type","seq","time","data"}`.
//!
//! Mapping:
//! * `user/message`, `system/message`, `assistant/message`, `tool/result` are
//!   conversation events (assistant blocks `reasoning` / `text` / `tool-call`);
//! * `agent/inbox/spliced` carries the same user messages again under the same
//!   ids, so each one collapses onto its `user/message` on import;
//! * `session/title` becomes a system note;
//! * `tool/call` repeats a `tool-call` block of the assistant message and is
//!   skipped, as is pure runtime telemetry (step/turn markers, request headers
//!   with the tool catalogue, retries, delivery acks, permission/sandbox state);
//! * any event type not known here is kept as an opaque event, never dropped.
//!
//! The per-chunk `stream` and `replayState` of assistant messages are provider
//! replay data that duplicates the final `content` and is not retained.

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use super::pi::{copy, opaque, parts_of};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct DeepSeekParser;

const LOG: &str = "session.v4.jsonl";

/// Event types that are runtime telemetry or duplicates of imported events.
const SKIPPED: &[&str] = &[
    "tool/call",
    "step/start",
    "step/end",
    "turn/start",
    "turn/end",
    "request/header",
    "request/context",
    "session-log-deepseek/delivery-accepted",
    "assistant/attempt",
    "llm/retry",
    "llm/retry-started",
    "permission/preset",
    "sandbox/mode",
    "approval/policy",
    "workspace/changes",
    "model/selection",
    "session/end-seed",
    "session/title-llm-request",
];

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn is_header(o: &Map<String, Value>) -> bool {
    s(o, "type") == Some("session")
        && s(o, "id").is_some()
        && s(o, "cwd").is_some()
        && o.get("version").is_some_and(Value::is_number)
}

/// `session-<uuid>` directory of the log, when the header is unusable.
fn dir_id(path: &Path) -> Option<String> {
    path.ancestors()
        .filter_map(|a| a.file_name()?.to_str())
        .find_map(|n| n.strip_prefix("session-"))
        .map(str::to_string)
        .filter(|id| is_globally_unique(id))
}

impl SourceParser for DeepSeekParser {
    fn id(&self) -> &'static str {
        "deepseek"
    }
    fn provider(&self) -> &'static str {
        "deepseek"
    }
    fn application(&self) -> &'static str {
        "deepseek-dsh"
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
        "DeepSeek DSH session logs (~/.dsh/sessions/**/session.v4.jsonl[.zstd])"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir || probe.filename() != LOG {
            return Detection::none(self.id());
        }
        let records = probe_records(probe, 5);
        if !records.first().is_some_and(is_header) {
            return Detection::none(self.id());
        }
        let in_store = probe.has_component(".dsh") || probe.has_component("sessions");
        Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            "deepseek-dsh-session-v4",
            if in_store {
                Confidence::Certain
            } else {
                Confidence::Strong
            },
            "session header (version, id, cwd) followed by seq/time/data events",
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

        while let Some(line) = reader.next_line().context("read session file")? {
            if let Some(n) = line.oversize {
                tally.fail(
                    line.number,
                    format!("record of {n} bytes exceeds the record limit"),
                );
                continue;
            }
            let obj = match serde_json::from_slice::<Value>(reader.bytes()) {
                Ok(Value::Object(o)) => o,
                Ok(_) => {
                    tally.fail(line.number, "record is not a JSON object");
                    continue;
                }
                Err(e) => {
                    tally.fail(line.number, format!("invalid JSON: {e}"));
                    continue;
                }
            };
            if is_header(&obj) {
                if !stream.begun() {
                    stream.begin(meta(&obj))?;
                }
                // The header is conversation metadata, not an event.
                tally.skip("session header (applied to the conversation)");
                continue;
            }
            if !stream.begun() {
                // No header before the first event: name the conversation by
                // its directory, or leave it to a content fingerprint.
                let id =
                    dir_id(&source.read_path).or_else(|| dir_id(Path::new(&source.display_path)));
                stream.begin(ConversationMeta {
                    native_id: id.clone(),
                    native_session_id: id.clone(),
                    identity_hint: if id.is_some() {
                        IdentityHint::Native
                    } else {
                        IdentityHint::Fingerprint
                    },
                    ..Default::default()
                })?;
            }
            match convert(&obj, line.number) {
                Converted::Event(d) => stream.push(*d)?,
                Converted::Skip(kind) => tally.skip(&kind),
                Converted::Fail(why) => tally.fail(line.number, why),
            }
        }
        stream.finish(ConversationMeta::default())?;
        Ok(tally.into_report(&stream))
    }
}

fn meta(h: &Map<String, Value>) -> ConversationMeta {
    let id = s(h, "id")
        .filter(|id| is_globally_unique(id))
        .map(str::to_string);
    let mut metadata = Map::new();
    for (k, dst) in [
        ("version", "format_version"),
        ("agentPreset", "agent_preset"),
        ("delegationDepth", "delegation_depth"),
        ("isSeeded", "is_seeded"),
        ("parentSessionId", "parent_session"),
    ] {
        if let Some(v) = h.get(k).filter(|v| !v.is_null()) {
            metadata.insert(dst.into(), v.clone());
        }
    }
    ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        working_directory: s(h, "cwd").map(str::to_string),
        started_at: Some(stamp(h.get("createdAt"))).filter(|st| st.utc.is_some()),
        metadata,
        identity_hint: if id.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    }
}

enum Converted {
    Event(Box<EventDraft>),
    Skip(String),
    Fail(String),
}

fn convert(obj: &Map<String, Value>, line: u64) -> Converted {
    let Some(ty) = s(obj, "type") else {
        return Converted::Fail("record has no string `type`".into());
    };
    if SKIPPED.contains(&ty) {
        return Converted::Skip(ty.to_string());
    }
    let data = obj.get("data").and_then(Value::as_object);
    let empty = Map::new();
    let data = data.unwrap_or(&empty);
    let seq = obj.get("seq").and_then(Value::as_u64);
    let mut d = EventDraft {
        timestamp: stamp(obj.get("time")),
        // Events without a message id are still addressable by their seq.
        native_id: seq.map(|n| format!("seq-{n}")),
        ..Default::default()
    };
    d.metadata.insert("source_line".into(), json!(line));
    d.metadata.insert("record_type".into(), json!(ty));
    if let Some(n) = seq {
        d.metadata.insert("seq".into(), json!(n));
    }
    copy(&mut d, data, &[("turn", "turn"), ("step", "step")]);
    let result = match ty {
        "user/message" => message(&mut d, data, data, Role::User),
        "system/message" => message(&mut d, data, nested(data), Role::System),
        "assistant/message" => assistant(&mut d, data),
        "tool/result" => tool_result(&mut d, data),
        "agent/inbox/spliced" => splice(&mut d, data),
        "session/title" => {
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            d.content = vec![match s(data, "title") {
                Some(t) => Part::text(t),
                None => opaque(ty, Value::Object(obj.clone())),
            }];
            Ok(())
        }
        other => {
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![opaque(other, Value::Object(obj.clone()))];
            Ok(())
        }
    };
    if let Err(why) = result {
        return Converted::Fail(why);
    }
    for p in &d.content {
        match p {
            Part::ToolCall { id: Some(id), .. }
            | Part::ToolResult {
                tool_call_id: Some(id),
                ..
            } => d.tool_call_ids.push(id.clone()),
            _ => {}
        }
    }
    Converted::Event(Box::new(d))
}

fn nested(data: &Map<String, Value>) -> &Map<String, Value> {
    data.get("message")
        .and_then(Value::as_object)
        .unwrap_or(data)
}

/// A plain message: `msg` holds `id`, `content` and `source`.
fn message(
    d: &mut EventDraft,
    data: &Map<String, Value>,
    msg: &Map<String, Value>,
    role: Role,
) -> Result<(), String> {
    d.role = role;
    d.event_type = EventType::Message;
    if let Some(id) = s(msg, "id") {
        d.native_id = Some(id.to_string());
    }
    if let Some(src) = msg.get("source").or_else(|| data.get("source")) {
        d.metadata.insert("message_source".into(), src.clone());
    }
    d.content = blocks(msg.get("content"));
    Ok(())
}

/// One splice line is one event; with a single inserted message it takes that
/// message's id so it collapses onto the matching `user/message`.
fn splice(d: &mut EventDraft, data: &Map<String, Value>) -> Result<(), String> {
    let inserted: Vec<&Map<String, Value>> = data
        .get("inserted")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    d.role = Role::User;
    d.event_type = EventType::Message;
    if let [one] = inserted.as_slice() {
        if let Some(id) = s(one, "id") {
            d.native_id = Some(id.to_string());
        }
        if let Some(src) = one.get("source") {
            d.metadata.insert("message_source".into(), src.clone());
        }
    }
    d.content = inserted
        .iter()
        .flat_map(|m| blocks(m.get("content")))
        .collect();
    Ok(())
}

fn assistant(d: &mut EventDraft, data: &Map<String, Value>) -> Result<(), String> {
    let msg = data
        .get("message")
        .and_then(Value::as_object)
        .ok_or("assistant event has no message object")?;
    d.role = Role::Assistant;
    d.event_type = EventType::Message;
    if let Some(id) = s(msg, "id") {
        d.native_id = Some(id.to_string());
    }
    copy(d, data, &[("usage", "usage")]);
    if let Some(src) = msg.get("source").and_then(Value::as_object) {
        d.model = s(src, "model").map(str::to_string);
        copy(d, src, &[("provider", "provider")]);
    }
    d.content = blocks(msg.get("content"));
    let text = d.content.iter().any(|p| matches!(p, Part::Text { .. }));
    let calls = d.content.iter().any(|p| matches!(p, Part::ToolCall { .. }));
    if !text && calls {
        d.event_type = EventType::ToolCall;
    } else if !text
        && !d.content.is_empty()
        && d.content
            .iter()
            .all(|p| matches!(p, Part::Reasoning { .. }))
    {
        d.event_type = EventType::Reasoning;
    }
    Ok(())
}

fn tool_result(d: &mut EventDraft, data: &Map<String, Value>) -> Result<(), String> {
    let msg = data
        .get("message")
        .and_then(Value::as_object)
        .ok_or("tool result has no message object")?;
    d.role = Role::Tool;
    d.event_type = EventType::ToolResult;
    if let Some(id) = s(msg, "id") {
        d.native_id = Some(id.to_string());
    }
    d.content = vec![Part::ToolResult {
        tool_call_id: s(msg, "toolCallId").map(str::to_string),
        output: msg.get("content").cloned().unwrap_or(Value::Null),
        is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
    }];
    Ok(())
}

fn blocks(content: Option<&Value>) -> Vec<Part> {
    let Some(Value::Array(list)) = content else {
        return parts_of(content);
    };
    list.iter()
        .map(|b| {
            let Some(o) = b.as_object() else {
                return opaque("non_object_block", b.clone());
            };
            match s(o, "type").unwrap_or("untyped") {
                "text" => Part::text(s(o, "text").unwrap_or("")),
                "reasoning" => match s(o, "text") {
                    Some(t) => Part::Reasoning {
                        text: t.into(),
                        visibility: ReasoningVisibility::Public,
                    },
                    None => opaque("reasoning", b.clone()),
                },
                "tool-call" => Part::ToolCall {
                    id: s(o, "id").map(str::to_string),
                    name: s(o, "name").unwrap_or("").to_string(),
                    // Arguments are a JSON document in a string; keep the text if it is not.
                    arguments: match o.get("arguments") {
                        Some(Value::String(a)) => {
                            serde_json::from_str(a).unwrap_or_else(|_| Value::String(a.clone()))
                        }
                        Some(v) => v.clone(),
                        None => Value::Null,
                    },
                },
                other => opaque(other, b.clone()),
            }
        })
        .collect()
}

/// Files under a DSH store that are not conversation records: `(format, reason)`.
pub fn known_unsupported(probe: &Probe) -> Option<(&'static str, &'static str)> {
    let p = Path::new(&probe.full_path);
    let comps: Vec<&str> = p
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let at = comps.iter().position(|c| *c == ".dsh")?;
    let rest = &comps[at + 1..];
    Some(match rest.first().copied()? {
        "storages" => (
            "deepseek-dsh-metadata",
            "workspace/session projection cache (titles, token stats, settings per session); the conversations come from sessions/**/session.v4.jsonl",
        ),
        "sessions" => (
            "deepseek-dsh-session-other",
            "session directory file other than the session.v4 event log",
        ),
        "profiles" => (
            "deepseek-dsh-config",
            "application profile/package configuration; not conversation content",
        ),
        _ => (
            "deepseek-dsh-other",
            "DSH application file (identity/credentials/settings); not a conversation record",
        ),
    })
}
