//! Helpers shared by the line-oriented (JSONL) parsers: a bounded line reader,
//! per-record accounting, a buffer that defers `begin` until the conversation
//! header is known, and probe-side record sniffing.

use crate::model::{EventDraft, EventType};
use crate::parser::{ConversationMeta, EventSink};
use crate::source::{ParseReport, Probe};
use crate::timeutil::{parse_json_timestamp, Stamp, TimestampConfidence};
use anyhow::Result;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::{self, BufRead};

/// How many individual failure messages are kept in the report notes; the rest
/// are only counted.
const MAX_FAILURE_NOTES: usize = 20;

pub struct Line {
    /// 1-based physical line number (blank lines count).
    pub number: u64,
    /// `Some(bytes)` when the line exceeded the limit and was discarded unread.
    pub oversize: Option<u64>,
}

/// Reads newline-delimited records without ever holding more than `max` bytes.
pub struct LineReader<R: BufRead> {
    inner: R,
    buf: Vec<u8>,
    max: usize,
    number: u64,
}

impl<R: BufRead> LineReader<R> {
    pub fn new(inner: R, max: usize) -> Self {
        LineReader {
            inner,
            buf: Vec::new(),
            max: max.max(1),
            number: 0,
        }
    }

    /// Bytes of the line returned by the last [`next_line`](Self::next_line)
    /// (empty for an oversize line).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Next non-blank line, or `None` at EOF.
    pub fn next_line(&mut self) -> io::Result<Option<Line>> {
        loop {
            self.buf.clear();
            let (mut total, mut oversize, mut any) = (0u64, false, false);
            loop {
                let chunk = self.inner.fill_buf()?;
                if chunk.is_empty() {
                    break;
                }
                any = true;
                let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (chunk.len(), false),
                };
                total += take as u64;
                if !oversize {
                    // +2 leaves room for the line terminator.
                    if self.buf.len() + take > self.max + 2 {
                        oversize = true;
                        self.buf.clear();
                    } else {
                        self.buf.extend_from_slice(&chunk[..take]);
                    }
                }
                self.inner.consume(take);
                if done {
                    break;
                }
            }
            if !any {
                return Ok(None);
            }
            self.number += 1;
            if oversize {
                return Ok(Some(Line {
                    number: self.number,
                    oversize: Some(total),
                }));
            }
            if self.number == 1 && self.buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                self.buf.drain(..3);
            }
            while matches!(self.buf.last(), Some(b'\n' | b'\r' | b' ' | b'\t')) {
                self.buf.pop();
            }
            if self.buf.is_empty() {
                continue;
            }
            return Ok(Some(Line {
                number: self.number,
                oversize: None,
            }));
        }
    }
}

/// Per-source record accounting. Every non-blank line must end up as exactly
/// one of: an emitted event, a skip, or a failure.
#[derive(Default)]
pub struct Tally {
    pub failed: u64,
    pub skipped: u64,
    failures: Vec<String>,
    skipped_by_kind: BTreeMap<String, u64>,
}

impl Tally {
    pub fn fail(&mut self, line: u64, why: impl Display) {
        self.failed += 1;
        if self.failures.len() < MAX_FAILURE_NOTES {
            self.failures.push(format!("line {line}: {why}"));
        }
    }

    /// Failure located by a free-form label (a file name, a table row) rather
    /// than a line number.
    pub fn fail_at(&mut self, at: impl Display, why: impl Display) {
        self.failed += 1;
        if self.failures.len() < MAX_FAILURE_NOTES {
            self.failures.push(format!("{at}: {why}"));
        }
    }

    pub fn skip(&mut self, kind: &str) {
        self.skipped += 1;
        *self.skipped_by_kind.entry(kind.to_string()).or_default() += 1;
    }

    pub fn into_report(self, stream: &Stream<'_>) -> ParseReport {
        self.into_totals(stream.conversations, stream.events, stream.tool_calls)
    }

    /// Same as [`into_report`](Self::into_report) for sources that stream several
    /// conversations through several [`Stream`]s.
    pub fn into_totals(self, conversations: u64, events: u64, tool_calls: u64) -> ParseReport {
        let mut notes = self.failures;
        if self.failed as usize > notes.len() {
            notes.push(format!(
                "{} more record failure(s) not listed",
                self.failed as usize - notes.len()
            ));
        }
        if !self.skipped_by_kind.is_empty() {
            let kinds: Vec<String> = self
                .skipped_by_kind
                .iter()
                .map(|(k, n)| format!("{k}={n}"))
                .collect();
            notes.push(format!(
                "records intentionally not imported: {}",
                kinds.join(", ")
            ));
        }
        ParseReport {
            conversations,
            events,
            tool_calls,
            records_examined: events + self.skipped + self.failed,
            records_failed: self.failed,
            records_skipped: self.skipped,
            notes,
        }
    }
}

/// Forwards drafts to the sink, holding them back until `begin` has been called
/// so a parser can learn the conversation header from the first useful record.
pub struct Stream<'a> {
    sink: &'a mut dyn EventSink,
    begun: bool,
    pending: Vec<EventDraft>,
    pub events: u64,
    pub tool_calls: u64,
    pub conversations: u64,
}

impl<'a> Stream<'a> {
    pub fn new(sink: &'a mut dyn EventSink) -> Self {
        Stream {
            sink,
            begun: false,
            pending: Vec::new(),
            events: 0,
            tool_calls: 0,
            conversations: 0,
        }
    }

    pub fn begun(&self) -> bool {
        self.begun
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn begin(&mut self, meta: ConversationMeta) -> Result<()> {
        if self.begun {
            return Ok(());
        }
        self.sink.begin(meta)?;
        self.begun = true;
        self.conversations += 1;
        for d in std::mem::take(&mut self.pending) {
            self.sink.emit(d)?;
        }
        Ok(())
    }

    pub fn push(&mut self, draft: EventDraft) -> Result<()> {
        self.events += 1;
        if draft.event_type == EventType::ToolCall {
            self.tool_calls += 1;
        }
        if self.begun {
            self.sink.emit(draft)
        } else {
            self.pending.push(draft);
            Ok(())
        }
    }

    /// Close the conversation. A source that produced no events yields no
    /// conversation at all.
    pub fn finish(&mut self, fallback: ConversationMeta) -> Result<()> {
        if !self.begun && !self.pending.is_empty() {
            self.begin(fallback)?;
        }
        if self.begun {
            self.sink.end()?;
        }
        Ok(())
    }
}

/// Timestamp exactly as the source states it. Absent stays unknown; present but
/// unparsable keeps the literal text with unknown confidence. Nothing is guessed.
pub fn stamp(v: Option<&Value>) -> Stamp {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Stamp::unknown();
    };
    let original = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match parse_json_timestamp(v) {
        Some((utc, conf)) => Stamp::from_utc(utc, conf, Some(original)),
        None => Stamp {
            utc: None,
            original: Some(original),
            confidence: TimestampConfidence::Unknown,
        },
    }
}

/// Non-empty string field.
pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Complete JSON-object records from the probe window (at most `limit`). The
/// last line is dropped when the window cut it short.
pub fn probe_records(probe: &Probe, limit: usize) -> Vec<Map<String, Value>> {
    let whole_file = probe.head_bytes.len() as u64 >= probe.size;
    let mut lines: Vec<&[u8]> = probe.head_bytes.split(|&b| b == b'\n').collect();
    if !whole_file {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| l.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(l))
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .take(limit)
        .filter_map(|l| match serde_json::from_slice::<Value>(l) {
            Ok(Value::Object(o)) => Some(o),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reader_bounds_lines_and_skips_blanks() {
        let data = b"\xEF\xBB\xBF{\"a\":1}\r\n\n  \n0123456789ABCDEF\n{\"b\":2}";
        let mut r = LineReader::new(Cursor::new(&data[..]), 10);
        let l = r.next_line().unwrap().unwrap();
        assert_eq!((l.number, l.oversize), (1, None));
        assert_eq!(r.bytes(), b"{\"a\":1}");
        let l = r.next_line().unwrap().unwrap();
        assert_eq!((l.number, l.oversize), (4, Some(17)));
        assert!(r.bytes().is_empty());
        let l = r.next_line().unwrap().unwrap();
        assert_eq!((l.number, l.oversize), (5, None));
        assert_eq!(r.bytes(), b"{\"b\":2}");
        assert!(r.next_line().unwrap().is_none());
    }

    #[test]
    fn stamp_never_fabricates() {
        assert_eq!(stamp(None).confidence, TimestampConfidence::Unknown);
        let bad = stamp(Some(&Value::String("yesterday".into())));
        assert!(bad.utc.is_none());
        assert_eq!(bad.original.as_deref(), Some("yesterday"));
        let ok = stamp(Some(&Value::String("2026-01-02T03:04:05.5Z".into())));
        assert_eq!(ok.confidence, TimestampConfidence::Exact);
    }
}
