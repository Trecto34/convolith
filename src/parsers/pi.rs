//! Pi coding agent (`~/.pi/agent/sessions/**.jsonl`) and its fork Oh My Pi
//! (`~/.omp/agent/sessions/**.jsonl`) session parser.
//!
//! Both write the same v3 tree-structured JSONL: a `session` header, then
//! entries linked by `id` / `parentId`. Oh My Pi differs by a leading
//! fixed-width `title` record and a few extra entry kinds, so it is the same
//! parser registered twice with its own id and application label.
//!
//! Format knowledge follows xhluca/session-migrate (MIT licence,
//! <https://github.com/xhluca/session-migrate>, `docs/omp-format.md` and
//! `formats/pi.py`); the semantics were ported and adapted to convolith's
//! canonical model, not copied line by line.
//!
//! Mapping rules, in short:
//! * one source line -> at most one event; every non-blank line is an event,
//!   an intentional skip, or a reported failure;
//! * the entry `id` is the native event id and `parentId` is kept verbatim, so
//!   every branch of the tree is imported (nothing is pruned to the "active"
//!   path); entry ids are only unique per file, which the importer scopes by
//!   conversation;
//! * `toolCall` blocks and `toolResult` messages become structured parts;
//! * `thinking` text is emitted only when literally present;
//! * a timestamp comes from the entry (or the message) or stays unknown.

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::BufReader;

/// Records buffered while waiting for the session header.
const PREAMBLE_CAP: usize = 256;

pub struct PiParser {
    omp: bool,
}

impl PiParser {
    pub fn pi() -> PiParser {
        PiParser { omp: false }
    }
    pub fn oh_my_pi() -> PiParser {
        PiParser { omp: true }
    }
}

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn is_header(o: &Map<String, Value>) -> bool {
    o.get("type").and_then(Value::as_str) == Some("session")
        && s(o, "id").is_some()
        && s(o, "cwd").is_some()
        && o.get("version").map_or(true, Value::is_number)
}

fn is_title_slot(o: &Map<String, Value>) -> bool {
    o.get("type").and_then(Value::as_str) == Some("title")
        && o.get("v").is_some_and(Value::is_number)
}

/// A tree entry as Pi writes it: `type: message`, own `id`, a `parentId` key
/// (null for the root) and a role-carrying message object.
fn is_message_entry(o: &Map<String, Value>) -> bool {
    o.get("type").and_then(Value::as_str) == Some("message")
        && s(o, "id").is_some()
        && o.contains_key("parentId")
        && o.get("message")
            .and_then(Value::as_object)
            .is_some_and(|m| s(m, "role").is_some())
}

impl SourceParser for PiParser {
    fn id(&self) -> &'static str {
        if self.omp {
            "oh_my_pi"
        } else {
            "pi"
        }
    }
    fn provider(&self) -> &'static str {
        "pi"
    }
    fn application(&self) -> &'static str {
        if self.omp {
            "oh-my-pi"
        } else {
            "pi"
        }
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
        if self.omp {
            "Oh My Pi JSONL tree sessions (~/.omp/agent/sessions/**)"
        } else {
            "Pi coding agent JSONL tree sessions (~/.pi/agent/sessions/**)"
        }
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir || probe.ext() != "jsonl" {
            return Detection::none(self.id());
        }
        let records = probe_records(probe, 20);
        let slot = records.first().is_some_and(is_title_slot);
        let omp_store = probe.has_component(".omp");
        if (slot || omp_store) != self.omp {
            return Detection::none(self.id());
        }
        let in_store = probe.has_component("sessions")
            && probe.has_component(if self.omp { ".omp" } else { ".pi" });
        let body = &records[usize::from(slot).min(records.len())..];
        let reason = if body.first().is_some_and(is_header) {
            "session header with id/cwd followed by tree entries"
        } else if body.iter().any(is_message_entry) {
            "message entries with id/parentId and message.role"
        } else {
            return Detection::none(self.id());
        };
        let d = Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            "pi-jsonl-v3",
            if in_store {
                Confidence::Certain
            } else {
                Confidence::Strong
            },
            reason,
        );
        let d = if slot {
            d.with_reason("leading title record (Oh My Pi)")
        } else {
            d
        };
        if in_store {
            d.with_reason("located under an agent sessions directory")
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
        let mut head = Head::default();

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
            if !stream.begun() {
                head.absorb(&obj);
                if head.session.is_some() || stream.pending_len() >= PREAMBLE_CAP {
                    stream.begin(head.meta(None))?;
                }
            }
            match convert(&obj, line.number) {
                Converted::Event(d) => stream.push(d)?,
                Converted::Skip(kind) => tally.skip(&kind),
                Converted::Fail(why) => tally.fail(line.number, why),
            }
        }
        stream.finish(head.meta(filename_id(source)))?;
        Ok(tally.into_report(&stream))
    }
}

/// `<timestamp>_<uuid>.jsonl`: the uuid, used only when no header names the
/// session.
fn filename_id(source: &Source) -> Option<String> {
    let stem = source.read_path.file_stem()?.to_str()?;
    let id = stem.rsplit('_').next()?;
    is_globally_unique(id).then(|| id.to_string())
}

#[derive(Default)]
struct Head {
    session: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    parent_session: Option<String>,
    version: Option<Value>,
    started: Option<Value>,
}

impl Head {
    fn absorb(&mut self, o: &Map<String, Value>) {
        if is_title_slot(o) {
            self.title = s(o, "title").map(|t| t.trim().to_string());
        } else if is_header(o) && self.session.is_none() {
            self.session = s(o, "id").map(str::to_string);
            self.cwd = s(o, "cwd").map(str::to_string);
            self.parent_session = s(o, "parentSession").map(str::to_string);
            self.version = o.get("version").cloned();
            self.started = o.get("timestamp").cloned();
        }
    }

    fn meta(&self, fallback_id: Option<String>) -> ConversationMeta {
        let id = self
            .session
            .clone()
            .or(fallback_id)
            // Anything not provably unique must not name a conversation.
            .filter(|id| is_globally_unique(id));
        let mut metadata = Map::new();
        if let Some(v) = &self.version {
            metadata.insert("format_version".into(), v.clone());
        }
        if let Some(p) = &self.parent_session {
            metadata.insert("parent_session".into(), json!(p));
        }
        ConversationMeta {
            native_id: id.clone(),
            native_session_id: id.clone(),
            title: self.title.clone().filter(|t| !t.is_empty()),
            working_directory: self.cwd.clone(),
            started_at: self
                .started
                .as_ref()
                .map(|v| stamp(Some(v)))
                .filter(|st| st.utc.is_some()),
            metadata,
            identity_hint: if id.is_some() {
                IdentityHint::Native
            } else {
                IdentityHint::Fingerprint
            },
            ..Default::default()
        }
    }
}

enum Converted {
    Event(EventDraft),
    Skip(String),
    Fail(String),
}

pub(super) fn opaque(kind: &str, raw: Value) -> Part {
    Part::Opaque {
        kind: kind.to_string(),
        note: None,
        raw: Some(raw),
    }
}

fn convert(obj: &Map<String, Value>, line: u64) -> Converted {
    let Some(ty) = obj.get("type").and_then(Value::as_str) else {
        return Converted::Fail("record has no string `type`".into());
    };
    if is_title_slot(obj) {
        return Converted::Skip("title (applied to the conversation title)".into());
    }
    let mut d = EventDraft {
        timestamp: stamp(obj.get("timestamp")),
        native_id: s(obj, "id").map(str::to_string),
        parent_native_id: s(obj, "parentId").map(str::to_string),
        ..Default::default()
    };
    d.metadata.insert("source_line".into(), json!(line));
    d.metadata.insert("record_type".into(), json!(ty));
    let whole = || Value::Object(obj.clone());
    match ty {
        "message" => {
            let Some(msg) = obj.get("message").and_then(Value::as_object) else {
                return Converted::Fail("message entry has no message object".into());
            };
            if obj.get("timestamp").is_none() {
                // Fall back to the message's own epoch-ms stamp, never to anything else.
                d.timestamp = stamp(msg.get("timestamp"));
            } else if let Some(t) = msg.get("timestamp").filter(|t| !t.is_null()) {
                d.metadata.insert("message_timestamp".into(), t.clone());
            }
            if let Err(why) = convert_message(&mut d, msg) {
                return Converted::Fail(why);
            }
        }
        "compaction" => {
            d.role = Role::System;
            d.event_type = EventType::Compaction;
            d.content = vec![match s(obj, "summary") {
                Some(t) => Part::text(t),
                None => opaque("compaction", whole()),
            }];
            copy(
                &mut d,
                obj,
                &[
                    ("firstKeptEntryId", "first_kept_entry_id"),
                    ("tokensBefore", "tokens_before"),
                    ("details", "details"),
                    ("fromHook", "from_hook"),
                ],
            );
        }
        "branch_summary" => {
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            d.content = vec![match s(obj, "summary") {
                Some(t) => Part::text(t),
                None => opaque("branch_summary", whole()),
            }];
            copy(
                &mut d,
                obj,
                &[("fromId", "from_id"), ("details", "details")],
            );
        }
        "custom_message" => {
            d.role = Role::Other;
            copy(
                &mut d,
                obj,
                &[
                    ("customType", "custom_type"),
                    ("display", "display"),
                    ("details", "details"),
                ],
            );
            d.content = parts_of(obj.get("content"));
        }
        "session" => {
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            d.content = vec![opaque("session_header", whole())];
        }
        "session_info" => {
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            d.content = vec![match s(obj, "name") {
                Some(t) => Part::text(t),
                None => opaque("session_info", whole()),
            }];
        }
        // Runtime state (model/thinking changes, labels, extension data, Oh My
        // Pi mode changes ...) and anything newer: kept verbatim, not guessed at.
        other => {
            d.role = if matches!(
                other,
                "model_change" | "thinking_level_change" | "label" | "title_change"
            ) {
                Role::System
            } else {
                Role::Other
            };
            d.event_type = if d.role == Role::System {
                EventType::SystemNote
            } else {
                EventType::Opaque
            };
            d.content = vec![opaque(other, whole())];
        }
    }
    Converted::Event(d)
}

pub(super) fn copy(d: &mut EventDraft, o: &Map<String, Value>, pairs: &[(&str, &str)]) {
    for (src, dst) in pairs {
        if let Some(v) = o.get(*src).filter(|v| !v.is_null()) {
            d.metadata.insert((*dst).into(), v.clone());
        }
    }
}

pub(super) fn convert_message(d: &mut EventDraft, msg: &Map<String, Value>) -> Result<(), String> {
    let role = s(msg, "role").ok_or("message has no role")?;
    d.event_type = EventType::Message;
    match role {
        "user" | "assistant" => {
            d.role = Role::parse(role);
            d.model = s(msg, "model").map(str::to_string);
            copy(
                d,
                msg,
                &[
                    ("provider", "provider"),
                    ("api", "api"),
                    ("usage", "usage"),
                    ("stopReason", "stop_reason"),
                    ("errorMessage", "error_message"),
                    ("responseId", "response_id"),
                ],
            );
            d.content = parts_of(msg.get("content"));
            if d.role == Role::Assistant {
                let text = d.content.iter().any(|p| matches!(p, Part::Text { .. }));
                let calls = d.content.iter().any(|p| matches!(p, Part::ToolCall { .. }));
                if !text && calls {
                    d.event_type = EventType::ToolCall;
                } else if !text
                    && !d.content.is_empty()
                    && d.content.iter().all(|p| {
                        matches!(p, Part::Reasoning { .. })
                            || matches!(p, Part::Opaque { kind, .. } if kind == "thinking")
                    })
                {
                    d.event_type = EventType::Reasoning;
                }
            }
        }
        "toolResult" => {
            d.role = Role::Tool;
            d.event_type = EventType::ToolResult;
            copy(d, msg, &[("toolName", "tool_name"), ("details", "details")]);
            d.content = vec![Part::ToolResult {
                tool_call_id: s(msg, "toolCallId").map(str::to_string),
                output: msg.get("content").cloned().unwrap_or(Value::Null),
                is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
            }];
        }
        "bashExecution" => {
            // A shell command the user ran themselves (`!cmd`): a call and its
            // result with no provider-issued ids to link them.
            d.role = Role::User;
            copy(
                d,
                msg,
                &[
                    ("exitCode", "exit_code"),
                    ("cancelled", "cancelled"),
                    ("truncated", "truncated"),
                    ("excludeFromContext", "exclude_from_context"),
                ],
            );
            d.metadata.insert("source_role".into(), json!(role));
            d.content = vec![
                Part::ToolCall {
                    id: None,
                    name: "bash".into(),
                    arguments: json!({ "command": msg.get("command").cloned().unwrap_or(Value::Null) }),
                },
                Part::ToolResult {
                    tool_call_id: None,
                    output: msg.get("output").cloned().unwrap_or(Value::Null),
                    is_error: msg
                        .get("exitCode")
                        .and_then(Value::as_i64)
                        .is_some_and(|c| c != 0),
                },
            ];
        }
        "compactionSummary" | "branchSummary" => {
            d.role = Role::System;
            d.event_type = if role == "compactionSummary" {
                EventType::Compaction
            } else {
                EventType::SystemNote
            };
            copy(
                d,
                msg,
                &[("tokensBefore", "tokens_before"), ("fromId", "from_id")],
            );
            d.metadata.insert("source_role".into(), json!(role));
            d.content = vec![match s(msg, "summary") {
                Some(t) => Part::text(t),
                None => opaque(role, Value::Object(msg.clone())),
            }];
        }
        "custom" => {
            d.role = Role::Other;
            copy(
                d,
                msg,
                &[("customType", "custom_type"), ("display", "display")],
            );
            d.metadata.insert("source_role".into(), json!(role));
            d.content = parts_of(msg.get("content"));
        }
        other => {
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![opaque(other, Value::Object(msg.clone()))];
        }
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
    Ok(())
}

pub(super) fn parts_of(content: Option<&Value>) -> Vec<Part> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(t)) if t.is_empty() => Vec::new(),
        Some(Value::String(t)) => vec![Part::text(t.as_str())],
        Some(Value::Array(blocks)) => blocks.iter().map(block_part).collect(),
        Some(other) => vec![opaque("content", other.clone())],
    }
}

fn block_part(block: &Value) -> Part {
    let Some(b) = block.as_object() else {
        return opaque("non_object_block", block.clone());
    };
    match s(b, "type").unwrap_or("untyped") {
        "text" => Part::text(s(b, "text").unwrap_or("")),
        "thinking" => match s(b, "thinking") {
            Some(t) => Part::Reasoning {
                text: t.into(),
                visibility: ReasoningVisibility::Public,
            },
            // Signature-only (redacted) thinking: no readable text, none invented.
            None => Part::Opaque {
                kind: "thinking".into(),
                note: Some("no thinking text present in source".into()),
                raw: None,
            },
        },
        "toolCall" => Part::ToolCall {
            id: s(b, "id").map(str::to_string),
            name: s(b, "name").unwrap_or("").to_string(),
            arguments: b.get("arguments").cloned().unwrap_or(Value::Null),
        },
        // ponytail: base64 images stay inline as opaque raw blocks (bounded by
        // max_record_bytes); decode into artifacts if sessions get heavy.
        other => opaque(other, block.clone()),
    }
}
