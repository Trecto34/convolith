//! Gemini CLI session parser (`~/.gemini/tmp/<project>/chats/session-*.jsonl`
//! and the older single-document `session-*.json`).
//!
//! The JSONL form is a `{sessionId, projectHash, startTime, lastUpdated,
//! kind}` header line, then one record per message (`user`, `gemini`, `info`,
//! `warning`, `error`) and `{"$set": {...}}` patches that bump the header.
//! Gemini CLI rewrites a message when its tool calls complete, so the same
//! `id` can appear on several lines: the last one wins and keeps the position
//! of the first. The `.json` form is the same header with a `messages` array.
//!
//! Mapping, in short:
//! * a `gemini` message is one assistant event (thoughts, text, tool calls);
//!   each tool call that carries a `result` adds a `tool` event with the output;
//! * `thoughts` are provider summaries (`subject` + `description`);
//! * `tokens` is kept verbatim as `usage`, unknown keys under `unmapped`;
//! * the working directory comes from the sibling `.project_root` file the CLI
//!   writes next to `chats/`, when present.

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};

pub struct GeminiCliParser;

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn is_header(o: &Map<String, Value>) -> bool {
    s(o, "sessionId").is_some() && s(o, "projectHash").is_some() && s(o, "startTime").is_some()
}

impl SourceParser for GeminiCliParser {
    fn id(&self) -> &'static str {
        "gemini_cli"
    }
    fn provider(&self) -> &'static str {
        "google"
    }
    fn application(&self) -> &'static str {
        "gemini-cli"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: false,
            reasoning: true,
            streaming: false,
            partial: false,
        }
    }
    fn description(&self) -> &'static str {
        "Gemini CLI chat sessions (~/.gemini/tmp/*/chats/session-*.jsonl, legacy .json)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir {
            return Detection::none(self.id());
        }
        let (format, reason) = match probe.ext().as_str() {
            "jsonl" if probe_records(probe, 3).first().is_some_and(is_header) => (
                "gemini-cli-jsonl",
                "header line with sessionId/projectHash/startTime",
            ),
            // The probe window may end mid-document; the three header keys plus
            // `messages` together are specific enough.
            "json"
                if probe.head.trim_start().starts_with('{')
                    && ["\"sessionId\"", "\"projectHash\"", "\"messages\""]
                        .iter()
                        .all(|k| probe.head.contains(k)) =>
            {
                (
                    "gemini-cli-json",
                    "document with sessionId/projectHash/messages",
                )
            }
            _ => return Detection::none(self.id()),
        };
        let in_store = probe.has_component("chats") && probe.filename().starts_with("session-");
        Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            format,
            if in_store {
                Confidence::Certain
            } else {
                Confidence::Strong
            },
            reason,
        )
    }

    fn parse(
        &self,
        ctx: &mut dyn ParseContext,
        source: &Source,
        sink: &mut dyn EventSink,
    ) -> Result<ParseReport> {
        let mut tally = Tally::default();
        let mut head = Map::new();
        let mut slots: Vec<Option<(u64, Map<String, Value>)>> = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut set_extra = Map::new();

        // ponytail: whole session buffered (ids can repeat, last wins); fine
        // for chat-sized files, stream per id if sessions ever reach GBs.
        let is_json = source.read_path.extension().is_some_and(|e| e == "json");
        let records: Vec<(u64, Value)> = if is_json {
            if source.size > ctx.max_file_bytes() {
                bail!("{} bytes exceeds the file size limit", source.size);
            }
            let mut text = String::new();
            File::open(&source.read_path)
                .and_then(|mut f| f.read_to_string(&mut text))
                .with_context(|| format!("read {}", source.read_path.display()))?;
            let Value::Object(mut doc) = serde_json::from_str::<Value>(&text)
                .with_context(|| format!("parse {}", source.read_path.display()))?
            else {
                bail!("top level is not a JSON object");
            };
            let Some(Value::Array(msgs)) = doc.remove("messages") else {
                bail!("document has no `messages` array");
            };
            head = doc;
            msgs.into_iter()
                .enumerate()
                .map(|(i, m)| (i as u64 + 1, m))
                .collect()
        } else {
            let file = File::open(&source.read_path)
                .with_context(|| format!("open {}", source.read_path.display()))?;
            let mut reader = LineReader::new(BufReader::new(file), ctx.max_record_bytes());
            let mut out = Vec::new();
            while let Some(line) = reader.next_line().context("read session file")? {
                if let Some(n) = line.oversize {
                    tally.fail(
                        line.number,
                        format!("record of {n} bytes exceeds the record limit"),
                    );
                    continue;
                }
                match serde_json::from_slice::<Value>(reader.bytes()) {
                    Ok(v) => out.push((line.number, v)),
                    Err(e) => tally.fail(line.number, format!("invalid JSON: {e}")),
                }
            }
            out
        };

        for (n, v) in records {
            let Value::Object(o) = v else {
                tally.fail(n, "record is not a JSON object");
                continue;
            };
            if let Some(Value::Object(set)) = o.get("$set") {
                for (k, v) in set {
                    if k == "lastUpdated" {
                        head.insert(k.clone(), v.clone());
                    } else {
                        set_extra.insert(k.clone(), v.clone());
                    }
                }
                tally.skip("$set header update (applied to the conversation)");
                continue;
            }
            if is_header(&o) && !o.contains_key("type") {
                if head.is_empty() {
                    head = o;
                } else {
                    tally.skip("repeated session header");
                }
                continue;
            }
            match s(&o, "id").map(str::to_string) {
                Some(id) => match by_id.get(&id) {
                    Some(&i) => {
                        tally.skip("message superseded by a later rewrite of the same id");
                        slots[i] = Some((n, o));
                    }
                    None => {
                        by_id.insert(id, slots.len());
                        slots.push(Some((n, o)));
                    }
                },
                None => slots.push(Some((n, o))),
            }
        }

        let mut stream = Stream::new(sink);
        let meta = meta_of(&head, set_extra, source, is_json);
        stream.begin(meta.clone())?;
        for (n, o) in slots.into_iter().flatten() {
            match convert(&o, n) {
                Ok(drafts) => {
                    for d in drafts {
                        stream.push(d)?;
                    }
                }
                Err(why) => tally.fail(n, why),
            }
        }
        stream.finish(meta)?;
        Ok(tally.into_report(&stream))
    }
}

fn meta_of(
    head: &Map<String, Value>,
    set_extra: Map<String, Value>,
    source: &Source,
    is_json: bool,
) -> ConversationMeta {
    let id = s(head, "sessionId")
        .filter(|id| is_globally_unique(id))
        .map(str::to_string);
    let mut metadata = Map::new();
    metadata.insert(
        "format".into(),
        json!(if is_json { "json" } else { "jsonl" }),
    );
    for (src, dst) in [
        ("projectHash", "project_hash"),
        ("kind", "kind"),
        ("lastUpdated", "last_updated"),
    ] {
        if let Some(v) = head.get(src).filter(|v| !v.is_null()) {
            metadata.insert(dst.into(), v.clone());
        }
    }
    let unmapped: Map<String, Value> = head
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "sessionId" | "projectHash" | "startTime" | "lastUpdated" | "kind"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .chain(set_extra)
        .collect();
    if !unmapped.is_empty() {
        metadata.insert("unmapped".into(), Value::Object(unmapped));
    }
    let utc = |k: &str| Some(stamp(head.get(k))).filter(|st| st.utc.is_some());
    ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        working_directory: project_root(source),
        started_at: utc("startTime"),
        ended_at: utc("lastUpdated"),
        metadata,
        identity_hint: if id.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    }
}

/// `<project>/.project_root` next to `<project>/chats/`, read-only and bounded.
fn project_root(source: &Source) -> Option<String> {
    let chats = source.read_path.parent()?;
    if chats.file_name()? != "chats" {
        return None;
    }
    let mut buf = Vec::new();
    File::open(chats.parent()?.join(".project_root"))
        .ok()?
        .take(4096)
        .read_to_end(&mut buf)
        .ok()?;
    let p = String::from_utf8(buf).ok()?.trim().to_string();
    (!p.is_empty()).then_some(p)
}

fn opaque(kind: &str, raw: Value) -> Part {
    Part::Opaque {
        kind: kind.to_string(),
        note: None,
        raw: Some(raw),
    }
}

fn convert(o: &Map<String, Value>, line: u64) -> Result<Vec<EventDraft>, String> {
    let ty = s(o, "type").ok_or("record has no string `type`")?;
    let mut d = EventDraft {
        timestamp: stamp(o.get("timestamp")),
        native_id: s(o, "id").map(str::to_string),
        ..Default::default()
    };
    d.metadata.insert("source_line".into(), json!(line));
    d.metadata.insert("record_type".into(), json!(ty));
    let known = [
        "id",
        "timestamp",
        "type",
        "content",
        "thoughts",
        "tokens",
        "model",
        "toolCalls",
    ];
    let unmapped: Map<String, Value> = o
        .iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !unmapped.is_empty() {
        d.metadata
            .insert("unmapped".into(), Value::Object(unmapped));
    }
    let mut results = Vec::new();
    match ty {
        "user" => {
            d.role = Role::User;
            d.content = content_parts(o.get("content"));
        }
        "gemini" => {
            d.role = Role::Assistant;
            d.model = s(o, "model").map(str::to_string);
            if let Some(t) = o.get("tokens").filter(|v| !v.is_null()) {
                d.metadata.insert("usage".into(), t.clone());
            }
            match o.get("thoughts") {
                Some(Value::Array(ts)) => d.content.extend(ts.iter().map(thought)),
                Some(Value::Null) | None => {}
                Some(other) => d.content.push(opaque("thoughts", other.clone())),
            }
            d.content.extend(content_parts(o.get("content")));
            let mut unanswered = Vec::new();
            match o.get("toolCalls") {
                Some(Value::Array(calls)) => {
                    for c in calls {
                        tool_call(&mut d, c, line, &mut results, &mut unanswered);
                    }
                }
                Some(Value::Null) | None => {}
                Some(other) => d.content.push(opaque("toolCalls", other.clone())),
            }
            if !unanswered.is_empty() {
                d.metadata
                    .insert("tool_calls_without_result".into(), json!(unanswered));
            }
            let has = |f: fn(&Part) -> bool| d.content.iter().any(f);
            let text = has(|p| matches!(p, Part::Text { .. }));
            let calls = has(|p| matches!(p, Part::ToolCall { .. }));
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
        }
        "info" | "warning" | "error" => {
            d.role = Role::System;
            d.event_type = if ty == "error" {
                EventType::Error
            } else {
                EventType::SystemNote
            };
            d.content = content_parts(o.get("content"));
        }
        other => {
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![opaque(other, Value::Object(o.clone()))];
        }
    }
    for p in &d.content {
        if let Part::ToolCall { id: Some(id), .. } = p {
            d.tool_call_ids.push(id.clone());
        }
    }
    let mut out = vec![d];
    out.extend(results);
    Ok(out)
}

fn content_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(t)) if t.is_empty() => Vec::new(),
        Some(Value::String(t)) => vec![Part::text(t.as_str())],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| match b.get("text").and_then(Value::as_str) {
                Some(t) if b.as_object().is_some_and(|o| o.len() == 1) => Part::text(t),
                _ => opaque("content_part", b.clone()),
            })
            .collect(),
        Some(other) => vec![opaque("content", other.clone())],
    }
}

fn thought(t: &Value) -> Part {
    let (Some(subject), description) = (
        t.get("subject").and_then(Value::as_str),
        t.get("description").and_then(Value::as_str),
    ) else {
        return match t.get("description").and_then(Value::as_str) {
            Some(d) => Part::Reasoning {
                text: d.to_string(),
                visibility: ReasoningVisibility::Summary,
            },
            None => opaque("thought", t.clone()),
        };
    };
    Part::Reasoning {
        text: match description {
            Some(d) => format!("{subject}\n\n{d}"),
            None => subject.to_string(),
        },
        visibility: ReasoningVisibility::Summary,
    }
}

fn tool_call(
    d: &mut EventDraft,
    c: &Value,
    line: u64,
    results: &mut Vec<EventDraft>,
    unanswered: &mut Vec<Value>,
) {
    let Some(o) = c.as_object().filter(|o| s(o, "name").is_some()) else {
        d.content.push(opaque("toolCall", c.clone()));
        return;
    };
    let id = s(o, "id").map(str::to_string);
    d.content.push(Part::ToolCall {
        id: id.clone(),
        name: s(o, "name").unwrap_or_default().to_string(),
        arguments: o.get("args").cloned().unwrap_or(Value::Null),
    });
    let Some(result) = o.get("result").filter(|r| !r.is_null()) else {
        unanswered.push(json!(id));
        return;
    };
    // `result` is a list of Gemini `functionResponse` parts.
    let responses: Vec<&Value> = result
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get("functionResponse")?.get("response"))
                .collect()
        })
        .unwrap_or_default();
    let (output, mut is_error) = match responses.as_slice() {
        [r] => match (r.get("output"), r.get("error")) {
            (Some(out), None) => (out.clone(), false),
            (None, Some(err)) => (err.clone(), true),
            _ => ((*r).clone(), false),
        },
        _ => (result.clone(), false),
    };
    is_error |= s(o, "status") == Some("error");
    let mut r = EventDraft {
        role: Role::Tool,
        event_type: EventType::ToolResult,
        timestamp: stamp(o.get("timestamp")),
        native_id: d
            .native_id
            .as_ref()
            .zip(id.as_ref())
            .map(|(m, c)| format!("{m}/{c}")),
        parent_native_id: d.native_id.clone(),
        content: vec![Part::ToolResult {
            tool_call_id: id.clone(),
            output,
            is_error,
        }],
        tool_call_ids: id.into_iter().collect(),
        ..Default::default()
    };
    r.metadata.insert("source_line".into(), json!(line));
    for (src, dst) in [
        ("name", "tool_name"),
        ("status", "status"),
        ("displayName", "display_name"),
        ("description", "description"),
        ("resultDisplay", "result_display"),
        ("renderOutputAsMarkdown", "render_output_as_markdown"),
    ] {
        if let Some(v) = o.get(src).filter(|v| !v.is_null()) {
            r.metadata.insert(dst.into(), v.clone());
        }
    }
    let unmapped: Map<String, Value> = o
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "id" | "name"
                    | "args"
                    | "result"
                    | "status"
                    | "timestamp"
                    | "displayName"
                    | "description"
                    | "resultDisplay"
                    | "renderOutputAsMarkdown"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !unmapped.is_empty() {
        r.metadata
            .insert("unmapped".into(), Value::Object(unmapped));
    }
    results.push(r);
}
