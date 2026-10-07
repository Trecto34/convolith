//! Codex session parser: JSONL rollouts under `~/.codex/sessions/**`.
//!
//! Format knowledge (the `session_meta` / `response_item` / `event_msg` /
//! `turn_context` envelope, response-item kinds, the older bare-item rollout
//! layout) follows xhluca/session-migrate (MIT licence,
//! <https://github.com/xhluca/session-migrate>); the semantics were ported and
//! adapted to convolith's canonical model, not copied line by line.
//!
//! Mapping rules, in short:
//! * `response_item` is the canonical content; `event_msg` echoes of the same
//!   messages/reasoning are counted as skipped, other `event_msg` kinds that
//!   carry data of their own (`turn_aborted`, `error`, completed command / file
//!   change items) are kept;
//! * `turn_context` is folded into the model / cwd of the events that follow;
//! * call and output of one tool call share a `call_id`, so native ids are
//!   `<call_id>:call` and `<call_id>:output`;
//! * encrypted reasoning is kept as an opaque part, never decoded;
//! * a timestamp is taken from the record or left unknown, never invented.

use super::jsonl::{probe_records, stamp, str_of, LineReader, Stream, Tally};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::BufReader;

pub struct CodexParser;

/// High-volume bookkeeping records with no conversational content.
const NOISE: &[&str] = &["token_usage_record", "world_state"];
/// `event_msg` kinds that repeat a `response_item`.
const ECHO_EVENTS: &[&str] = &[
    "user_message",
    "agent_message",
    "agent_reasoning",
    "agent_reasoning_raw_content",
];
/// `item_completed` item kinds that repeat a `response_item`.
const ECHO_ITEMS: &[&str] = &["UserMessage", "AgentMessage", "Reasoning"];
/// Item kinds of the older rollout layout, where items are bare top-level records.
const LEGACY_ITEMS: &[&str] = &[
    "message",
    "reasoning",
    "function_call",
    "function_call_output",
    "local_shell_call",
    "custom_tool_call",
    "custom_tool_call_output",
    "web_search_call",
];

impl SourceParser for CodexParser {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn provider(&self) -> &'static str {
        "openai"
    }
    fn application(&self) -> &'static str {
        "codex"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: false,
            reasoning: true,
            streaming: true,
            // token/world-state records and message echoes are not imported.
            partial: true,
        }
    }
    fn description(&self) -> &'static str {
        "Codex JSONL rollouts (~/.codex/sessions/**)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir || probe.ext() != "jsonl" {
            return Detection::none(self.id());
        }
        let own_store = probe.has_component(".codex") || probe.filename().starts_with("rollout-");
        let records = probe_records(probe, 20);
        let reason = match records.iter().find_map(evidence) {
            Some(r) => r,
            // Real `session_meta` records embed the full base instructions and
            // routinely overflow the probe window.
            None if records.is_empty() && {
                let compact: String = probe.head.split_whitespace().collect();
                compact.starts_with('{')
                    && compact.contains("\"type\":\"session_meta\"")
                    && compact.contains("\"payload\":{")
            } =>
            {
                "oversized session_meta record"
            }
            None => return Detection::none(self.id()),
        };
        let conf = if own_store {
            Confidence::Certain
        } else {
            Confidence::Strong
        };
        let d = Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            "codex-rollout-jsonl",
            conf,
            reason,
        );
        if own_store {
            d.with_reason("located in a .codex store or named rollout-*")
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
        let file = File::open(&source.read_path)
            .with_context(|| format!("open {}", source.read_path.display()))?;
        let mut reader = LineReader::new(BufReader::new(file), ctx.max_record_bytes());
        let mut tally = Tally::default();
        let mut stream = Stream::new(sink);
        let mut st = State::default();

        while let Some(line) = reader.next_line().context("read rollout file")? {
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
            if !stream.begun() {
                let meta = match session_payload(&obj) {
                    Some(p) => meta_from_session(p),
                    None => filename_meta(source),
                };
                stream.begin(meta)?;
            }
            match convert(&obj, line.number, &mut st) {
                Converted::Event(d) => stream.push(*d)?,
                Converted::Skip(kind) => tally.skip(&kind),
                Converted::Fail(why) => tally.fail(line.number, why),
            }
        }
        stream.finish(filename_meta(source))?;
        Ok(tally.into_report(&stream))
    }
}

fn evidence(o: &Map<String, Value>) -> Option<&'static str> {
    let ty = o.get("type").and_then(Value::as_str);
    let payload = o.get("payload").and_then(Value::as_object);
    let has = |m: &Map<String, Value>, k: &str| m.get(k).is_some_and(Value::is_string);
    match (ty, payload) {
        (Some("session_meta"), Some(p)) if has(p, "id") => Some("session_meta record"),
        (Some("response_item" | "event_msg"), Some(p)) if has(o, "timestamp") && has(p, "type") => {
            Some("timestamped response_item/event_msg envelope")
        }
        (Some("turn_context"), Some(p))
            if has(o, "timestamp") && (has(p, "cwd") || has(p, "model")) =>
        {
            Some("turn_context record")
        }
        // Older rollouts: untyped header, then bare items.
        (None, None) if has(o, "id") && has(o, "timestamp") && o.contains_key("instructions") => {
            Some("legacy rollout header")
        }
        (Some("function_call"), None) if has(o, "call_id") && has(o, "name") => {
            Some("legacy function_call item")
        }
        (Some("message"), None)
            if o.get("content")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|b| b.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|t| t == "input_text" || t == "output_text") =>
        {
            Some("legacy message item")
        }
        _ => None,
    }
}

/// The `session_meta` payload of a record, or the record itself when it is a
/// legacy untyped header.
fn session_payload(o: &Map<String, Value>) -> Option<&Map<String, Value>> {
    match o.get("type").and_then(Value::as_str) {
        Some("session_meta") => o.get("payload").and_then(Value::as_object),
        None if str_id(o).is_some() && !o.contains_key("record_type") => Some(o),
        _ => None,
    }
}

fn str_id(o: &Map<String, Value>) -> Option<&str> {
    ["id", "session_id"]
        .iter()
        .find_map(|k| o.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
}

fn meta_from_session(p: &Map<String, Value>) -> ConversationMeta {
    let id = str_id(p)
        .filter(|id| is_globally_unique(id))
        .map(str::to_string);
    let git = p.get("git").and_then(Value::as_object);
    let git_str = |k: &str| {
        git.and_then(|g| g.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut metadata = Map::new();
    for k in [
        "forked_from_id",
        "originator",
        "cli_version",
        "source",
        "thread_source",
    ] {
        if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
            metadata.insert(k.into(), v.clone());
        }
    }
    if is_subagent(p) {
        metadata.insert("is_subagent".into(), json!(true));
    }
    ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        working_directory: p.get("cwd").and_then(Value::as_str).map(str::to_string),
        branch: git_str("branch"),
        git_remote: git_str("repository_url"),
        metadata,
        identity_hint: if id.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    }
}

/// `source` is a plain string for user-started sessions; an object with a
/// `subagent` key marks a spawned one.
fn is_subagent(p: &Map<String, Value>) -> bool {
    p.get("source")
        .and_then(Value::as_object)
        .is_some_and(|s| s.contains_key("subagent"))
}

/// No usable session header: identify the conversation by the id embedded in a
/// `rollout-<time>-<uuid>.jsonl` file name, when it has one.
fn filename_meta(source: &Source) -> ConversationMeta {
    let stem = source
        .inner_path
        .as_deref()
        .map(std::path::Path::new)
        .unwrap_or(&source.read_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let id = stem
        .strip_prefix("rollout-")
        .and_then(|rest| rest.get(rest.len().checked_sub(36)?..))
        .filter(|id| is_globally_unique(id))
        .map(str::to_string);
    ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        identity_hint: if id.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    }
}

#[derive(Default)]
struct State {
    base_cwd: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
}

enum Converted {
    Event(Box<EventDraft>),
    Skip(String),
    Fail(String),
}

impl Converted {
    fn event(d: EventDraft) -> Converted {
        Converted::Event(Box::new(d))
    }
}

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn convert(obj: &Map<String, Value>, line: u64, st: &mut State) -> Converted {
    let ty = s(obj, "type");
    let payload_obj = obj.get("payload").and_then(Value::as_object);
    // Legacy layout: the record itself is the item.
    let legacy_item = payload_obj.is_none() && ty.is_some_and(|t| LEGACY_ITEMS.contains(&t));
    let kind = match (ty, legacy_item) {
        (_, true) => "response_item",
        (Some(t), false) => t,
        (None, _) if obj.contains_key("record_type") => {
            return Converted::Skip(format!(
                "record_type:{}",
                s(obj, "record_type").unwrap_or("?")
            ))
        }
        (None, _) if session_payload(obj).is_some() => "session_meta",
        (None, _) => return Converted::Fail("record has no string `type`".into()),
    };
    if NOISE.contains(&kind) {
        return Converted::Skip(kind.to_string());
    }
    let payload: &Map<String, Value> = match (legacy_item, payload_obj) {
        (true, _) => obj,
        (false, Some(p)) => p,
        (false, None) if kind == "session_meta" => obj,
        (false, None) => return Converted::Fail(format!("{kind} record has no payload object")),
    };
    let mut d = EventDraft {
        timestamp: stamp(
            obj.get("timestamp")
                .or_else(|| payload.get("timestamp").filter(|_| kind == "session_meta")),
        ),
        metadata: Map::new(),
        ..Default::default()
    };
    d.metadata.insert("source_line".into(), json!(line));
    d.metadata.insert("record_type".into(), json!(kind));
    if let Some(o) = obj.get("ordinal").filter(|v| !v.is_null()) {
        d.metadata.insert("ordinal".into(), o.clone());
    }
    if let Some(c) = &st.cwd {
        if st.base_cwd.as_ref() != Some(c) {
            d.metadata.insert("cwd".into(), json!(c));
        }
    }
    let payload_val = || Value::Object(payload.clone());
    match kind {
        "session_meta" => {
            st.base_cwd = s(payload, "cwd").map(str::to_string);
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            if let Some(id) = str_id(payload) {
                d.native_id = Some(format!("{id}:session_meta"));
            }
            // `base_instructions` is `{text}` in current rollouts, `instructions`
            // is a bare string in older ones.
            let instructions = payload
                .get("base_instructions")
                .and_then(|b| b.as_str().or_else(|| b.get("text").and_then(Value::as_str)))
                .or_else(|| s(payload, "instructions"))
                .filter(|t| !t.is_empty());
            d.content = instructions.map(Part::text).into_iter().collect();
            for k in [
                "forked_from_id",
                "originator",
                "cli_version",
                "source",
                "thread_source",
                "model_provider",
                "git",
            ] {
                if let Some(v) = payload.get(k).filter(|v| !v.is_null()) {
                    d.metadata.insert(k.into(), v.clone());
                }
            }
            if is_subagent(payload) {
                d.metadata.insert("is_subagent".into(), json!(true));
            }
        }
        "turn_context" => {
            st.model = s(payload, "model").map(str::to_string).or(st.model.take());
            st.cwd = s(payload, "cwd").map(str::to_string).or(st.cwd.take());
            return Converted::Skip("turn_context".into());
        }
        "compacted" => {
            d.role = Role::System;
            d.event_type = EventType::Compaction;
            if let Some(id) =
                s(payload, "compaction_response_id").or_else(|| s(payload, "window_id"))
            {
                d.native_id = Some(format!("compacted:{id}"));
            }
            if let Some(m) = s(payload, "message") {
                d.content.push(Part::text(m));
            }
            // The replacement history and window ids ride along verbatim.
            let mut rest = payload.clone();
            rest.remove("message");
            if !rest.is_empty() {
                d.content.push(Part::Opaque {
                    kind: "compacted".into(),
                    note: None,
                    raw: Some(Value::Object(rest)),
                });
            }
        }
        "response_item" => return response_item(d, payload, st),
        "event_msg" => return event_msg(d, payload),
        other => {
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![Part::Opaque {
                kind: other.into(),
                note: None,
                raw: Some(payload_val()),
            }];
        }
    }
    Converted::event(d)
}

fn event_msg(mut d: EventDraft, p: &Map<String, Value>) -> Converted {
    let Some(t) = s(p, "type") else {
        return Converted::Fail("event_msg has no string `type`".into());
    };
    d.metadata.insert("payload_type".into(), json!(t));
    d.role = Role::System;
    match t {
        t if ECHO_EVENTS.contains(&t) => {
            return Converted::Skip(format!("event_msg:{t} (echo of response_item)"))
        }
        "item_completed" => {
            let item = p.get("item").cloned().unwrap_or(Value::Null);
            let item_ty = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            if ECHO_ITEMS.contains(&item_ty.as_str()) {
                return Converted::Skip(format!(
                    "event_msg:item_completed:{item_ty} (echo of response_item)"
                ));
            }
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                d.native_id = Some(format!("{id}:completed"));
            }
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![Part::Opaque {
                kind: format!("item_completed:{item_ty}"),
                note: None,
                raw: Some(item),
            }];
        }
        "turn_aborted" | "error" => {
            d.event_type = if t == "error" {
                EventType::Error
            } else {
                EventType::Interruption
            };
            for k in ["turn_id", "duration_ms"] {
                if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
                    d.metadata.insert(k.into(), v.clone());
                }
            }
            d.content = vec![match s(p, "reason").or_else(|| s(p, "message")) {
                Some(text) => Part::text(text),
                None => Part::Opaque {
                    kind: t.into(),
                    note: None,
                    raw: Some(Value::Object(p.clone())),
                },
            }];
        }
        other => return Converted::Skip(format!("event_msg:{other}")),
    }
    Converted::event(d)
}

fn response_item(mut d: EventDraft, p: &Map<String, Value>, st: &State) -> Converted {
    let Some(t) = s(p, "type") else {
        return Converted::Fail("response_item has no string `type`".into());
    };
    d.metadata.insert("payload_type".into(), json!(t));
    if let Some(c) = p
        .get("internal_chat_message_metadata_passthrough")
        .filter(|v| !v.is_null())
    {
        d.metadata.insert("chat_metadata".into(), c.clone());
    }
    if let Some(v) = p.get("status").filter(|v| !v.is_null()) {
        d.metadata.insert("status".into(), v.clone());
    }
    let call_id = s(p, "call_id").or_else(|| s(p, "id")).map(str::to_string);
    let model_made = |d: &mut EventDraft| d.model = st.model.clone();
    match t {
        "message" => {
            d.role = Role::parse(s(p, "role").unwrap_or("other"));
            d.native_id = s(p, "id").map(str::to_string);
            d.content = content_parts(p.get("content"));
            if d.role == Role::Assistant {
                model_made(&mut d);
            }
        }
        "agent_message" => {
            d.role = Role::Other;
            d.native_id = s(p, "id").map(str::to_string);
            d.content = content_parts(p.get("content"));
            for k in ["author", "recipient"] {
                if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
                    d.metadata.insert(k.into(), v.clone());
                }
            }
        }
        "reasoning" => {
            d.role = Role::Assistant;
            d.event_type = EventType::Reasoning;
            d.native_id = s(p, "id").map(str::to_string);
            d.content = reasoning_parts(p);
            model_made(&mut d);
        }
        "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" => {
            d.role = Role::Assistant;
            d.event_type = EventType::ToolCall;
            model_made(&mut d);
            let (name, arguments) = match t {
                "function_call" => (
                    s(p, "name").unwrap_or("").to_string(),
                    match p.get("arguments") {
                        // Arguments arrive as a JSON string; keep the text when it is not JSON.
                        Some(Value::String(a)) => {
                            serde_json::from_str(a).unwrap_or_else(|_| Value::String(a.clone()))
                        }
                        Some(v) => v.clone(),
                        None => Value::Null,
                    },
                ),
                "custom_tool_call" => (
                    s(p, "name").unwrap_or("").to_string(),
                    p.get("input").cloned().unwrap_or(Value::Null),
                ),
                "local_shell_call" => (
                    "local_shell".to_string(),
                    p.get("action").cloned().unwrap_or(Value::Null),
                ),
                _ => (
                    "web_search".to_string(),
                    p.get("action").cloned().unwrap_or(Value::Null),
                ),
            };
            d.native_id = call_id.as_ref().map(|c| format!("{c}:call"));
            d.tool_call_ids = call_id.clone().into_iter().collect();
            d.content = vec![Part::ToolCall {
                id: call_id,
                name,
                arguments,
            }];
        }
        "function_call_output" | "custom_tool_call_output" => {
            d.role = Role::Tool;
            d.event_type = EventType::ToolResult;
            let output = p.get("output").cloned().unwrap_or(Value::Null);
            let is_error = output.get("success").and_then(Value::as_bool) == Some(false);
            d.native_id = call_id.as_ref().map(|c| format!("{c}:output"));
            d.tool_call_ids = call_id.clone().into_iter().collect();
            d.content = vec![Part::ToolResult {
                tool_call_id: call_id,
                output,
                is_error,
            }];
        }
        other => {
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.native_id = s(p, "id").map(str::to_string);
            d.content = vec![Part::Opaque {
                kind: other.into(),
                note: None,
                raw: Some(Value::Object(p.clone())),
            }];
        }
    }
    Converted::event(d)
}

fn content_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(t)) if t.is_empty() => Vec::new(),
        Some(Value::String(t)) => vec![Part::text(t.as_str())],
        Some(Value::Array(blocks)) => blocks.iter().map(block_part).collect(),
        Some(other) => vec![Part::Opaque {
            kind: "content".into(),
            note: Some("unexpected content shape".into()),
            raw: Some(other.clone()),
        }],
    }
}

fn block_part(block: &Value) -> Part {
    let opaque = |kind: &str| Part::Opaque {
        kind: kind.to_string(),
        note: None,
        raw: Some(block.clone()),
    };
    let Some(b) = block.as_object() else {
        return opaque("non_object_block");
    };
    match s(b, "type").unwrap_or("untyped") {
        "input_text" | "output_text" | "text" => Part::text(s(b, "text").unwrap_or("")),
        "input_image" => match str_of(block, "image_url").filter(|u| !u.starts_with("data:")) {
            Some(url) => Part::Image {
                artifact: None,
                mime: None,
                filename: None,
                source_ref: Some(url.to_string()),
            },
            // ponytail: inline data URLs stay as opaque raw blocks (bounded by
            // max_record_bytes); decode into artifacts if screenshots get large.
            None => opaque("input_image"),
        },
        other => opaque(other),
    }
}

fn reasoning_parts(p: &Map<String, Value>) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut texts = |key: &str, visibility: ReasoningVisibility| {
        for item in p.get(key).and_then(Value::as_array).into_iter().flatten() {
            let text = item
                .as_str()
                .or_else(|| item.get("text").and_then(Value::as_str));
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                parts.push(Part::Reasoning {
                    text: text.to_string(),
                    visibility,
                });
            }
        }
    };
    texts("summary", ReasoningVisibility::Summary);
    texts("content", ReasoningVisibility::Public);
    if let Some(enc) = s(p, "encrypted_content") {
        parts.push(Part::Opaque {
            kind: "encrypted_reasoning".into(),
            note: Some("encrypted_content present; not decoded".into()),
            raw: Some(json!({ "encrypted_content": enc })),
        });
    }
    if parts.is_empty() {
        parts.push(Part::Opaque {
            kind: "reasoning".into(),
            note: None,
            raw: Some(Value::Object(p.clone())),
        });
    }
    parts
}
