//! Hermes Agent parser (NousResearch hermes-agent, `~/.hermes/**`): the SQLite
//! session store `state.db`, the per-session JSON logs
//! `sessions/session_<id>.json`, and gateway JSONL transcripts
//! `sessions/<id>.jsonl`.
//!
//! Format knowledge (the `sessions` / `messages` tables, OpenAI-style
//! `tool_calls`, compression summaries, inactive rows, reasoning columns)
//! follows xhluca/session-migrate (MIT licence,
//! <https://github.com/xhluca/session-migrate>, `docs/hermes-format.md` and
//! `formats/hermes.py`); the semantics were ported and adapted to convolith's
//! canonical model, not copied line by line.
//!
//! Mapping rules, in short:
//! * a `sessions` row / JSON log / JSONL file is one conversation; each
//!   `messages` row / message object / transcript line is one event;
//! * `assistant.tool_calls` become `ToolCall` parts, `tool` rows become
//!   `ToolResult` parts linked by `tool_call_id`;
//! * rows that Hermes no longer replays to the model (`active = 0`, compacted
//!   or rewound) are still imported, flagged `inactive` in metadata, because an
//!   archive must not lose history the source still holds;
//! * `reasoning` columns are emitted only when text is literally stored;
//! * timestamps are the source's (epoch seconds or ISO text) or unknown.

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use super::rows::{oversize_cells, query_maps};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use crate::sqlite::{is_sqlite_bytes, Db};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::io::{BufReader, Read};

pub struct HermesParser;

/// Largest JSON session log read whole; bigger ones are one failed record.
const MAX_JSON_LOG: u64 = 512 * 1024 * 1024;

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

/// Hermes session ids (`20260830_233734_3ff33e`) are unique in practice but too
/// short for the importer's global-uniqueness test; namespacing them gives a
/// deterministic native id that passes it. Anything still too weak is dropped
/// so the importer falls back to path/title coordinates.
fn native(id: &str) -> Option<String> {
    let n = format!("hermes-session-{id}");
    is_globally_unique(&n).then_some(n)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Db,
    JsonLog,
    Transcript,
}

fn classify(p: &Probe) -> Option<(Kind, Confidence, &'static str)> {
    let in_store = p.has_component(".hermes");
    let certain = |c: bool| {
        if c {
            Confidence::Certain
        } else {
            Confidence::Strong
        }
    };
    if is_sqlite_bytes(&p.head_bytes) {
        let h = p.head.replace(['`', '"', '[', ']'], "");
        if h.contains("CREATE TABLE sessions")
            && h.contains("CREATE TABLE messages")
            && h.contains("tool_calls")
        {
            return Some((
                Kind::Db,
                certain(in_store),
                "SQLite sessions/messages tables with tool_calls",
            ));
        }
        // Schema text past the probe window: only the location speaks for it.
        if in_store && p.filename() == "state.db" {
            return Some((
                Kind::Db,
                Confidence::Weak,
                "state.db under .hermes (schema not in probe window)",
            ));
        }
        return None;
    }
    match p.ext().as_str() {
        "json" => {
            let h: String = p.head.chars().filter(|c| !c.is_whitespace()).collect();
            (h.starts_with('{')
                && h.contains("\"session_id\":\"")
                && h.contains("\"session_start\":\"")
                && (h.contains("\"model\":") || h.contains("\"platform\":")))
            .then_some((
                Kind::JsonLog,
                certain(in_store),
                "session log with session_id/session_start",
            ))
        }
        "jsonl" => {
            let recs = probe_records(p, 20);
            if recs.first().is_some_and(|r| {
                s(r, "role") == Some("session_meta")
                    && ["tools", "model", "platform"]
                        .iter()
                        .any(|k| r.contains_key(*k))
            }) {
                return Some((
                    Kind::Transcript,
                    certain(in_store),
                    "transcript opening with a session_meta record",
                ));
            }
            let shaped = !recs.is_empty()
                && recs.iter().all(|r| {
                    s(r, "role").is_some() && r.contains_key("content") && !r.contains_key("type")
                });
            (in_store && shaped).then_some((
                Kind::Transcript,
                Confidence::Strong,
                "role/content transcript under .hermes",
            ))
        }
        _ => None,
    }
}

impl SourceParser for HermesParser {
    fn id(&self) -> &'static str {
        "hermes"
    }
    fn provider(&self) -> &'static str {
        "nousresearch"
    }
    fn application(&self) -> &'static str {
        "hermes-agent"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: false,
            reasoning: true,
            streaming: true,
            // Token counters, FTS tables, system prompts of JSON logs are not imported.
            partial: true,
        }
    }
    fn description(&self) -> &'static str {
        "Hermes Agent sessions (~/.hermes/state.db, sessions/*.json, sessions/*.jsonl)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir {
            return Detection::none(self.id());
        }
        let Some((kind, conf, reason)) = classify(probe) else {
            return Detection::none(self.id());
        };
        let format = match kind {
            Kind::Db => "hermes-sqlite",
            Kind::JsonLog => "hermes-session-json",
            Kind::Transcript => "hermes-transcript-jsonl",
        };
        let d = Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            format,
            conf,
            reason,
        );
        if probe.has_component(".hermes") {
            d.with_reason("located under .hermes")
        } else {
            d
        }
    }

    fn parse(
        &self,
        ctx: &mut dyn ParseContext,
        source: &Source,
        sink: &mut dyn EventSink,
    ) -> Result<ParseReport> {
        let mut magic = [0u8; 16];
        let n = std::fs::File::open(&source.read_path)
            .and_then(|mut f| f.read(&mut magic))
            .with_context(|| format!("open {}", source.read_path.display()))?;
        if is_sqlite_bytes(&magic[..n]) {
            return parse_db(ctx, source, sink);
        }
        let ext = source
            .read_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if ext.eq_ignore_ascii_case("jsonl") {
            parse_transcript(ctx, source, sink)
        } else {
            parse_json_log(ctx, source, sink)
        }
    }
}

// ------------------------------------------------------------ SQLite

fn parse_db(
    ctx: &mut dyn ParseContext,
    source: &Source,
    sink: &mut dyn EventSink,
) -> Result<ParseReport> {
    let db = Db::open_snapshot(&source.read_path, ctx.staging_dir())?;
    for (table, cols) in [
        ("sessions", &["id"][..]),
        ("messages", &["id", "session_id", "role", "content"][..]),
    ] {
        if !db.table_exists(table) {
            return Err(anyhow!("not a Hermes database: table `{table}` is missing"));
        }
        let have = db.columns(table)?;
        if let Some(c) = cols.iter().find(|c| !have.iter().any(|h| h == *c)) {
            return Err(anyhow!("not a Hermes database: `{table}.{c}` is missing"));
        }
    }
    let max = ctx.max_record_bytes();
    let mut tally = Tally::default();
    let (mut conversations, mut events, mut calls) = (0, 0, 0);
    let mut sessions = Vec::new();
    if let Err(e) = query_maps(
        &db.conn,
        "select * from sessions order by started_at, id",
        &[],
        max,
        |r| {
            sessions.push(r);
            Ok(())
        },
    )
    // `started_at` is absent in very old schemas.
    .or_else(|_| {
        query_maps(
            &db.conn,
            "select * from sessions order by id",
            &[],
            max,
            |r| {
                sessions.push(r);
                Ok(())
            },
        )
    }) {
        tally.fail_at("sessions table", format!("{e:#}"));
    }
    for row in sessions {
        let sid = s(&row, "id").unwrap_or("").to_string();
        let at = format!("session {sid}");
        if !oversize_cells(&row).is_empty() {
            tally.fail_at(&at, "session row has an oversize cell");
            continue;
        }
        let meta = db_session_meta(&row);
        let mut stream = Stream::new(sink);
        let r = query_maps(
            &db.conn,
            "select * from messages where session_id = ?1 order by id",
            &[&sid],
            max,
            |m| {
                let at = format!(
                    "message row {}",
                    m.get("id").map_or("?".into(), |v| v.to_string())
                );
                if !oversize_cells(&m).is_empty() {
                    tally.fail_at(&at, "message row has an oversize cell");
                    return Ok(());
                }
                match convert(&m) {
                    Ok(d) => {
                        if !stream.begun() {
                            stream.begin(meta.clone())?;
                            if let Some(sp) = s(&row, "system_prompt") {
                                stream.push(system_prompt_event(sp))?;
                            }
                        }
                        stream.push(d)?;
                    }
                    Err(why) => tally.fail_at(at, why),
                }
                Ok(())
            },
        );
        if let Err(e) = r {
            tally.fail_at(&at, format!("reading messages: {e:#}"));
        }
        stream.finish(meta)?;
        conversations += stream.conversations;
        events += stream.events;
        calls += stream.tool_calls;
    }
    let mut report = tally.into_totals(conversations, events, calls);
    report
        .notes
        .push(format!("sqlite: {}", db.recovery.describe()));
    Ok(report)
}

fn system_prompt_event(text: &str) -> EventDraft {
    let mut d =
        EventDraft::with_content(Role::System, EventType::SystemNote, vec![Part::text(text)]);
    d.metadata
        .insert("record_type".into(), json!("system_prompt"));
    d
}

fn db_session_meta(row: &Map<String, Value>) -> ConversationMeta {
    let id = s(row, "id").and_then(native);
    let time = |k: &str| {
        let st = stamp(row.get(k));
        st.utc.is_some().then_some(st)
    };
    let mut metadata = Map::new();
    for (src, dst) in [
        ("source", "source"),
        ("parent_session_id", "parent_session_id"),
        ("end_reason", "end_reason"),
        ("billing_provider", "billing_provider"),
        ("message_count", "message_count"),
        ("tool_call_count", "tool_call_count"),
    ] {
        if let Some(v) = row.get(src).filter(|v| !v.is_null()) {
            metadata.insert(dst.into(), v.clone());
        }
    }
    ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        title: s(row, "title").map(str::to_string),
        model: s(row, "model").map(str::to_string),
        working_directory: s(row, "cwd").map(str::to_string),
        started_at: time("started_at"),
        ended_at: time("ended_at"),
        metadata,
        identity_hint: hint(&id),
        ..Default::default()
    }
}

fn hint(id: &Option<String>) -> IdentityHint {
    if id.is_some() {
        IdentityHint::Native
    } else {
        IdentityHint::Fingerprint
    }
}

// ------------------------------------------------------------ JSON log

fn parse_json_log(
    ctx: &mut dyn ParseContext,
    source: &Source,
    sink: &mut dyn EventSink,
) -> Result<ParseReport> {
    let mut tally = Tally::default();
    let limit = ctx.max_file_bytes().min(MAX_JSON_LOG);
    let mut buf = Vec::new();
    std::fs::File::open(&source.read_path)
        .with_context(|| format!("open {}", source.read_path.display()))?
        .take(limit + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        tally.fail_at(
            "file",
            format!("session log exceeds the {limit}-byte limit"),
        );
        return Ok(tally.into_totals(0, 0, 0));
    }
    let doc = match serde_json::from_slice::<Value>(
        buf.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&buf),
    ) {
        Ok(Value::Object(o)) => o,
        Ok(_) => {
            tally.fail_at("file", "document is not a JSON object");
            return Ok(tally.into_totals(0, 0, 0));
        }
        Err(e) => {
            tally.fail_at("file", format!("invalid JSON: {e}"));
            return Ok(tally.into_totals(0, 0, 0));
        }
    };
    drop(buf);
    let stem_id = source
        .read_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_prefix("session_").unwrap_or(s).to_string());
    let id = s(&doc, "session_id")
        .map(str::to_string)
        .or(stem_id)
        .as_deref()
        .and_then(native);
    let mut metadata = Map::new();
    for k in ["platform", "base_url", "message_count"] {
        if let Some(v) = doc.get(k).filter(|v| !v.is_null()) {
            metadata.insert(k.into(), v.clone());
        }
    }
    let time = |k: &str| {
        let st = stamp(doc.get(k));
        st.utc.is_some().then_some(st)
    };
    // Hermes writes naive local ISO text here; it is not UTC and is not guessed
    // at, so a stamp without a usable instant is kept as the literal instead.
    for k in ["session_start", "last_updated"] {
        if time(k).is_none() {
            if let Some(v) = doc.get(k).filter(|v| !v.is_null()) {
                metadata.insert(format!("{k}_original"), v.clone());
            }
        }
    }
    let meta = ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        model: s(&doc, "model").map(str::to_string),
        started_at: time("session_start"),
        ended_at: time("last_updated"),
        metadata,
        identity_hint: hint(&id),
        ..Default::default()
    };
    let mut stream = Stream::new(sink);
    let Some(messages) = doc.get("messages").and_then(Value::as_array) else {
        tally.fail_at("file", "session log has no messages array");
        return Ok(tally.into_totals(0, 0, 0));
    };
    for (i, m) in messages.iter().enumerate() {
        let at = format!("message #{i}");
        let Some(obj) = m.as_object() else {
            tally.fail_at(at, "message is not a JSON object");
            continue;
        };
        if s(obj, "role") == Some("session_meta") {
            tally.skip("session_meta");
            continue;
        }
        match convert(obj) {
            Ok(mut d) => {
                if !stream.begun() {
                    stream.begin(meta.clone())?;
                    if let Some(sp) = s(&doc, "system_prompt") {
                        stream.push(system_prompt_event(sp))?;
                    }
                }
                d.metadata.insert("source_index".into(), json!(i));
                stream.push(d)?;
            }
            Err(why) => tally.fail_at(at, why),
        }
    }
    stream.finish(meta)?;
    Ok(tally.into_report(&stream))
}

// ------------------------------------------------------------ JSONL transcript

fn parse_transcript(
    ctx: &mut dyn ParseContext,
    source: &Source,
    sink: &mut dyn EventSink,
) -> Result<ParseReport> {
    let file = std::fs::File::open(&source.read_path)
        .with_context(|| format!("open {}", source.read_path.display()))?;
    let mut reader = LineReader::new(BufReader::new(file), ctx.max_record_bytes());
    let mut tally = Tally::default();
    let mut stream = Stream::new(sink);
    let id = source
        .read_path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(native);
    let mut meta = ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        identity_hint: hint(&id),
        ..Default::default()
    };
    while let Some(line) = reader.next_line().context("read transcript")? {
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
        if s(&obj, "role") == Some("session_meta") {
            // Applied to the conversation header; the tool schema list is not imported.
            if !stream.begun() {
                meta.model = s(&obj, "model").map(str::to_string);
                if let Some(p) = obj.get("platform").filter(|v| !v.is_null()) {
                    meta.metadata.insert("platform".into(), p.clone());
                }
                let st = stamp(obj.get("timestamp"));
                meta.started_at = st.utc.is_some().then_some(st);
            }
            tally.skip("session_meta (applied to conversation metadata)");
            continue;
        }
        match convert(&obj) {
            Ok(mut d) => {
                d.metadata.insert("source_line".into(), json!(line.number));
                if !stream.begun() {
                    stream.begin(meta.clone())?;
                }
                stream.push(d)?;
            }
            Err(why) => tally.fail(line.number, why),
        }
    }
    stream.finish(meta)?;
    Ok(tally.into_report(&stream))
}

// ------------------------------------------------------------ conversion

/// `content` / `tool_calls` may arrive as a JSON array or as text holding one.
fn decoded(v: Option<&Value>) -> Option<Value> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) if t.trim_start().starts_with('[') => {
            Some(serde_json::from_str::<Value>(t).unwrap_or_else(|_| Value::String(t.clone())))
        }
        Some(other) => Some(other.clone()),
    }
}

fn opaque(kind: &str, raw: Value) -> Part {
    Part::Opaque {
        kind: kind.to_string(),
        note: None,
        raw: Some(raw),
    }
}

fn content_parts(content: Option<Value>) -> Vec<Part> {
    match content {
        None => Vec::new(),
        Some(Value::String(t)) if t.is_empty() => Vec::new(),
        Some(Value::String(t)) => vec![Part::text(t)],
        Some(Value::Array(blocks)) => blocks
            .into_iter()
            .map(
                |b| match b.as_object().and_then(|o| s(o, "type").map(str::to_string)) {
                    Some(t) if t == "text" || t == "input_text" => {
                        Part::text(b.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    Some(t) if t == "image_url" || t == "input_image" => {
                        let url = b
                            .get("image_url")
                            .and_then(|i| i.get("url").unwrap_or(i).as_str())
                            .unwrap_or("");
                        if url.starts_with("http://") || url.starts_with("https://") {
                            Part::Image {
                                artifact: None,
                                mime: None,
                                filename: None,
                                source_ref: Some(url.to_string()),
                            }
                        } else {
                            opaque("image_url", b)
                        }
                    }
                    Some(t) => opaque(&t, b),
                    None => opaque("non_object_block", b),
                },
            )
            .collect(),
        Some(other) => vec![opaque("content", other)],
    }
}

fn truthy_flag(m: &Map<String, Value>, k: &str) -> Option<bool> {
    match m.get(k)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

/// One `messages` row, JSON-log message or transcript line.
fn convert(m: &Map<String, Value>) -> Result<EventDraft, String> {
    let role_name = s(m, "role").ok_or("message has no role")?;
    let mut d = EventDraft {
        role: Role::parse(role_name),
        timestamp: stamp(m.get("timestamp")),
        native_id: match m.get("id") {
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::String(t)) if !t.is_empty() => Some(t.clone()),
            _ => None,
        },
        ..Default::default()
    };
    d.metadata.insert("record_type".into(), json!("message"));
    d.metadata.insert("source_role".into(), json!(role_name));
    for k in [
        "finish_reason",
        "token_count",
        "tool_name",
        "observed",
        "display_kind",
        "model",
    ] {
        if let Some(v) = m.get(k).filter(|v| !v.is_null()) {
            d.metadata.insert(k.into(), v.clone());
        }
    }
    if truthy_flag(m, "active") == Some(false) {
        d.metadata.insert("inactive".into(), json!(true));
        if truthy_flag(m, "compacted") == Some(true) {
            d.metadata.insert("compacted".into(), json!(true));
        }
    }
    let content = decoded(m.get("content"));
    if truthy_flag(m, "_compressed_summary") == Some(true) {
        d.role = Role::System;
        d.event_type = EventType::Compaction;
        d.content = content_parts(content);
        return Ok(d);
    }
    let call_id = s(m, "tool_call_id").map(str::to_string);
    if role_name == "tool" {
        d.role = Role::Tool;
        d.event_type = EventType::ToolResult;
        let output = content.unwrap_or(Value::Null);
        d.content = vec![Part::ToolResult {
            is_error: tool_failed(&output),
            tool_call_id: call_id.clone(),
            output,
        }];
        d.tool_call_ids.extend(call_id);
        return Ok(d);
    }
    for k in ["reasoning", "reasoning_content"] {
        if let Some(t) = s(m, k) {
            let dup = d
                .content
                .iter()
                .any(|p| matches!(p, Part::Reasoning { text, .. } if text == t));
            if !dup {
                d.content.push(Part::Reasoning {
                    text: t.into(),
                    visibility: ReasoningVisibility::Public,
                });
            }
        }
    }
    if let Some(details) = decoded(m.get("reasoning_details")).filter(|v| {
        !matches!(v, Value::Null)
            && v.as_str() != Some("")
            && v.as_array().map_or(true, |a| !a.is_empty())
    }) {
        d.content.push(opaque("reasoning_details", details));
    }
    d.content.extend(content_parts(content));
    if let Some(calls) = decoded(m.get("tool_calls")) {
        let Value::Array(calls) = calls else {
            return Err("tool_calls is not an array".into());
        };
        for c in calls {
            let f = c.get("function").and_then(Value::as_object);
            let args = f
                .and_then(|f| f.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            let id = c
                .get("id")
                .and_then(Value::as_str)
                .filter(|i| !i.is_empty())
                .map(str::to_string);
            d.tool_call_ids.extend(id.clone());
            d.content.push(Part::ToolCall {
                id,
                name: f.and_then(|f| s(f, "name")).unwrap_or("").to_string(),
                arguments: match args {
                    // Hermes stores arguments as JSON text; keep the text when it is not JSON.
                    Value::String(t) => serde_json::from_str(&t).unwrap_or(Value::String(t)),
                    other => other,
                },
            });
        }
    }
    if d.role == Role::Assistant {
        let text = d.content.iter().any(|p| matches!(p, Part::Text { .. }));
        if !text && d.content.iter().any(|p| matches!(p, Part::ToolCall { .. })) {
            d.event_type = EventType::ToolCall;
        } else if !text
            && !d.content.is_empty()
            && d.content
                .iter()
                .all(|p| matches!(p, Part::Reasoning { .. } | Part::Opaque { .. }))
        {
            d.event_type = EventType::Reasoning;
        }
    }
    Ok(d)
}

/// Terminal-style results are stored as `{"output", "exit_code", "error"}`;
/// an error is only reported when that envelope says so.
fn tool_failed(output: &Value) -> bool {
    let Some(text) = output.as_str() else {
        return false;
    };
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let err = o
        .get("error")
        .is_some_and(|e| !e.is_null() && e.as_str() != Some(""));
    let code = o
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|c| c != 0);
    err || code
}
