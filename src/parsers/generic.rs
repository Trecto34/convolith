use crate::model::{EventDraft, EventType, Part, Role};
use crate::parser::{ConversationMeta, EventSink, ParseContext, SourceParser};
use crate::parsers::jsonl::LineReader;
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use crate::timeutil::Stamp;
use anyhow::{bail, Context, Result};
use serde::de::{Error as _, SeqAccess, Visitor};
use serde_json::Value;
use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::rc::Rc;

pub struct GenericJsonlParser;
pub struct GenericJsonParser;

fn capability() -> Capabilities {
    Capabilities {
        detect: true,
        parse: true,
        tool_calls: false,
        attachments: false,
        reasoning: false,
        streaming: true,
        partial: true,
    }
}
fn hit(p: &Probe, id: &str, format: &str) -> Detection {
    if looks_like_messages(&p.head) {
        Detection::hit(
            id,
            "unknown",
            "generic",
            format,
            Confidence::Weak,
            "records contain role and textual content",
        )
    } else {
        Detection::none(id)
    }
}
/// Role/content evidence in the probe window: a JSONL line, an element of a
/// (possibly truncated) top-level array, or one small whole object.
fn looks_like_messages(head: &str) -> bool {
    let is_msg = |v: &Value| clear_message(v).is_some();
    if head.lines().take(8).any(|l| {
        serde_json::from_str::<Value>(l)
            .map(|v| is_msg(&v))
            .unwrap_or(false)
    }) {
        return true;
    }
    if serde_json::from_str::<Value>(head).is_ok_and(|v| is_msg(&v)) {
        return true;
    }
    let Some(mut rest) = head.trim_start().strip_prefix('[') else {
        return false;
    };
    for _ in 0..8 {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        let mut it = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        match it.next() {
            Some(Ok(v)) if is_msg(&v) => return true,
            Some(Ok(_)) => rest = &rest[it.byte_offset()..],
            _ => return false,
        }
    }
    false
}
pub(super) fn clear_message(v: &Value) -> Option<(&str, &str)> {
    let role = v.get("role")?.as_str()?;
    let content = v.get("content").or_else(|| v.get("text"))?;
    let text = content.as_str()?;
    if text.trim().is_empty() {
        None
    } else {
        Some((role, text))
    }
}
fn emit(v: &Value, sink: &mut dyn EventSink, report: &mut ParseReport) -> Result<()> {
    let Some((role, text)) = clear_message(v) else {
        report.records_skipped += 1;
        return Ok(());
    };
    let mut d = EventDraft::with_content(
        Role::parse(role),
        EventType::Message,
        vec![Part::text(text)],
    );
    if let Some(t) = v
        .get("timestamp")
        .or_else(|| v.get("created_at"))
        .and_then(crate::timeutil::parse_json_timestamp)
    {
        d.timestamp = Stamp::from_utc(t.0, t.1, None);
    }
    if let Some(id) = v.get("id").and_then(Value::as_str) {
        d.native_id = Some(id.into());
    }
    d.metadata
        .insert("generic_confidence".into(), Value::String("low".into()));
    d.metadata.insert(
        "generic_status".into(),
        Value::String("partially parsed".into()),
    );
    sink.emit(d)?;
    report.events += 1;
    Ok(())
}
/// One JSON value per line, each bounded by `max_record_bytes`. A line that is
/// oversize, not UTF-8 or not JSON is a failed record; the import continues.
fn parse_lines(
    ctx: &dyn ParseContext,
    source: &Source,
    sink: &mut dyn EventSink,
    r: &mut ParseReport,
) -> Result<()> {
    let f = File::open(&source.read_path)?;
    let mut reader = LineReader::new(BufReader::new(f), ctx.max_record_bytes());
    let mut notes = 0;
    while let Some(line) = reader.next_line()? {
        let why = match line.oversize {
            Some(n) => Some(format!("{n} bytes exceeds the record limit")),
            None => match serde_json::from_slice::<Value>(reader.bytes()) {
                Ok(v) => {
                    emit(&v, sink, r)?;
                    None
                }
                Err(e) => Some(e.to_string()),
            },
        };
        if let Some(why) = why {
            r.records_failed += 1;
            if notes < 20 {
                notes += 1;
                r.notes.push(format!("line {}: {why}", line.number));
            }
        }
    }
    Ok(())
}
/// Reader that fails once more than `max` bytes were read since the last reset,
/// so one record can never buffer more than the record limit.
struct Capped<R> {
    inner: R,
    used: Rc<Cell<u64>>,
    max: u64,
}
impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.used.set(self.used.get() + n as u64);
        if self.used.get() > self.max {
            return Err(io::Error::other("record exceeds the record size limit"));
        }
        Ok(n)
    }
}
struct Elements<'a> {
    sink: &'a mut dyn EventSink,
    report: &'a mut ParseReport,
    used: Rc<Cell<u64>>,
}
impl<'de> Visitor<'de> for Elements<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of messages")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        loop {
            self.used.set(0);
            let Some(v) = seq.next_element::<Value>()? else {
                return Ok(());
            };
            emit(&v, self.sink, self.report).map_err(|e| A::Error::custom(format!("{e:#}")))?;
        }
    }
}
/// Skip BOM and whitespace; the next byte, if any, is left unread.
fn first_byte(r: &mut impl BufRead) -> io::Result<Option<u8>> {
    loop {
        let buf = r.fill_buf()?;
        let Some(&b) = buf.first() else {
            return Ok(None);
        };
        let n = if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else if b.is_ascii_whitespace() {
            1
        } else {
            return Ok(Some(b));
        };
        r.consume(n);
    }
}
fn begin(sink: &mut dyn EventSink, source: &Source) -> Result<()> {
    let mut m = ConversationMeta {
        provider: Some("unknown".into()),
        application: Some("generic-json".into()),
        identity_hint: crate::parser::IdentityHint::Coordinates,
        ..Default::default()
    };
    m.native_id = Some(source.source_id.clone());
    m.title = Some(source.display_path.clone());
    sink.begin(m)
}
impl SourceParser for GenericJsonlParser {
    fn id(&self) -> &'static str {
        "generic_jsonl"
    }
    fn provider(&self) -> &'static str {
        "unknown"
    }
    fn application(&self) -> &'static str {
        "generic-jsonl"
    }
    fn capabilities(&self) -> Capabilities {
        capability()
    }
    fn description(&self) -> &'static str {
        "Conservative role/content JSON Lines fallback"
    }
    fn detect(&self, p: &Probe) -> Detection {
        if p.ext() == "jsonl" || p.ext() == "ndjson" {
            hit(p, self.id(), "generic-jsonl")
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
        let mut r = ParseReport::default();
        begin(sink, source)?;
        parse_lines(ctx, source, sink, &mut r)?;
        sink.end()?;
        r.conversations = 1;
        Ok(r)
    }
}
impl SourceParser for GenericJsonParser {
    fn id(&self) -> &'static str {
        "generic_json"
    }
    fn provider(&self) -> &'static str {
        "unknown"
    }
    fn application(&self) -> &'static str {
        "generic-json"
    }
    fn capabilities(&self) -> Capabilities {
        capability()
    }
    fn description(&self) -> &'static str {
        "Conservative role/content JSON fallback"
    }
    fn detect(&self, p: &Probe) -> Detection {
        if p.ext() == "json" {
            hit(p, self.id(), "generic-json")
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
        let mut r = ParseReport::default();
        begin(sink, source)?;
        let mut file = BufReader::new(File::open(&source.read_path)?);
        let used = Rc::new(Cell::new(0));
        let capped = |f: BufReader<File>| Capped {
            inner: f,
            used: used.clone(),
            max: ctx.max_record_bytes() as u64,
        };
        match first_byte(&mut file)? {
            Some(b'[') => {
                let mut de = serde_json::Deserializer::from_reader(capped(file));
                serde::Deserializer::deserialize_seq(
                    &mut de,
                    Elements {
                        sink,
                        report: &mut r,
                        used: used.clone(),
                    },
                )
                .context("reading top-level messages array")?;
            }
            Some(b'{') => {
                // One object, or JSON Lines saved with a .json name.
                let mut de = serde_json::Deserializer::from_reader(capped(file));
                match serde::Deserialize::deserialize(&mut de)
                    .and_then(|v: Value| de.end().map(|_| v))
                {
                    Ok(v) => emit(&v, sink, &mut r)?,
                    Err(_) => parse_lines(ctx, source, sink, &mut r)?,
                }
            }
            _ => bail!("top level is neither an array nor an object"),
        }
        sink.end()?;
        r.conversations = 1;
        Ok(r)
    }
}
