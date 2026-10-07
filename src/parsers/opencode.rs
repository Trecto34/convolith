//! OpenCode parser: the file store `~/.local/share/opencode/storage/{session,
//! message,part}/**.json`, the SQLite store `opencode.db`, and the JSON bundle
//! written by `opencode export`.
//!
//! Format knowledge (session / message / part shapes, part kinds, tool state
//! machine, compaction markers) follows xhluca/session-migrate (MIT licence,
//! <https://github.com/xhluca/session-migrate>, `docs/opencode-source-exploration.md`
//! and `formats/opencode.py`); the semantics were ported and adapted to
//! convolith's canonical model, not copied line by line.
//!
//! Mapping rules, in short:
//! * a *session* file / `session` row / export bundle is the source of one
//!   conversation; its messages and parts are read from the sibling
//!   `message/<session>/` and `part/<message>/` directories (file store) or the
//!   `message` and `part` tables (database). Message and part files met on their
//!   own are claimed but contribute nothing: the session source owns them;
//! * one message -> one event carrying text / reasoning / tool-call / file
//!   parts in part order; a finished tool part additionally yields a `tool_result`
//!   event (role `tool`) so call and result are both structured;
//! * `step-start`, `step-finish` and `snapshot` parts are bookkeeping and are
//!   counted as skipped; unknown part kinds are kept as opaque parts;
//! * timestamps are the source's epoch milliseconds or unknown, never invented.

use super::jsonl::{stamp, Stream, Tally};
use super::rows::{oversize_cells, query_maps};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use crate::sqlite::{is_sqlite_bytes, Db};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct OpenCodeParser;

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

/// `ses_…`, `msg_…`, `prt_…` ids end up in file paths; accept only plain tokens.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Session,
    Message,
    Part,
    Bundle,
    Db,
}

fn compact(head: &str) -> String {
    head.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Classify a probe from provider-specific id prefixes and key combinations;
/// the head may be a truncated prefix of a large file.
fn classify(p: &Probe) -> Option<Kind> {
    if is_sqlite_bytes(&p.head_bytes) {
        let h = p.head.replace(['`', '"', '[', ']'], "");
        let tables = ["session", "message", "part"]
            .iter()
            .all(|t| h.contains(&format!("CREATE TABLE {t}")));
        let named = p.filename().starts_with("opencode") && p.has_component("opencode");
        return (tables || named).then_some(Kind::Db);
    }
    if p.ext() != "json" {
        return None;
    }
    let h = compact(&p.head);
    let has = |needles: &[&str]| needles.iter().all(|n| h.contains(n));
    if has(&[
        "\"id\":\"prt_",
        "\"messageID\":\"msg_",
        "\"sessionID\":\"ses_",
        "\"type\":\"",
    ]) {
        Some(Kind::Part)
    } else if has(&["\"id\":\"msg_", "\"sessionID\":\"ses_", "\"role\":\""]) {
        Some(Kind::Message)
    } else if h.starts_with("{\"info\":{") && has(&["\"id\":\"ses_"]) {
        Some(Kind::Bundle)
    } else if h.starts_with('{')
        && has(&["\"id\":\"ses_", "\"time\":{"])
        && (h.contains("\"directory\":\"") || h.contains("\"projectID\":\""))
        && !h.contains("\"sessionID\"")
    {
        Some(Kind::Session)
    } else {
        None
    }
}

impl SourceParser for OpenCodeParser {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn provider(&self) -> &'static str {
        "opencode"
    }
    fn application(&self) -> &'static str {
        "opencode"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: true,
            attachments: false,
            reasoning: true,
            streaming: true,
            // Step bookkeeping, snapshots, todos and binary cells are not imported.
            partial: true,
        }
    }
    fn description(&self) -> &'static str {
        "OpenCode sessions (storage/{session,message,part}/**.json, opencode.db, export bundles)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir {
            return Detection::none(self.id());
        }
        let Some(kind) = classify(probe) else {
            return Detection::none(self.id());
        };
        let in_store =
            probe.has_component("opencode") && (kind == Kind::Db || probe.has_component("storage"));
        let (format, reason) = match kind {
            Kind::Db => (
                "opencode-sqlite",
                "SQLite database with OpenCode session/message/part tables",
            ),
            Kind::Session => (
                "opencode-storage",
                "session record with ses_ id, directory and time",
            ),
            Kind::Message => (
                "opencode-storage",
                "message record with msg_ id and ses_ sessionID",
            ),
            Kind::Part => ("opencode-storage", "part record with prt_/msg_/ses_ ids"),
            Kind::Bundle => ("opencode-export", "export bundle: info with ses_ id"),
        };
        let d = Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            format,
            // A tables-only guess (file named opencode*.db with no schema text in
            // the probe window) is a claim about the path, not the payload.
            if in_store {
                Confidence::Certain
            } else {
                Confidence::Strong
            },
            reason,
        );
        if in_store {
            d.with_reason("located under an opencode data directory")
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
        let mut head = [0u8; 16];
        let n = std::fs::File::open(&source.read_path)
            .and_then(|mut f| f.read(&mut head))
            .with_context(|| format!("open {}", source.read_path.display()))?;
        if is_sqlite_bytes(&head[..n]) {
            return parse_db(ctx, source, sink);
        }
        let mut tally = Tally::default();
        let value = match read_json(&source.read_path, ctx.max_record_bytes()) {
            Ok(Value::Object(o)) => o,
            Ok(_) => {
                tally.fail_at("file", "document is not a JSON object");
                return Ok(tally.into_totals(0, 0, 0));
            }
            Err(e) => {
                tally.fail_at("file", format!("{e:#}"));
                return Ok(tally.into_totals(0, 0, 0));
            }
        };
        if value.contains_key("messages") && value.get("info").is_some_and(Value::is_object) {
            return parse_bundle(value, sink, tally);
        }
        if s(&value, "messageID").is_some() || s(&value, "role").is_some() {
            // Message and part files belong to their session's source.
            let mut r = ParseReport::default();
            r.notes.push(
                "message/part file: imported through its session file, which owns the transcript"
                    .into(),
            );
            return Ok(r);
        }
        parse_session_file(ctx, source, value, sink, tally)
    }
}

/// Read one JSON document of at most `max` bytes.
fn read_json(path: &Path, max: usize) -> Result<Value> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > max {
        return Err(anyhow!("document exceeds the {max}-byte record limit"));
    }
    let body = buf.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&buf);
    serde_json::from_slice(body).map_err(|e| anyhow!("invalid JSON: {e}"))
}

fn sorted_json_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    v
}

// ------------------------------------------------------------ file store

/// `<root>/session/<project>/<id>.json` -> `<root>`.
fn storage_root(session_file: &Path) -> Option<PathBuf> {
    let session_dir = session_file.parent()?.parent()?;
    (session_dir.file_name()? == "session").then(|| session_dir.parent().map(Path::to_path_buf))?
}

fn parse_session_file(
    ctx: &mut dyn ParseContext,
    source: &Source,
    info: Map<String, Value>,
    sink: &mut dyn EventSink,
    mut tally: Tally,
) -> Result<ParseReport> {
    let mut notes = Vec::new();
    let stem = source
        .read_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string);
    let Some(sid) = s(&info, "id")
        .map(str::to_string)
        .or(stem)
        .filter(|i| safe_id(i))
    else {
        tally.fail_at("session", "no usable session id");
        return Ok(tally.into_totals(0, 0, 0));
    };
    let mut stream = Stream::new(sink);
    let meta = session_meta(&info);
    match storage_root(&source.read_path) {
        None => notes.push("session file is outside a storage/session/<project>/ tree".into()),
        Some(root) => {
            let dir = root.join("message").join(&sid);
            if !dir.is_dir() {
                notes.push(format!("no message directory for session {sid}"));
            }
            for file in sorted_json_files(&dir) {
                let at = format!(
                    "message {}",
                    file.file_name().unwrap_or_default().to_string_lossy()
                );
                let minfo = match read_json(&file, ctx.max_record_bytes()) {
                    Ok(Value::Object(o)) => o,
                    Ok(_) => {
                        tally.fail_at(&at, "not a JSON object");
                        continue;
                    }
                    Err(e) => {
                        tally.fail_at(&at, format!("{e:#}"));
                        continue;
                    }
                };
                let mid = s(&minfo, "id")
                    .map(str::to_string)
                    .or_else(|| file.file_stem().map(|x| x.to_string_lossy().into_owned()));
                let Some(mid) = mid.filter(|i| safe_id(i)) else {
                    tally.fail_at(&at, "no usable message id");
                    continue;
                };
                let mut parts = Vec::new();
                for pf in sorted_json_files(&root.join("part").join(&mid)) {
                    match read_json(&pf, ctx.max_record_bytes()) {
                        Ok(v @ Value::Object(_)) => parts.push(v),
                        Ok(_) => tally.fail_at(format!("{at} part"), "not a JSON object"),
                        Err(e) => tally.fail_at(
                            format!(
                                "part {}",
                                pf.file_name().unwrap_or_default().to_string_lossy()
                            ),
                            format!("{e:#}"),
                        ),
                    }
                }
                if !stream.begun() {
                    stream.begin(meta.clone())?;
                }
                for d in convert_message(&minfo, &mid, &parts, &mut tally) {
                    stream.push(d)?;
                }
            }
        }
    }
    stream.finish(meta)?;
    let mut report = tally.into_report(&stream);
    report.notes.extend(notes);
    Ok(report)
}

/// `opencode export` bundle: `{info, messages: [{info, parts}]}`.
fn parse_bundle(
    bundle: Map<String, Value>,
    sink: &mut dyn EventSink,
    mut tally: Tally,
) -> Result<ParseReport> {
    let info = bundle
        .get("info")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let meta = session_meta(&info);
    let mut stream = Stream::new(sink);
    stream.begin(meta.clone())?;
    for (i, m) in bundle
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("message #{i}");
        let (Some(minfo), Some(parts)) = (
            m.get("info").and_then(Value::as_object),
            m.get("parts").and_then(Value::as_array),
        ) else {
            tally.fail_at(&at, "entry lacks info object or parts array");
            continue;
        };
        let mid = s(minfo, "id").unwrap_or("").to_string();
        for d in convert_message(minfo, &mid, parts, &mut tally) {
            stream.push(d)?;
        }
    }
    stream.finish(meta)?;
    Ok(tally.into_report(&stream))
}

// -------------------------------------------------------------- database

fn parse_db(
    ctx: &mut dyn ParseContext,
    source: &Source,
    sink: &mut dyn EventSink,
) -> Result<ParseReport> {
    let db = Db::open_snapshot(&source.read_path, ctx.staging_dir())?;
    for t in ["session", "message", "part"] {
        if !db.table_exists(t) {
            return Err(anyhow!("not an OpenCode database: table `{t}` is missing"));
        }
    }
    let max = ctx.max_record_bytes();
    let mut tally = Tally::default();
    let (mut conversations, mut events, mut calls) = (0, 0, 0);
    let order = if db.columns("session")?.iter().any(|c| c == "time_created") {
        "time_created, id"
    } else {
        "id"
    };
    let mut sessions = Vec::new();
    if let Err(e) = query_maps(
        &db.conn,
        &format!("select * from session order by {order}"),
        &[],
        max,
        |row| {
            sessions.push(row);
            Ok(())
        },
    ) {
        tally.fail_at("session table", format!("{e:#}"));
    }
    for row in sessions {
        let info = session_row_info(&row);
        let sid = s(&row, "id").unwrap_or("").to_string();
        let at = format!("session {sid}");
        if !oversize_cells(&row).is_empty() {
            tally.fail_at(&at, "session row has an oversize cell");
            continue;
        }
        let meta = session_meta(&info);
        let mut stream = Stream::new(sink);
        let mut ids = Vec::new();
        let r = query_maps(
            &db.conn,
            "select * from message where session_id = ?1 order by time_created, id",
            &[&sid],
            max,
            |m| {
                ids.push(m);
                Ok(())
            },
        );
        if let Err(e) = r {
            tally.fail_at(&at, format!("reading messages: {e:#}"));
        }
        for m in ids {
            let mid = s(&m, "id").unwrap_or("").to_string();
            let at = format!("message {mid}");
            if !oversize_cells(&m).is_empty() {
                tally.fail_at(&at, "message row has an oversize cell");
                continue;
            }
            let Some(minfo) = json_cell(&m, "data").map(|mut o| {
                o.entry("id").or_insert(json!(mid));
                if !o.contains_key("time") {
                    o.insert("time".into(), json!({ "created": m.get("time_created") }));
                }
                o
            }) else {
                tally.fail_at(&at, "message data is not a JSON object");
                continue;
            };
            let mut parts = Vec::new();
            let r = query_maps(
                &db.conn,
                "select * from part where message_id = ?1 order by id",
                &[&mid],
                max,
                |p| {
                    let pat = format!("part {}", s(&p, "id").unwrap_or("?"));
                    if !oversize_cells(&p).is_empty() {
                        tally.fail_at(&pat, "part row has an oversize cell");
                    } else if let Some(mut o) = json_cell(&p, "data") {
                        o.entry("id")
                            .or_insert(p.get("id").cloned().unwrap_or(Value::Null));
                        parts.push(Value::Object(o));
                    } else {
                        tally.fail_at(&pat, "part data is not a JSON object");
                    }
                    Ok(())
                },
            );
            if let Err(e) = r {
                tally.fail_at(&at, format!("reading parts: {e:#}"));
            }
            if !stream.begun() {
                stream.begin(meta.clone())?;
            }
            for d in convert_message(&minfo, &mid, &parts, &mut tally) {
                stream.push(d)?;
            }
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

fn json_cell(row: &Map<String, Value>, col: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(row.get(col)?.as_str()?) {
        Ok(Value::Object(o)) => Some(o),
        _ => None,
    }
}

/// Session table row -> the file-store `Session.Info` shape.
fn session_row_info(row: &Map<String, Value>) -> Map<String, Value> {
    let mut o = Map::new();
    for (col, key) in [
        ("id", "id"),
        ("project_id", "projectID"),
        ("parent_id", "parentID"),
        ("slug", "slug"),
        ("directory", "directory"),
        ("title", "title"),
        ("version", "version"),
    ] {
        if let Some(v) = row.get(col).filter(|v| !v.is_null()) {
            o.insert(key.into(), v.clone());
        }
    }
    o.insert(
        "time".into(),
        json!({ "created": row.get("time_created"), "updated": row.get("time_updated") }),
    );
    o
}

// ------------------------------------------------------------ conversion

fn session_meta(info: &Map<String, Value>) -> ConversationMeta {
    let id = s(info, "id")
        .filter(|i| is_globally_unique(i))
        .map(str::to_string);
    let time = |k: &str| {
        let st = stamp(info.get("time").and_then(|t| t.get(k)));
        st.utc.is_some().then_some(st)
    };
    let mut metadata = Map::new();
    for (src, dst) in [
        ("version", "version"),
        ("projectID", "project_id"),
        ("parentID", "parent_session_id"),
        ("slug", "slug"),
    ] {
        if let Some(v) = info.get(src).filter(|v| !v.is_null()) {
            metadata.insert(dst.into(), v.clone());
        }
    }
    ConversationMeta {
        native_id: id.clone(),
        native_session_id: id.clone(),
        title: s(info, "title").map(str::to_string),
        working_directory: s(info, "directory").map(str::to_string),
        started_at: time("created"),
        ended_at: time("updated"),
        metadata,
        identity_hint: if id.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    }
}

/// Part kinds that only record step boundaries or git snapshots.
const NOISE: &[&str] = &["step-start", "step-finish", "snapshot"];

fn opaque(kind: &str, raw: &Map<String, Value>) -> Part {
    Part::Opaque {
        kind: kind.to_string(),
        note: None,
        raw: Some(Value::Object(raw.clone())),
    }
}

/// One message plus its parts -> the message event and one `tool_result` event
/// per finished tool part. Part-level problems are tallied, never fatal.
fn convert_message(
    info: &Map<String, Value>,
    mid: &str,
    parts: &[Value],
    tally: &mut Tally,
) -> Vec<EventDraft> {
    let role_name = s(info, "role").unwrap_or("");
    let model_obj = info.get("model").and_then(Value::as_object);
    let mut d = EventDraft {
        role: Role::parse(role_name),
        timestamp: stamp(info.get("time").and_then(|t| t.get("created"))),
        native_id: Some(mid.to_string()).filter(|m| !m.is_empty()),
        parent_native_id: s(info, "parentID").map(str::to_string),
        agent: s(info, "agent").map(str::to_string),
        model: s(info, "modelID")
            .or_else(|| model_obj.and_then(|m| s(m, "modelID")))
            .map(str::to_string),
        ..Default::default()
    };
    d.metadata.insert("record_type".into(), json!("message"));
    d.metadata.insert("source_role".into(), json!(role_name));
    if let Some(p) = s(info, "providerID").or_else(|| model_obj.and_then(|m| s(m, "providerID"))) {
        d.metadata.insert("provider_id".into(), json!(p));
    }
    for k in [
        "mode", "cost", "tokens", "finish", "error", "summary", "path", "system",
    ] {
        if let Some(v) = info.get(k).filter(|v| !v.is_null()) {
            d.metadata.insert(k.into(), v.clone());
        }
    }
    let mut results = Vec::new();
    let mut unfinished = Vec::new();
    let mut compaction = false;
    for part in parts {
        let Some(p) = part.as_object() else {
            tally.fail_at(format!("message {mid}"), "part is not a JSON object");
            continue;
        };
        let ty = s(p, "type").unwrap_or("untyped");
        let part_id = s(p, "id");
        match ty {
            t if NOISE.contains(&t) => tally.skip(t),
            "text" => match s(p, "text") {
                Some(t) => d.content.push(Part::text(t)),
                None => tally.skip("empty-text"),
            },
            "reasoning" => d.content.push(match s(p, "text") {
                Some(t) => Part::Reasoning {
                    text: t.into(),
                    visibility: ReasoningVisibility::Public,
                },
                None => Part::Opaque {
                    kind: "reasoning".into(),
                    note: Some("no reasoning text present in source".into()),
                    raw: None,
                },
            }),
            "tool" => {
                let state = p.get("state").and_then(Value::as_object);
                let call_id = s(p, "callID").map(str::to_string);
                let name = s(p, "tool").unwrap_or("").to_string();
                d.content.push(Part::ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: state
                        .and_then(|st| st.get("input"))
                        .cloned()
                        .unwrap_or(Value::Null),
                });
                d.tool_call_ids.extend(call_id.clone());
                let status = state.and_then(|st| s(st, "status"));
                let (Some(st), Some("completed" | "error")) = (state, status) else {
                    // pending / running: the call never finished in the source.
                    unfinished.push(json!(call_id));
                    continue;
                };
                let failed = status == Some("error");
                let output = st
                    .get(if failed { "error" } else { "output" })
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut r = EventDraft {
                    role: Role::Tool,
                    event_type: EventType::ToolResult,
                    timestamp: stamp(st.get("time").and_then(|t| t.get("end"))),
                    native_id: part_id.map(|i| format!("{i}:result")),
                    parent_native_id: d.native_id.clone(),
                    tool_call_ids: call_id.clone().into_iter().collect(),
                    content: vec![Part::ToolResult {
                        tool_call_id: call_id,
                        output,
                        is_error: failed,
                    }],
                    ..Default::default()
                };
                r.metadata
                    .insert("record_type".into(), json!("tool_result"));
                r.metadata.insert("tool".into(), json!(name));
                for k in ["title", "metadata", "attachments"] {
                    if let Some(v) = st.get(k).filter(|v| !v.is_null()) {
                        r.metadata.insert(k.into(), v.clone());
                    }
                }
                results.push(r);
            }
            "file" => {
                let url = s(p, "url").unwrap_or("");
                d.content
                    .push(if url.is_empty() || url.starts_with("data:") {
                        // Inline payloads stay as the raw part rather than being guessed at.
                        opaque("file", p)
                    } else {
                        Part::FileRef {
                            path: Some(url.to_string()),
                            artifact: None,
                            mime: s(p, "mime").map(str::to_string),
                            filename: s(p, "filename").map(str::to_string),
                        }
                    });
            }
            "compaction" => {
                compaction = true;
                d.content.push(opaque("compaction", p));
            }
            other => d.content.push(opaque(other, p)),
        }
    }
    if !unfinished.is_empty() {
        d.metadata
            .insert("unfinished_tool_calls".into(), Value::Array(unfinished));
    }
    d.event_type = if compaction || info.get("summary").and_then(Value::as_bool) == Some(true) {
        EventType::Compaction
    } else if d.role == Role::Assistant {
        let text = d.content.iter().any(|p| matches!(p, Part::Text { .. }));
        if !text && d.content.iter().any(|p| matches!(p, Part::ToolCall { .. })) {
            EventType::ToolCall
        } else if !text
            && !d.content.is_empty()
            && d.content.iter().all(|p| {
                matches!(p, Part::Reasoning { .. })
                    || matches!(p, Part::Opaque { kind, .. } if kind == "reasoning")
            })
        {
            EventType::Reasoning
        } else {
            EventType::Message
        }
    } else {
        EventType::Message
    };
    let mut out = vec![d];
    out.extend(results);
    out
}
