//! MiniMax Code (`mcode`) session parser: `~/.minimax/v2/sessions/YYYY/MM/DD/
//! <time>-session_<base64 id>/{messages.jsonl, snapshots/g*--ctx_*.jsonl}`.
//!
//! Each line is `{"message_id", "turn_id", "message": {...}}` where `message`
//! is a pi-ai style message (`user` / `assistant` / `toolResult` / `custom` /
//! `compactionSummary`, content blocks `text` / `thinking` / `toolCall`), so the
//! message mapping is shared with the Pi parser. `messages.jsonl` is the live
//! context; `snapshots/` keep the earlier generations that compaction replaced.
//! Both belong to the same conversation (the id encoded in the directory name)
//! and a message keeps its `message_id` across them, so overlapping records
//! collapse on import instead of being counted twice.
//!
//! Everything else in the store (manifests, `llm-call.json`, locators, the
//! SQLite runtime index, tool-output spills, logs, skills, install files) is not
//! conversation content; see [`known_unsupported`].

use super::jsonl::{probe_records, stamp, LineReader, Stream, Tally};
use super::pi::{convert_message, copy};
use crate::dedup::is_globally_unique;
use crate::model::{EventDraft, EventType};
use crate::parser::{ConversationMeta, EventSink, IdentityHint, ParseContext, SourceParser};
use crate::source::{Capabilities, Confidence, Detection, ParseReport, Probe, Source};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct MiniMaxParser;

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

/// `messages.jsonl`, or `snapshots/g<generation>--ctx_<id>.jsonl`.
fn is_history_file(name: &str, parent: Option<&str>) -> bool {
    name == "messages.jsonl"
        || (parent == Some("snapshots")
            && name.starts_with('g')
            && name.contains("--ctx_")
            && name.ends_with(".jsonl"))
}

fn is_record(o: &Map<String, Value>) -> bool {
    s(o, "message_id").is_some()
        && s(o, "turn_id").is_some()
        && o.get("message")
            .and_then(Value::as_object)
            .is_some_and(|m| s(m, "role").is_some())
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for c in text.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// `mvs_<hex>` from the `<time>-session_<base64 of mvs_<hex>>` directory name.
fn session_id(path: &Path) -> Option<String> {
    let session_dir = path
        .ancestors()
        .filter_map(|a| a.file_name()?.to_str())
        .find(|n| n.contains("-session_"))?;
    let encoded = session_dir.rsplit_once("-session_")?.1;
    let id = base64_decode(encoded)
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|d| d.starts_with("mvs_"))
        .unwrap_or_else(|| encoded.to_string());
    is_globally_unique(&id).then_some(id)
}

impl SourceParser for MiniMaxParser {
    fn id(&self) -> &'static str {
        "minimax"
    }
    fn provider(&self) -> &'static str {
        "minimax"
    }
    fn application(&self) -> &'static str {
        "minimax-code"
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
        "MiniMax Code session history (~/.minimax/v2/sessions/**/messages.jsonl, snapshots/*.jsonl)"
    }

    fn detect(&self, probe: &Probe) -> Detection {
        if probe.is_dir || probe.ext() != "jsonl" {
            return Detection::none(self.id());
        }
        if !is_history_file(probe.filename(), probe.parent_name.as_deref()) {
            return Detection::none(self.id());
        }
        let records = probe_records(probe, 20);
        let oversized_first = records.is_empty()
            && probe.head.starts_with("{\"message_id\":")
            && probe.head.contains("\"turn_id\":");
        if !records.iter().any(is_record) && !oversized_first {
            return Detection::none(self.id());
        }
        let in_store = probe.has_component(".minimax") && probe.has_component("sessions");
        Detection::hit(
            self.id(),
            self.provider(),
            self.application(),
            "minimax-session-jsonl",
            if in_store {
                Confidence::Certain
            } else {
                Confidence::Strong
            },
            "records with message_id/turn_id and a pi-style message object",
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
        // Same id from both the live file and its snapshots: one conversation.
        let id =
            session_id(&source.read_path).or_else(|| session_id(Path::new(&source.display_path)));
        let meta = ConversationMeta {
            native_id: id.clone(),
            native_session_id: id.clone(),
            identity_hint: if id.is_some() {
                IdentityHint::Native
            } else {
                IdentityHint::Fingerprint
            },
            ..Default::default()
        };
        stream.begin(meta)?;

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
            match convert(&obj, line.number) {
                Ok(d) => stream.push(d)?,
                Err(why) => tally.fail(line.number, why),
            }
        }
        stream.finish(ConversationMeta::default())?;
        Ok(tally.into_report(&stream))
    }
}

fn convert(obj: &Map<String, Value>, line: u64) -> Result<EventDraft, String> {
    let id = s(obj, "message_id").ok_or("record has no message_id")?;
    let msg = obj
        .get("message")
        .and_then(Value::as_object)
        .ok_or("record has no message object")?;
    let mut d = EventDraft {
        timestamp: stamp(msg.get("timestamp")),
        native_id: Some(id.to_string()),
        event_type: EventType::Message,
        ..Default::default()
    };
    d.metadata.insert("source_line".into(), json!(line));
    copy(
        &mut d,
        obj,
        &[
            ("turn_id", "turn_id"),
            ("history_artifact", "history_artifact"),
        ],
    );
    copy(
        &mut d,
        msg,
        &[
            ("hostMetadata", "host_metadata"),
            ("canonicalTextRange", "canonical_text_range"),
        ],
    );
    convert_message(&mut d, msg)?;
    Ok(d)
}

/// Files under a MiniMax store that are not conversation records, with the
/// reason: `(format label, reason)`. Path-based and only for the app's own
/// directories (`~/.minimax`, `~/.minimax-code`).
pub fn known_unsupported(probe: &Probe) -> Option<(&'static str, &'static str)> {
    let p = Path::new(&probe.full_path);
    let comps: Vec<&str> = p
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if comps.contains(&".minimax-code") {
        return Some((
            "minimax-install",
            "MiniMax Code installation (release binaries, launchers, locks); not user data",
        ));
    }
    let at = comps.iter().position(|c| *c == ".minimax")?;
    let rest = &comps[at + 1..];
    let name = *rest.last()?;
    let under = |d: &str| rest.contains(&d);
    Some(match rest.first().copied()? {
        "v2" if under("sessions") && under("tool-outputs") => (
            "minimax-tool-output",
            "tool output spilled to its own file; the conversation holds the tool call and a reference, this is the full output blob",
        ),
        "v2" if under("sessions") => (
            "minimax-session-metadata",
            "per-session bookkeeping (manifest, llm-call config, history catalog, message locators, environment snapshot); no messages",
        ),
        "v2" if under("sqlite") => (
            "minimax-runtime-sqlite",
            "runtime state database indexing the session files (its message rows mirror messages.jsonl); the JSONL files are imported instead",
        ),
        "v2" if under("observability") => (
            "minimax-runtime-log",
            "runtime diagnostic log, not conversation content",
        ),
        "v2" if under("mcode") => (
            "minimax-draft",
            "unsent composer draft state, not a conversation record",
        ),
        "v2" => (
            "minimax-runtime-state",
            "runtime bookkeeping (leases, migrations, plugin caches); not conversation content",
        ),
        "background-tasks" => (
            "minimax-background-task",
            "captured stdout/summary of a background shell task; not a conversation record",
        ),
        ".builtin-skills" | "plugins" | "integrations" | "bin" | "shims" => (
            "minimax-install",
            "bundled skills, plugins and launchers; application files, not user data",
        ),
        _ if name.ends_with(".yaml") || name.ends_with(".json") => (
            "minimax-config",
            "application configuration, identity or model cache; not conversation content",
        ),
        _ => (
            "minimax-other",
            "MiniMax application file that is not a conversation record",
        ),
    })
}
