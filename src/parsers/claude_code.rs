//! Claude Code session parser: JSONL transcripts under `~/.claude/projects/**`
//! (including `<session>/subagents/agent-*.jsonl`).
//!
//! Format knowledge (record envelope, `parentUuid` tree, sidechains, content
//! blocks) follows xhluca/session-migrate (MIT licence,
//! <https://github.com/xhluca/session-migrate>); the semantics were ported and
//! adapted to convolith's canonical model, not copied line by line.
//!
//! Mapping rules, in short:
//! * one source record -> at most one event; every non-blank line is an event,
//!   an intentional skip, or a reported failure;
//! * the record `uuid` is the native event id, `parentUuid` is kept verbatim;
//! * `tool_use` / `tool_result` blocks become structured parts; a user record
//!   carrying only tool results is role `tool` (source role kept in metadata);
//! * `thinking` text is emitted only when literally present;
//! * a timestamp is taken from the record or left unknown, never invented.

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::BufReader;
use std::io::Read;

pub struct ClaudeCodeParser;

/// Bookkeeping records with no conversational content. They are counted and
/// named in the report notes, not silently dropped.
const NOISE: &[&str] = &[
    "file-history-snapshot",
    "file-history-delta",
    "progress",
    "queue-operation",
    "last-prompt",
    "permission-mode",
    "mode",
    "atis-latch",
    "bridge-session",
    "cost-state",
];

/// Envelope fields copied into event metadata under a snake_case name.
const COPIED: &[(&str, &str)] = &[
    ("version", "version"),
    ("entrypoint", "entrypoint"),
    ("userType", "user_type"),
    ("requestId", "request_id"),
    ("promptId", "prompt_id"),
    ("slug", "slug"),
    ("isMeta", "is_meta"),
    ("isCompactSummary", "is_compact_summary"),
    ("isApiErrorMessage", "is_api_error_message"),
    ("sourceToolAssistantUUID", "source_tool_assistant_uuid"),
    ("sourceToolUseID", "source_tool_use_id"),
    ("toolUseID", "tool_use_id"),
    ("level", "level"),
    ("subtype", "subtype"),
    ("compactMetadata", "compact_metadata"),
    ("leafUuid", "leaf_uuid"),
    ("logicalParentUuid", "logical_parent_uuid"),
];

/// Records buffered while waiting for one that names the session and cwd.
const PREAMBLE_CAP: usize = 256;
const DETECT_PREAMBLE_BYTES: u64 = 1024 * 1024;

impl SourceParser for ClaudeCodeParser {
    fn id(&self) -> &'static str {
        "claude_code"
    }
    fn provider(&self) -> &'static str {
        "anthropic"
    }
    fn application(&self) -> &'static str {
        "claude-code"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: false,
            reasoning: true,
            streaming: true,
            // toolUseResult side-channel and bookkeeping records are not imported.
            partial: true,
        }
    }
    fn description(&self) -> &'static str {
        "Claude Code JSONL sessions (~/.claude/projects/**, incl. subagent sidechains)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir || probe.ext() != "jsonl" {
            return Detection::none(self.id());
        }
        let in_store = probe.has_component(".claude") && probe.has_component("projects");
        let records = probe_records(probe, 20);
        let evidence = records.iter().find_map(evidence).or_else(|| {
            // Real bridge/queue preambles can hide the first message beyond
            // the ordinary 16 KiB window. Retry only this known store/shape,
            // still bounded, and still require the existing envelope evidence.
            if !in_store
                || probe.head_bytes.len() as u64 >= probe.size
                || !["\"queue-operation\"", "\"bridge-session\""]
                    .iter()
                    .any(|k| probe.head.contains(k))
            {
                return None;
            }
            let mut wide = probe.clone();
            wide.head_bytes.clear();
            File::open(&probe.path)
                .ok()?
                .take(DETECT_PREAMBLE_BYTES)
                .read_to_end(&mut wide.head_bytes)
                .ok()?;
            probe_records(&wide, PREAMBLE_CAP).iter().find_map(evidence)
        });
        let (conf, reason) = match evidence {
            Some(r) => (
                if in_store {
                    Confidence::Certain
                } else {
                    Confidence::Strong
                },
                r.to_string(),
            ),
            // First record longer than the probe window: fall back to the keys
            // that only a Claude Code message envelope has together.
            None if records.is_empty()
                && probe.head.starts_with('{')
                && ["\"parentUuid\"", "\"sessionId\"", "\"uuid\""]
                    .iter()
                    .all(|k| probe.head.contains(k)) =>
            {
                (
                    if in_store {
                        Confidence::Certain
                    } else {
                        Confidence::Strong
                    },
                    "oversized first record carries parentUuid/sessionId/uuid".to_string(),
                )
            }
            None => return Detection::none(self.id()),
        };
        let d = Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            "claude-code-jsonl",
            conf,
            reason,
        );
        if in_store {
            d.with_reason("located under .claude/projects")
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
                if head.ready() || stream.pending_len() >= PREAMBLE_CAP {
                    stream.begin(head.meta())?;
                }
            }
            match convert(&obj, line.number, &head) {
                Converted::Event(d) => stream.push(d)?,
                Converted::Skip(kind) => tally.skip(&kind),
                Converted::Fail(why) => tally.fail(line.number, why),
            }
        }
        stream.finish(head.meta())?;
        Ok(tally.into_report(&stream))
    }
}

/// Classify a complete, small bookkeeping-only file without creating events.
pub fn known_unsupported(probe: &Probe) -> Option<(&'static str, &'static str)> {
    if probe.ext() != "jsonl"
        || !probe.has_component(".claude")
        || !probe.has_component("projects")
        || (probe.head_bytes.len() as u64) < probe.size
    {
        return None;
    }
    let records = probe_records(probe, PREAMBLE_CAP);
    (!records.is_empty()
        && records.len() < PREAMBLE_CAP
        && records.len() == probe.head.lines().filter(|l| !l.trim().is_empty()).count()
        && records.iter().all(|o| {
            o.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| NOISE.contains(&t))
                && o.get("sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(is_globally_unique)
                && !o.contains_key("message")
        }))
    .then_some((
        "claude-bookkeeping",
        "complete Claude Code bookkeeping-only JSONL; no conversation messages",
    ))
}

fn evidence(o: &Map<String, Value>) -> Option<&'static str> {
    let ty = o.get("type")?.as_str()?;
    let has = |k: &str| o.get(k).is_some_and(Value::is_string);
    match ty {
        "user" | "assistant" | "system" | "attachment"
            if has("uuid")
                && has("sessionId")
                && (o.contains_key("parentUuid") || o.contains_key("isSidechain")) =>
        {
            Some("message record with uuid/parentUuid/sessionId")
        }
        "summary" if has("leafUuid") && has("summary") => Some("summary record with leafUuid"),
        "file-history-snapshot"
            if has("messageId") && o.get("snapshot").is_some_and(Value::is_object) =>
        {
            Some("file-history-snapshot record")
        }
        "ai-title" if has("aiTitle") && has("sessionId") => Some("ai-title record"),
        "custom-title" if has("customTitle") && has("sessionId") => Some("custom-title record"),
        "permission-mode" if has("permissionMode") && has("sessionId") => {
            Some("permission-mode record")
        }
        _ => None,
    }
}

/// Conversation header, learned from the first records of the file.
#[derive(Default)]
struct Head {
    session: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    agent_id: Option<String>,
    sidechain: bool,
    version: Option<String>,
    entrypoint: Option<String>,
}

impl Head {
    fn absorb(&mut self, o: &Map<String, Value>) {
        let take = |slot: &mut Option<String>, key: &str| {
            if slot.is_none() {
                *slot = o
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
        };
        take(&mut self.session, "sessionId");
        take(&mut self.cwd, "cwd");
        take(&mut self.branch, "gitBranch");
        take(&mut self.agent_id, "agentId");
        take(&mut self.version, "version");
        take(&mut self.entrypoint, "entrypoint");
        if o.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            self.sidechain = true;
        }
    }

    fn ready(&self) -> bool {
        self.session.is_some() && self.cwd.is_some()
    }

    fn meta(&self) -> ConversationMeta {
        // A subagent transcript shares the parent's sessionId, so it needs its
        // own conversation identity or it would merge into the parent.
        let sidechain_id = match (&self.session, &self.agent_id) {
            (Some(s), Some(a)) if self.sidechain => Some(format!("{s}/agent-{a}")),
            _ => None,
        };
        let id = sidechain_id
            .clone()
            .or_else(|| self.session.clone())
            // Anything not provably unique must not name a conversation: the
            // importer then falls back to path/title coordinates instead.
            .filter(|id| is_globally_unique(id));
        let mut metadata = Map::new();
        if self.sidechain {
            metadata.insert("is_sidechain".into(), json!(true));
        }
        if sidechain_id.is_some() {
            metadata.insert("parent_session_id".into(), json!(self.session));
            metadata.insert("agent_id".into(), json!(self.agent_id));
        }
        if let Some(v) = &self.version {
            metadata.insert("version".into(), json!(v));
        }
        if let Some(v) = &self.entrypoint {
            metadata.insert("entrypoint".into(), json!(v));
        }
        ConversationMeta {
            native_id: id.clone(),
            native_session_id: id.clone(),
            working_directory: self.cwd.clone(),
            branch: self.branch.clone(),
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

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn convert(obj: &Map<String, Value>, line: u64, head: &Head) -> Converted {
    let Some(ty) = obj.get("type").and_then(Value::as_str) else {
        return Converted::Fail("record has no string `type`".into());
    };
    if NOISE.contains(&ty) {
        return Converted::Skip(ty.to_string());
    }
    let mut d = EventDraft {
        timestamp: stamp(obj.get("timestamp")),
        native_id: s(obj, "uuid").map(str::to_string),
        parent_native_id: s(obj, "parentUuid").map(str::to_string),
        agent: s(obj, "agentId").map(str::to_string),
        metadata: base_meta(obj, line, ty, head),
        ..Default::default()
    };
    let whole = || Value::Object(obj.clone());
    match ty {
        "user" | "assistant" => {
            let Some(msg) = obj.get("message").and_then(Value::as_object) else {
                return Converted::Fail(format!("{ty} record has no message object"));
            };
            let source_role = s(msg, "role").unwrap_or(ty);
            d.role = Role::parse(source_role);
            d.model = s(msg, "model").map(str::to_string);
            for (src, dst) in [
                ("id", "message_id"),
                ("stop_reason", "stop_reason"),
                ("usage", "usage"),
            ] {
                if let Some(v) = msg.get(src).filter(|v| !v.is_null()) {
                    d.metadata.insert(dst.into(), v.clone());
                }
            }
            d.content = parts_of(msg.get("content"));
            d.event_type = EventType::Message;
            let only = |f: fn(&Part) -> bool| !d.content.is_empty() && d.content.iter().all(f);
            if only(|p| matches!(p, Part::ToolResult { .. })) {
                d.metadata.insert("source_role".into(), json!(source_role));
                d.role = Role::Tool;
                d.event_type = EventType::ToolResult;
            } else if obj.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                d.event_type = EventType::Compaction;
            } else if d.role == Role::Assistant {
                let text = d.content.iter().any(|p| matches!(p, Part::Text { .. }));
                if !text && d.content.iter().any(|p| matches!(p, Part::ToolCall { .. })) {
                    d.event_type = EventType::ToolCall;
                } else if !text && only(is_reasoning) {
                    d.event_type = EventType::Reasoning;
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
        }
        "system" => {
            d.role = Role::System;
            d.event_type = if s(obj, "subtype") == Some("compact_boundary") {
                EventType::Compaction
            } else {
                EventType::SystemNote
            };
            d.content = vec![match s(obj, "content") {
                Some(t) => Part::text(t),
                None => Part::Opaque {
                    kind: format!("system:{}", s(obj, "subtype").unwrap_or("record")),
                    note: None,
                    raw: Some(whole()),
                },
            }];
        }
        "attachment" => {
            let att = obj.get("attachment").cloned().unwrap_or(Value::Null);
            let kind = att.get("type").and_then(Value::as_str).unwrap_or("unknown");
            d.role = Role::Other;
            d.event_type = EventType::Attachment;
            d.content = vec![Part::Opaque {
                kind: format!("attachment:{kind}"),
                note: None,
                raw: Some(att),
            }];
        }
        "summary" | "ai-title" | "custom-title" => {
            let text = ["summary", "aiTitle", "customTitle"]
                .iter()
                .find_map(|k| s(obj, k));
            d.role = Role::System;
            d.event_type = EventType::SystemNote;
            d.content = vec![match text {
                Some(t) => Part::text(t),
                None => Part::Opaque {
                    kind: ty.into(),
                    note: None,
                    raw: Some(whole()),
                },
            }];
        }
        other => {
            // Unknown record kind: preserved verbatim rather than guessed at.
            d.role = Role::Other;
            d.event_type = EventType::Opaque;
            d.content = vec![Part::Opaque {
                kind: other.into(),
                note: None,
                raw: Some(whole()),
            }];
        }
    }
    Converted::Event(d)
}

fn base_meta(obj: &Map<String, Value>, line: u64, ty: &str, head: &Head) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("source_line".into(), json!(line));
    m.insert("record_type".into(), json!(ty));
    for (src, dst) in COPIED {
        if let Some(v) = obj.get(*src).filter(|v| !v.is_null()) {
            m.insert((*dst).into(), v.clone());
        }
    }
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        m.insert("is_sidechain".into(), json!(true));
    }
    if let Some(a) = s(obj, "agentId") {
        m.insert("agent_id".into(), json!(a));
    }
    for (key, name, base) in [
        ("cwd", "cwd", &head.cwd),
        ("gitBranch", "git_branch", &head.branch),
        ("sessionId", "record_session_id", &head.session),
    ] {
        if let Some(v) = s(obj, key).filter(|v| Some(*v) != base.as_deref()) {
            m.insert(name.into(), json!(v));
        }
    }
    m
}

fn is_reasoning(p: &Part) -> bool {
    match p {
        Part::Reasoning { .. } => true,
        Part::Opaque { kind, .. } => kind == "thinking" || kind == "redacted_thinking",
        _ => false,
    }
}

fn parts_of(content: Option<&Value>) -> Vec<Part> {
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
    let opaque = |kind: &str, note: Option<String>| Part::Opaque {
        kind: kind.to_string(),
        note,
        raw: Some(block.clone()),
    };
    let Some(b) = block.as_object() else {
        return opaque("non_object_block", None);
    };
    let kind = s(b, "type").unwrap_or("untyped");
    match kind {
        "text" => Part::text(s(b, "text").unwrap_or("")),
        "thinking" => match s(b, "thinking") {
            Some(t) => Part::Reasoning {
                text: t.into(),
                visibility: ReasoningVisibility::Public,
            },
            // Signature-only blocks carry no readable reasoning; none is invented.
            None => Part::Opaque {
                kind: "thinking".into(),
                note: Some("no thinking text present in source".into()),
                raw: None,
            },
        },
        "tool_use" | "server_tool_use" | "mcp_tool_use" => Part::ToolCall {
            id: s(b, "id").map(str::to_string),
            name: s(b, "name").unwrap_or("").to_string(),
            arguments: b.get("input").cloned().unwrap_or(Value::Null),
        },
        "tool_result" => Part::ToolResult {
            tool_call_id: s(b, "tool_use_id").map(str::to_string),
            output: b.get("content").cloned().unwrap_or(Value::Null),
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        },
        // ponytail: base64 image/document payloads are kept inline as opaque raw
        // blocks (bounded by max_record_bytes); decode into artifacts if archives
        // with many screenshots make events too large.
        "image" | "document" => opaque(kind, None),
        other => opaque(other, None),
    }
}
