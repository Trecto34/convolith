//! Gemini web history from Google Takeout.
//!
//! Two shapes:
//!
//! * `gemini-takeout-conversation` — **verified against a real export** (a
//!   Takeout "Gemini in Workspace / Conversation History" folder): one JSON
//!   document per conversation, stored as `conversation_<N>.txt`, with `title`,
//!   `creation_time`, `last_modification_time` and `conversation_turns[]` of
//!   either `{user_turn:{prompt,turn_index,turn_last_modified,turn_deleted_time?}}`
//!   or `{system_turn:{text[{data|cards[{content}]}],images[],model_thoughts[]…}}` —
//!   user and model turns are separate array elements and `turn_index` is a
//!   sequence over both (user 0, model 1, user 2, …). The same index can repeat
//!   (edited/regenerated turns), so the identity of a turn is
//!   `<role>:<turn_index>:<occurrence>` where occurrence counts earlier turns of the
//!   same role and index in file order.
//!   Images are referenced by name (`conversation_<N>_turn_<T>_images_<I>`);
//!   their bytes are not imported. Per-turn times are *last-modified* times,
//!   recorded as such in `gemini_timestamp_kind`. There is no per-message id, so
//!   identity is `<conversation file id>:<creation_time>` plus the turn identity above.
//!   Because a user and a model turn are separate elements, no parent link is
//!   invented between them.
//! * `gemini-myactivity` — **unverified-against-real-export**: Takeout
//!   `My Activity/Gemini Apps/MyActivity.json`, entries
//!   `{header,title:"Prompted …",time,safeHtmlItem[{html}]}`. Entries have no
//!   ids, so identity is the provider `time` plus a hash of the entry title;
//!   other activity kinds are counted as skipped. `MyActivity.html` is
//!   inventoried as unsupported.

use super::webexport::{iso_stamp, str_of, stream_array};
use crate::model::{EventDraft, EventType, Part, ReasoningVisibility, Role};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct GeminiWebParser;

fn is_myactivity_json(p: &Probe) -> bool {
    p.filename().eq_ignore_ascii_case("MyActivity.json")
        && (p.head.contains("Gemini Apps") || p.full_path.contains("Gemini"))
}

fn is_myactivity_html(p: &Probe) -> bool {
    p.filename().eq_ignore_ascii_case("MyActivity.html")
        && (p.head.contains("Gemini Apps")
            || p.head.contains("My Activity")
            || p.full_path.to_ascii_lowercase().contains("gemini")
            || p.rel_path.to_ascii_lowercase().contains("gemini"))
}

impl SourceParser for GeminiWebParser {
    fn id(&self) -> &'static str {
        "gemini_takeout"
    }
    fn provider(&self) -> &'static str {
        "google"
    }
    fn application(&self) -> &'static str {
        "gemini"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            detect: true,
            parse: true,
            tool_calls: false,
            attachments: true,
            reasoning: true,
            streaming: false,
            partial: true,
        }
    }
    fn is_web_export(&self) -> bool {
        true
    }
    fn description(&self) -> &'static str {
        "Gemini web history from Google Takeout (Conversation History verified; My Activity HTML verified; My Activity JSON unverified)"
    }
    fn detect(&self, p: &Probe) -> Detection {
        let ext = p.ext();
        if (ext == "txt" || ext == "json") && p.head.contains("\"conversation_turns\"") {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "gemini-takeout-conversation",
                Confidence::Strong,
                "Takeout conversation document with conversation_turns",
            )
        } else if is_myactivity_html(p) {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "gemini-myactivity-html",
                Confidence::Strong,
                "Takeout My Activity (Gemini Apps) HTML",
            )
        } else if is_myactivity_json(p) {
            Detection::hit(
                self.id(),
                self.provider(),
                self.application(),
                "gemini-myactivity",
                Confidence::Weak,
                "Takeout My Activity (Gemini Apps) JSON; unverified against a real export",
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
        let ext = source
            .read_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let inner_or_disp = source
            .inner_path
            .as_deref()
            .unwrap_or(&source.display_path)
            .to_ascii_lowercase();
        if ext == "html" && inner_or_disp.ends_with("myactivity.html") {
            return parse_myactivity_html(source, sink);
        }
        if ext == "json" && inner_or_disp.ends_with("myactivity.json") {
            return parse_myactivity(source, sink);
        }
        let doc: Value = serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(
            &source.read_path,
        )?))
        .context("reading conversation document")?;
        let mut report = ParseReport {
            records_examined: 1,
            ..Default::default()
        };
        parse_conversation(source, &doc, sink, &mut report)?;
        Ok(report)
    }
}

fn file_id(source: &Source) -> String {
    let p = source.inner_path.as_deref().unwrap_or(&source.display_path);
    let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
    name.rsplit_once('.').map_or(name, |(s, _)| s).to_owned()
}

fn parse_conversation(
    source: &Source,
    doc: &Value,
    sink: &mut dyn EventSink,
    report: &mut ParseReport,
) -> Result<()> {
    let turns = doc
        .get("conversation_turns")
        .and_then(Value::as_array)
        .context("missing conversation_turns array (unrecognized Gemini export shape)")?;
    let fid = file_id(source);
    let created = doc.get("creation_time").and_then(Value::as_str);
    // `<file id>:<creation_time>` is account-scoped but stable across Takeouts.
    let key = created.map(|c| format!("gemini-takeout:{fid}:{c}"));
    let mut meta = ConversationMeta {
        provider: Some("google".into()),
        application: Some("gemini".into()),
        native_id: key.clone(),
        title: str_of(doc, "title"),
        started_at: Some(iso_stamp(doc.get("creation_time"))).filter(|s| s.utc.is_some()),
        ended_at: Some(iso_stamp(doc.get("last_modification_time"))).filter(|s| s.utc.is_some()),
        identity_hint: if key.is_some() {
            IdentityHint::Native
        } else {
            IdentityHint::Fingerprint
        },
        ..Default::default()
    };
    meta.metadata
        .insert("gemini_conversation_file".into(), json!(fid));
    sink.begin(meta)?;
    report.conversations += 1;
    let mut seen: std::collections::HashMap<(&str, i64), u32> = Default::default();
    for (i, t) in turns.iter().enumerate() {
        let (u, s) = (t.get("user_turn"), t.get("system_turn"));
        if u.is_none() && s.is_none() {
            report.records_failed += 1;
            report
                .notes
                .push(format!("turn {i}: neither user_turn nor system_turn"));
            continue;
        }
        for (role, turn) in [("user", u), ("model", s)] {
            let Some(turn) = turn else { continue };
            let idx = turn
                .get("turn_index")
                .and_then(Value::as_i64)
                .unwrap_or(i as i64);
            let occ = seen.entry((role, idx)).or_insert(0);
            let nid = key.as_ref().map(|k| format!("{k}:{role}:{idx}:{occ}"));
            *occ += 1;
            let mut d = if role == "user" {
                EventDraft::with_content(
                    Role::User,
                    EventType::Message,
                    vec![Part::text(
                        turn.get("prompt").and_then(Value::as_str).unwrap_or(""),
                    )],
                )
            } else {
                EventDraft::with_content(Role::Assistant, EventType::Message, model_parts(turn))
            };
            d.native_id = nid;
            stamp_turn(&mut d, turn);
            sink.emit(d)?;
            report.events += 1;
        }
    }
    sink.end()?;
    Ok(())
}

fn model_parts(s: &Value) -> Vec<Part> {
    let mut content = Vec::new();
    for th in s
        .get("model_thoughts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let g = |k| th.get(k).and_then(Value::as_str).unwrap_or("");
        content.push(Part::Reasoning {
            text: format!("{}\n\n{}", g("headline"), g("description")),
            visibility: ReasoningVisibility::Summary,
        });
    }
    for item in s
        .get("text")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(x) = item.get("data").and_then(Value::as_str) {
            content.push(Part::text(x));
        }
        for c in item
            .get("cards")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(x) = c.get("content").and_then(Value::as_str) {
                content.push(Part::text(x));
            }
        }
    }
    for img in s
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        content.push(Part::Image {
            artifact: None,
            mime: None,
            filename: None,
            source_ref: img.as_str().map(str::to_owned),
        });
    }
    content
}

fn stamp_turn(d: &mut EventDraft, turn: &Value) {
    d.timestamp = iso_stamp(turn.get("turn_last_modified"));
    d.metadata
        .insert("gemini_timestamp_kind".into(), json!("turn_last_modified"));
    if let Some(x) = turn.get("turn_index") {
        d.metadata.insert("gemini_turn_index".into(), x.clone());
    }
    if let Some(x) = turn.get("turn_deleted_time").filter(|x| !x.is_null()) {
        d.metadata
            .insert("gemini_turn_deleted_time".into(), x.clone());
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// ---- Gemini Apps Takeout (MyActivity.html) parser ------------------------

pub fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            let mut entity = String::new();
            let mut found_semi = false;
            while let Some(&next) = chars.peek() {
                if next == ';' {
                    chars.next();
                    found_semi = true;
                    break;
                } else if next.is_ascii_alphanumeric() || next == '#' {
                    entity.push(chars.next().unwrap());
                    if entity.len() > 12 {
                        break;
                    }
                } else {
                    break;
                }
            }
            if found_semi {
                match entity.as_str() {
                    "quot" => out.push('"'),
                    "amp" => out.push('&'),
                    "apos" => out.push('\''),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "nbsp" => out.push(' '),
                    _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                        if let Ok(cp) = u32::from_str_radix(&entity[2..], 16) {
                            if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                                continue;
                            }
                        }
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                    _ if entity.starts_with('#') => {
                        if let Ok(cp) = entity[1..].parse::<u32>() {
                            if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                                continue;
                            }
                        }
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                    _ => {
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                }
            } else {
                out.push('&');
                out.push_str(&entity);
            }
        } else if c == '\u{a0}' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

fn month_number(s: &str) -> Option<u32> {
    let clean = s.trim_matches('.').to_ascii_lowercase();
    match clean.as_str() {
        "jan" | "january" | "janeiro" => Some(1),
        "feb" | "february" | "fev" | "fevereiro" => Some(2),
        "mar" | "march" | "marco" | "março" => Some(3),
        "apr" | "april" | "abr" | "abril" => Some(4),
        "may" | "mai" | "maio" => Some(5),
        "jun" | "june" | "junho" => Some(6),
        "jul" | "july" | "julho" => Some(7),
        "aug" | "august" | "ago" | "agosto" => Some(8),
        "sep" | "september" | "set" | "setembro" => Some(9),
        "oct" | "october" | "out" | "outubro" => Some(10),
        "nov" | "november" | "novembro" => Some(11),
        "dec" | "december" | "dez" | "dezembro" => Some(12),
        _ => None,
    }
}

fn parse_tz_offset(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() || s == "Z" || s == "z" || s == "UTC" || s == "GMT" {
        return "+00:00".to_string();
    }
    match s.to_ascii_uppercase().as_str() {
        "EDT" => return "-04:00".to_string(),
        "EST" => return "-05:00".to_string(),
        "CDT" => return "-05:00".to_string(),
        "CST" => return "-06:00".to_string(),
        "MDT" => return "-06:00".to_string(),
        "MST" => return "-07:00".to_string(),
        "PDT" => return "-07:00".to_string(),
        "PST" => return "-08:00".to_string(),
        "BRT" => return "-03:00".to_string(),
        "BRST" => return "-02:00".to_string(),
        "BST" => return "+01:00".to_string(),
        "CET" => return "+01:00".to_string(),
        "CEST" => return "+02:00".to_string(),
        "JST" => return "+09:00".to_string(),
        _ => {}
    }
    let rest = s
        .strip_prefix("GMT")
        .or_else(|| s.strip_prefix("UTC"))
        .unwrap_or(s);
    if rest.starts_with('+') || rest.starts_with('-') {
        let sign = &rest[..1];
        let num = rest[1..].trim();
        if num.contains(':') {
            let mut parts = num.split(':');
            let h = parts.next().unwrap_or("0");
            let m = parts.next().unwrap_or("0");
            return format!(
                "{}{:02}:{:02}",
                sign,
                h.parse::<u32>().unwrap_or(0),
                m.parse::<u32>().unwrap_or(0)
            );
        } else if let Ok(val) = num.parse::<u32>() {
            if num.len() <= 2 {
                return format!("{sign}{val:02}:00");
            } else if num.len() == 4 {
                return format!("{sign}{:02}:{:02}", val / 100, val % 100);
            }
        }
    }
    "+00:00".to_string()
}

pub fn parse_takeout_timestamp(raw: &str) -> Option<(crate::timeutil::Utc, String)> {
    let s = raw.replace(['\u{202f}', '\u{a0}'], " ");
    let s = s.trim();
    if let Some(utc) = crate::timeutil::parse_rfc3339(s) {
        return Some((utc, s.to_string()));
    }

    static RE_EN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_en = RE_EN.get_or_init(|| {
        regex::Regex::new(r"(?i)^([A-Za-z]{3,9})\s+(\d{1,2}),\s+(\d{4}),\s+(\d{1,2}):(\d{2}):(\d{2})\s*(AM|PM)?(?:\s+(?:GMT|UTC)?([+-]\d{1,2}(?::?\d{2})?|Z|[A-Za-z]{1,5}))?$").unwrap()
    });
    if let Some(cap) = re_en.captures(s) {
        let mon = month_number(cap.get(1)?.as_str())?;
        let day: u32 = cap.get(2)?.as_str().parse().ok()?;
        let year: i64 = cap.get(3)?.as_str().parse().ok()?;
        let mut hr: u32 = cap.get(4)?.as_str().parse().ok()?;
        let mn: u32 = cap.get(5)?.as_str().parse().ok()?;
        let sc: u32 = cap.get(6)?.as_str().parse().ok()?;
        if let Some(ampm) = cap.get(7) {
            let a = ampm.as_str().to_ascii_uppercase();
            if a == "PM" && hr < 12 {
                hr += 12;
            } else if a == "AM" && hr == 12 {
                hr = 0;
            }
        }
        let tz = cap
            .get(8)
            .map(|m| parse_tz_offset(m.as_str()))
            .unwrap_or_else(|| "+00:00".to_string());
        let rfc3339 = format!("{year:04}-{mon:02}-{day:02}T{hr:02}:{mn:02}:{sc:02}{tz}");
        let utc = crate::timeutil::parse_rfc3339(&rfc3339)?;
        return Some((utc, raw.to_string()));
    }

    static RE_PT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_pt = RE_PT.get_or_init(|| {
        regex::Regex::new(r"(?i)^(\d{1,2})\s+de\s+([A-Za-zçãé]{3,9}\.?)\s+de\s+(\d{4}),?\s+(\d{1,2}):(\d{2}):(\d{2})(?:\s+(?:GMT|UTC)?([+-]\d{1,2}(?::?\d{2})?|Z|[A-Za-z]{1,5}))?$").unwrap()
    });
    if let Some(cap) = re_pt.captures(s) {
        let day: u32 = cap.get(1)?.as_str().parse().ok()?;
        let mon = month_number(cap.get(2)?.as_str())?;
        let year: i64 = cap.get(3)?.as_str().parse().ok()?;
        let hr: u32 = cap.get(4)?.as_str().parse().ok()?;
        let mn: u32 = cap.get(5)?.as_str().parse().ok()?;
        let sc: u32 = cap.get(6)?.as_str().parse().ok()?;
        let tz = cap
            .get(7)
            .map(|m| parse_tz_offset(m.as_str()))
            .unwrap_or_else(|| "+00:00".to_string());
        let rfc3339 = format!("{year:04}-{mon:02}-{day:02}T{hr:02}:{mn:02}:{sc:02}{tz}");
        let utc = crate::timeutil::parse_rfc3339(&rfc3339)?;
        return Some((utc, raw.to_string()));
    }

    None
}

#[derive(Default)]
struct ConvertState {
    out: String,
    in_pre: bool,
    list_stack: Vec<ListKind>,
}

enum ListKind {
    Unordered,
    Ordered(usize),
}

fn convert_node(handle: tl::NodeHandle, parser: &tl::Parser, state: &mut ConvertState) {
    let node = match handle.get(parser) {
        Some(n) => n,
        None => return,
    };
    if let Some(tag) = node.as_tag() {
        let name = tag.name().as_utf8_str().to_ascii_lowercase();
        match name.as_str() {
            "h1" => {
                state.out.push_str("\n\n# ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "h2" => {
                state.out.push_str("\n\n## ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "h3" => {
                state.out.push_str("\n\n### ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "h4" => {
                state.out.push_str("\n\n#### ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "h5" => {
                state.out.push_str("\n\n##### ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "h6" => {
                state.out.push_str("\n\n###### ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "p" => {
                state.out.push_str("\n\n");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "br" => {
                state.out.push('\n');
            }
            "hr" => {
                state.out.push_str("\n\n---\n\n");
            }
            "strong" | "b" => {
                state.out.push_str("**");
                convert_children(tag, parser, state);
                state.out.push_str("**");
            }
            "em" | "i" => {
                state.out.push('*');
                convert_children(tag, parser, state);
                state.out.push('*');
            }
            "pre" => {
                state.out.push_str("\n\n```\n");
                let prev = state.in_pre;
                state.in_pre = true;
                convert_children(tag, parser, state);
                state.in_pre = prev;
                state.out.push_str("\n```\n\n");
            }
            "code" => {
                if state.in_pre {
                    convert_children(tag, parser, state);
                } else {
                    state.out.push('`');
                    convert_children(tag, parser, state);
                    state.out.push('`');
                }
            }
            "blockquote" => {
                state.out.push_str("\n\n> ");
                convert_children(tag, parser, state);
                state.out.push_str("\n\n");
            }
            "ul" => {
                state.out.push('\n');
                state.list_stack.push(ListKind::Unordered);
                convert_children(tag, parser, state);
                state.list_stack.pop();
                state.out.push('\n');
            }
            "ol" => {
                state.out.push('\n');
                state.list_stack.push(ListKind::Ordered(1));
                convert_children(tag, parser, state);
                state.list_stack.pop();
                state.out.push('\n');
            }
            "li" => {
                let indent = "  ".repeat(state.list_stack.len().saturating_sub(1));
                let prefix = match state.list_stack.last_mut() {
                    Some(ListKind::Ordered(n)) => {
                        let p = format!("\n{indent}{n}. ");
                        *n += 1;
                        p
                    }
                    _ => format!("\n{indent}- "),
                };
                state.out.push_str(&prefix);
                convert_children(tag, parser, state);
                state.out.push('\n');
            }
            "a" => {
                let href = tag
                    .attributes()
                    .get("href")
                    .flatten()
                    .map(|b| b.as_utf8_str().to_string());
                if let Some(h) = href {
                    state.out.push('[');
                    convert_children(tag, parser, state);
                    state.out.push_str(&format!("]({h})"));
                } else {
                    convert_children(tag, parser, state);
                }
            }
            "table" => {
                render_table(tag, parser, &mut state.out);
            }
            _ => {
                convert_children(tag, parser, state);
            }
        }
    } else if let Some(bytes) = node.as_raw() {
        if let Some(s) = bytes.try_as_utf8_str() {
            state.out.push_str(&decode_html_entities(s));
        }
    }
}

fn convert_children(tag: &tl::HTMLTag, parser: &tl::Parser, state: &mut ConvertState) {
    for ch in tag.children().top().iter() {
        convert_node(*ch, parser, state);
    }
}

fn render_table(tag: &tl::HTMLTag, parser: &tl::Parser, out: &mut String) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    collect_table_rows(tag, parser, &mut rows);
    if rows.is_empty() {
        return;
    }
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return;
    }
    out.push_str("\n\n");
    let r0 = &rows[0];
    out.push_str("| ");
    for c in 0..col_count {
        out.push_str(r0.get(c).map(String::as_str).unwrap_or(""));
        out.push_str(" | ");
    }
    out.push('\n');
    out.push_str("| ");
    for _ in 0..col_count {
        out.push_str("--- | ");
    }
    out.push('\n');
    for r in &rows[1..] {
        out.push_str("| ");
        for c in 0..col_count {
            out.push_str(r.get(c).map(String::as_str).unwrap_or(""));
            out.push_str(" | ");
        }
        out.push('\n');
    }
    out.push_str("\n\n");
}

fn collect_table_rows(tag: &tl::HTMLTag, parser: &tl::Parser, rows: &mut Vec<Vec<String>>) {
    for ch in tag.children().top().iter() {
        if let Some(n) = ch.get(parser) {
            if let Some(t) = n.as_tag() {
                if t.name().as_utf8_str().eq_ignore_ascii_case("tr") {
                    let mut cells = Vec::new();
                    for cell_h in t.children().top().iter() {
                        if let Some(cn) = cell_h.get(parser) {
                            if let Some(ct) = cn.as_tag() {
                                let cname = ct.name().as_utf8_str();
                                if cname.eq_ignore_ascii_case("th")
                                    || cname.eq_ignore_ascii_case("td")
                                {
                                    let txt = decode_html_entities(&cn.inner_text(parser));
                                    cells.push(txt.replace('|', "\\|").trim().to_string());
                                }
                            }
                        }
                    }
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                } else {
                    collect_table_rows(t, parser, rows);
                }
            }
        }
    }
}

fn normalize_markdown(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut blank_count = 0;
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                out.push('\n');
            }
        } else {
            blank_count = 0;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

pub fn html_to_normalized_markdown(html_snippet: &str) -> String {
    let trimmed = html_snippet.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let dom = match tl::parse(trimmed, tl::ParserOptions::default()) {
        Ok(d) => d,
        Err(_) => return strip_tags(trimmed),
    };
    let parser = dom.parser();
    let mut state = ConvertState::default();
    for handle in dom.children() {
        convert_node(*handle, parser, &mut state);
    }
    normalize_markdown(&state.out)
}

struct ParsedCard {
    app_id: Option<String>,
    timestamp_utc: crate::timeutil::Utc,
    timestamp_orig: String,
    prompt: String,
    attachments: Vec<String>,
    response_markdown: String,
    doc_index: usize,
}

fn is_conversational_card(raw: &str) -> bool {
    raw.contains("Prompted")
        || raw.contains("Branched")
        || raw.contains("Fez uma pergunta")
        || raw.contains("Perguntou")
}

fn extract_app_id(card_html: &str) -> Option<String> {
    let needle = "https://gemini.google.com/app/";
    let pos = card_html.find(needle)?;
    let rest = &card_html[pos + needle.len()..];
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn find_staging_root(read_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut curr = read_path.parent();
    while let Some(p) = curr {
        if let Some(file_name) = p.file_name().and_then(|s| s.to_str()) {
            if let Some(rest) = file_name.strip_prefix('d') {
                if let Some((depth_str, seq_str)) = rest.split_once('-') {
                    if depth_str.chars().all(|c| c.is_ascii_digit())
                        && seq_str.chars().all(|c| c.is_ascii_digit())
                    {
                        return p.parent().map(|parent| parent.to_path_buf());
                    }
                }
            }
        }
        curr = p.parent();
    }
    None
}

fn collect_known_members(source: &Source) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    if let Some(root) = find_staging_root(&source.read_path) {
        if let Ok(entries) = walkdir::WalkDir::new(&root)
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            for entry in entries {
                if entry.file_type().is_file() {
                    set.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
    } else if let Some(parent) = source.read_path.parent() {
        if let Ok(rd) = std::fs::read_dir(parent) {
            for entry in rd.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    set.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    set
}

fn extract_card_attachments(
    card_html: &str,
    base_member_dir: &str,
    known_members: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut atts = Vec::new();
    let mut seen = std::collections::HashSet::new();

    static RE_FILE_REF: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_ref = RE_FILE_REF.get_or_init(|| {
        regex::Regex::new(r#"(?i)(?:href|src)=["']([^"']+\.(?:jpg|jpeg|png|webp|gif|heic|svg|pdf|txt|csv|xlsx|docx|json|md|markdown|yaml|yml))["']"#).unwrap()
    });

    for cap in re_ref.captures_iter(card_html) {
        if let Some(m) = cap.get(1) {
            let raw_ref = m.as_str();
            if raw_ref.starts_with("http:")
                || raw_ref.starts_with("https:")
                || raw_ref.starts_with("mailto:")
                || raw_ref.starts_with('#')
            {
                continue;
            }
            let decoded = percent_decode(raw_ref);
            let filename = decoded
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&decoded)
                .to_string();

            let resolved_filename = if known_members.contains(&filename) {
                filename
            } else if filename.ends_with(".jpeg")
                && known_members.contains(&format!("{}.jpg", &filename[..filename.len() - 5]))
            {
                format!("{}.jpg", &filename[..filename.len() - 5])
            } else if filename.ends_with(".jpg")
                && known_members.contains(&format!("{}.jpeg", &filename[..filename.len() - 4]))
            {
                format!("{}.jpeg", &filename[..filename.len() - 4])
            } else if let Some((stem, _)) = filename.rsplit_once('.') {
                if known_members.contains(stem) {
                    stem.to_string()
                } else if filename.ends_with(".jpeg") {
                    format!("{}.jpg", &filename[..filename.len() - 5])
                } else {
                    filename
                }
            } else if filename.ends_with(".jpeg") {
                format!("{}.jpg", &filename[..filename.len() - 5])
            } else {
                filename
            };

            let member_path = if !base_member_dir.is_empty() {
                format!("{base_member_dir}/{resolved_filename}")
            } else {
                resolved_filename
            };
            if seen.insert(member_path.clone()) {
                atts.push(member_path);
            }
        }
    }
    atts
}

fn clean_user_prompt(user_html: &str) -> Option<String> {
    let mut text = user_html.trim();
    let mut matched = false;
    for prefix in &[
        "Prompted\u{a0}",
        "Prompted ",
        "Branched\u{a0}",
        "Branched ",
        "Fez uma pergunta:\u{a0}",
        "Fez uma pergunta: ",
        "Perguntou:\u{a0}",
        "Perguntou: ",
        "Preguntó:\u{a0}",
        "Preguntó: ",
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            matched = true;
            break;
        }
    }
    if !matched {
        return None;
    }

    // Strip trailing Takeout attachment boilerplate, e.g.:
    // "<br>1 attachment.<br>- <a href="...">...</a>"
    static RE_ATT_BOILERPLATE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_att = RE_ATT_BOILERPLATE.get_or_init(|| {
        regex::Regex::new(r#"(?is)(?:<br\s*/?>|\n)?\s*\d+\s+attachments?\.?\s*(?:<br\s*/?>|\n)?.*"#)
            .unwrap()
    });
    let mut s = text.to_string();
    if let Some(m) = re_att.find(&s) {
        s.truncate(m.start());
    }

    // Strip any remaining HTML tags and decode entities
    let stripped = strip_tags(&s);
    let decoded = decode_html_entities(&stripped);
    Some(decoded.trim().to_string())
}

fn first_line_title(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or(prompt).trim();
    if first.chars().count() > 100 {
        let t: String = first.chars().take(100).collect();
        format!("{t}…")
    } else {
        first.to_string()
    }
}

fn extract_card_info(
    card_html: &str,
    doc_index: usize,
    base_member_dir: &str,
    known_members: &std::collections::HashSet<String>,
) -> Result<Option<ParsedCard>> {
    let app_id = extract_app_id(card_html);

    // Find the body content cell
    static RE_BODY_CELL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_body = RE_BODY_CELL.get_or_init(|| {
        regex::Regex::new(
            r#"(?s)<div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">(.*?)</div>"#,
        )
        .unwrap()
    });
    let Some(cap_body) = re_body.captures(card_html) else {
        return Ok(None);
    };
    let body_inner = cap_body.get(1).unwrap().as_str();

    // Find timestamp line in body
    static RE_TS_SPLIT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_ts = RE_TS_SPLIT.get_or_init(|| {
        regex::Regex::new(r#"(?i)<br\s*/?>\s*(([A-Za-z]{3,9}\s+\d{1,2},\s+\d{4}|\d{1,2}\s+de\s+[A-Za-zçãé]{3,9}\.?\s+de\s+\d{4}|\d{4}-\d{2}-\d{2})[^\n<]*?\d{1,2}:\d{2}:\d{2}[^\n<]*?)\s*<br\s*/?>"#).unwrap()
    });
    let Some(cap_ts) = re_ts.captures(body_inner) else {
        return Ok(None);
    };

    let ts_match = cap_ts.get(1).unwrap();
    let ts_full = cap_ts.get(0).unwrap();
    let Some((ts_utc, ts_orig)) = parse_takeout_timestamp(ts_match.as_str()) else {
        return Ok(None);
    };

    let user_html = &body_inner[..ts_full.start()];
    let asst_html = &body_inner[ts_full.end()..];

    let Some(prompt) = clean_user_prompt(user_html) else {
        return Ok(None);
    };
    let attachments = extract_card_attachments(card_html, base_member_dir, known_members);
    let response_markdown = html_to_normalized_markdown(asst_html);

    Ok(Some(ParsedCard {
        app_id,
        timestamp_utc: ts_utc,
        timestamp_orig: ts_orig,
        prompt,
        attachments,
        response_markdown,
        doc_index,
    }))
}

fn parse_myactivity_html(source: &Source, sink: &mut dyn EventSink) -> Result<ParseReport> {
    let mut report = ParseReport::default();
    let content = std::fs::read_to_string(&source.read_path).context("reading MyActivity.html")?;
    let dom = tl::parse(&content, tl::ParserOptions::default())
        .map_err(|e| anyhow::anyhow!("failed to parse HTML with tl: {e:?}"))?;
    let parser = dom.parser();

    let inner_path = source.inner_path.as_deref().unwrap_or(&source.display_path);
    let base_member_dir = inner_path
        .rsplit_once(['/', '\\'])
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let known_members = collect_known_members(source);

    let card_handles: Vec<_> = match dom.query_selector("div.outer-cell") {
        Some(iter) => iter.collect(),
        None => return Ok(report),
    };

    let mut conversations: std::collections::BTreeMap<String, Vec<ParsedCard>> =
        std::collections::BTreeMap::new();

    for (doc_index, handle) in card_handles.into_iter().enumerate() {
        report.records_examined += 1;
        let card_node = match handle.get(parser) {
            Some(n) => n,
            None => {
                report.records_failed += 1;
                continue;
            }
        };

        let raw_html = card_node.inner_html(parser);
        if !is_conversational_card(&raw_html) {
            // Account management action (Cleared, Created, Selected, Gave, Used)
            report.records_skipped += 1;
            continue;
        }

        match extract_card_info(&raw_html, doc_index, base_member_dir, &known_members) {
            Ok(Some(card)) => {
                let conv_key = match &card.app_id {
                    Some(id) => format!("gemini-app:{id}"),
                    None => format!(
                        "gemini-activity:{}",
                        crate::id::id("", &[&card.timestamp_orig, &card.prompt])
                    ),
                };
                conversations.entry(conv_key).or_default().push(card);
            }
            Ok(None) => {
                report.records_skipped += 1;
            }
            Err(e) => {
                report.records_failed += 1;
                report.notes.push(format!("card {doc_index}: {e:#}"));
            }
        }
    }

    // Sort conversations deterministically by earliest timestamp
    let mut conv_list: Vec<(String, Vec<ParsedCard>)> = conversations.into_iter().collect();
    conv_list.sort_by(|a, b| {
        let t_a = a.1.first().map(|c| c.timestamp_utc);
        let t_b = b.1.first().map(|c| c.timestamp_utc);
        t_a.cmp(&t_b).then_with(|| a.0.cmp(&b.0))
    });

    for (conv_key, mut cards) in conv_list {
        // Sort cards within conversation chronologically (oldest-first)
        cards.sort_by(|a, b| {
            a.timestamp_utc
                .cmp(&b.timestamp_utc)
                .then_with(|| b.doc_index.cmp(&a.doc_index))
        });

        // Deduplicate duplicate cards within same conversation
        let before_dedup = cards.len();
        cards.dedup_by(|a, b| a.timestamp_orig == b.timestamp_orig && a.prompt == b.prompt);
        let dups = before_dedup - cards.len();
        report.records_skipped += dups as u64;

        if cards.is_empty() {
            continue;
        }

        let first = cards.first().unwrap();
        let last = cards.last().unwrap();
        let title = first_line_title(&cards[0].prompt);

        let mut meta = ConversationMeta {
            provider: Some("google".into()),
            application: Some("gemini".into()),
            native_id: Some(conv_key.clone()),
            title: if title.is_empty() { None } else { Some(title) },
            started_at: Some(crate::timeutil::Stamp::from_utc(
                first.timestamp_utc,
                crate::timeutil::TimestampConfidence::Exact,
                Some(first.timestamp_orig.clone()),
            )),
            ended_at: Some(crate::timeutil::Stamp::from_utc(
                last.timestamp_utc,
                crate::timeutil::TimestampConfidence::Exact,
                Some(last.timestamp_orig.clone()),
            )),
            identity_hint: IdentityHint::Native,
            ..Default::default()
        };
        if let Some(id) = &first.app_id {
            meta.metadata.insert("gemini_app_id".into(), json!(id));
        }
        sink.begin(meta)?;
        report.conversations += 1;

        for (turn_idx, card) in cards.iter().enumerate() {
            // User event
            let mut user_content = vec![Part::text(&card.prompt)];
            for att in &card.attachments {
                let fname = att.rsplit(['/', '\\']).next().unwrap_or(att).to_string();
                let ext = std::path::Path::new(&fname)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_img = matches!(
                    ext.as_str(),
                    "jpg" | "jpeg" | "png" | "webp" | "gif" | "heic" | "svg"
                );
                if is_img {
                    user_content.push(Part::Image {
                        artifact: None,
                        mime: crate::artifacts::guess_mime(&fname),
                        filename: Some(fname),
                        source_ref: Some(att.clone()),
                    });
                } else {
                    user_content.push(Part::FileRef {
                        path: Some(att.clone()),
                        artifact: None,
                        mime: crate::artifacts::guess_mime(&fname),
                        filename: Some(fname),
                    });
                }
            }

            let mut user_draft =
                EventDraft::with_content(Role::User, EventType::Message, user_content);
            user_draft.native_id = Some(format!("{conv_key}:turn:{turn_idx}:user"));
            if turn_idx > 0 {
                user_draft.parent_native_id =
                    Some(format!("{conv_key}:turn:{}:model", turn_idx - 1));
            }
            user_draft.timestamp = crate::timeutil::Stamp::from_utc(
                card.timestamp_utc,
                crate::timeutil::TimestampConfidence::Exact,
                Some(card.timestamp_orig.clone()),
            );
            sink.emit(user_draft)?;
            report.events += 1;

            // Assistant event
            if !card.response_markdown.is_empty() {
                let mut asst_draft = EventDraft::with_content(
                    Role::Assistant,
                    EventType::Message,
                    vec![Part::text(&card.response_markdown)],
                );
                asst_draft.native_id = Some(format!("{conv_key}:turn:{turn_idx}:model"));
                asst_draft.parent_native_id = Some(format!("{conv_key}:turn:{turn_idx}:user"));
                asst_draft.timestamp = crate::timeutil::Stamp::unknown();
                sink.emit(asst_draft)?;
                report.events += 1;
            }
        }

        sink.end()?;
    }

    Ok(report)
}

fn parse_myactivity(source: &Source, sink: &mut dyn EventSink) -> Result<ParseReport> {
    let mut report = ParseReport::default();
    stream_array(&source.read_path, "My Activity", &mut |e| {
        report.records_examined += 1;
        let title = e.get("title").and_then(Value::as_str).unwrap_or("");
        let Some(prompt) = title.strip_prefix("Prompted ") else {
            report.records_skipped += 1;
            return Ok(());
        };
        let Some(time) = e.get("time").and_then(Value::as_str) else {
            report.records_failed += 1;
            report
                .notes
                .push("activity entry without time: no stable identity".into());
            return Ok(());
        };
        let key = format!("gemini-myactivity:{}", crate::id::id("", &[time, title]));
        sink.begin(ConversationMeta {
            provider: Some("google".into()),
            application: Some("gemini".into()),
            native_id: Some(key.clone()),
            started_at: Some(iso_stamp(e.get("time"))).filter(|s| s.utc.is_some()),
            identity_hint: IdentityHint::Native,
            ..Default::default()
        })?;
        report.conversations += 1;
        let mut u =
            EventDraft::with_content(Role::User, EventType::Message, vec![Part::text(prompt)]);
        u.native_id = Some(format!("{key}:user"));
        u.timestamp = iso_stamp(e.get("time"));
        sink.emit(u)?;
        report.events += 1;
        let html: Vec<&str> = e
            .get("safeHtmlItem")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|i| i.get("html").and_then(Value::as_str))
            .collect();
        if !html.is_empty() {
            let raw = html.join("\n");
            let mut a = EventDraft::with_content(
                Role::Assistant,
                EventType::Message,
                vec![
                    Part::text(strip_tags(&raw)),
                    Part::Opaque {
                        kind: "gemini_safe_html".into(),
                        note: None,
                        raw: Some(Value::String(raw)),
                    },
                ],
            );
            a.native_id = Some(format!("{key}:model"));
            a.parent_native_id = Some(format!("{key}:user"));
            a.timestamp = iso_stamp(e.get("time"));
            sink.emit(a)?;
            report.events += 1;
        }
        sink.end()?;
        Ok(())
    })?;
    Ok(report)
}
