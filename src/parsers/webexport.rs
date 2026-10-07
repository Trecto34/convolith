//! Shared helpers for the official web-chat export parsers (ChatGPT, Claude,
//! Gemini/Takeout, Perplexity) and the inventory of the files that ship next to
//! the conversations in those exports.
//!
//! Exports are read straight from the files the provider hands out: no network,
//! no browser. Zips are expanded by discovery (with its path-traversal and
//! zip-bomb limits), so every member arrives here as an ordinary [`Probe`].

use crate::source::Probe;
use crate::timeutil::{parse_rfc3339, Stamp, TimestampConfidence, Utc};
use anyhow::{Context, Result};
use serde::de::{SeqAccess, Visitor};
use serde_json::Value;
use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Stream the elements of a top-level JSON array one at a time. A syntax error
/// in the middle surfaces as `Err` after the earlier elements were delivered.
pub fn stream_array(path: &Path, what: &str, f: &mut dyn FnMut(Value) -> Result<()>) -> Result<()> {
    struct V<'a>(&'a mut dyn FnMut(Value) -> Result<()>);
    impl<'de> Visitor<'de> for V<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
            while let Some(v) = seq.next_element::<Value>()? {
                (self.0)(v).map_err(serde::de::Error::custom)?;
            }
            Ok(())
        }
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    serde::Deserializer::deserialize_seq(
        &mut serde_json::Deserializer::from_reader(BufReader::new(file)),
        V(f),
    )
    .with_context(|| format!("reading top-level {what} array"))
}

/// RFC 3339 string -> stamp; an unparsable string is kept verbatim as the
/// original with no instant (never guessed).
pub fn iso_stamp(v: Option<&Value>) -> Stamp {
    let Some(s) = v.and_then(Value::as_str).filter(|s| !s.trim().is_empty()) else {
        return Stamp::unknown();
    };
    match parse_rfc3339(s) {
        Some(u) => Stamp::from_utc(u, TimestampConfidence::Exact, Some(s.to_string())),
        None => Stamp {
            utc: None,
            original: Some(s.to_string()),
            confidence: TimestampConfidence::Unknown,
        },
    }
}

/// Epoch-seconds number (fractional allowed) -> stamp.
pub fn epoch_stamp(v: Option<&Value>) -> Stamp {
    match v.and_then(Value::as_f64).filter(|t| t.is_finite()) {
        Some(t) => Stamp::from_utc(
            // microsecond rounding: f64 cannot carry more at epoch magnitude
            Utc(t.floor() as i64 * 1_000_000_000 + ((t - t.floor()) * 1e6).round() as i64 * 1000),
            TimestampConfidence::ProviderDerived,
            Some(v.map(Value::to_string).unwrap_or_default()),
        ),
        None => Stamp::unknown(),
    }
}

/// A provider id usable as a native identity only when it is globally unique
/// (UUID / long opaque token). Short ids like `msg_1` would otherwise collapse
/// unrelated conversations into one, so they are kept in metadata instead.
pub fn unique_id(id: Option<&str>) -> Option<String> {
    id.filter(|s| crate::dedup::is_globally_unique(s))
        .map(str::to_owned)
}

pub fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn in_export(p: &Probe) -> bool {
    !p.rel_path
        .replace('\\', "/")
        .trim_matches('/')
        .contains('/')
}

/// `conversations.json` or the sharded `conversations-NNN.json`.
pub fn is_conversations_name(name: &str) -> bool {
    name == "conversations.json"
        || name
            .strip_prefix("conversations-")
            .and_then(|r| r.strip_suffix(".json"))
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Files of the web exports that are inventoried, with a reason, instead of
/// imported. Consulted only after no parser claimed the file.
pub fn known_unsupported(p: &Probe) -> Option<(&'static str, &'static str)> {
    let name = p.filename().to_ascii_lowercase();
    let path = p.full_path.replace('\\', "/");
    let lower = path.to_ascii_lowercase();
    if [".xlsx!/", ".pptx!/", ".docx!/"]
        .iter()
        .any(|e| lower.contains(e))
    {
        return Some((
            "office-document-part",
            "internal part of an Office document found inside an export; the document itself is not conversation history",
        ));
    }
    if path.contains("/NotebookLM/") {
        return Some((
            "takeout-notebooklm",
            "Google Takeout NotebookLM notebook/source data; not a chat export",
        ));
    }
    if name.starts_with("gemini_") && name.ends_with(".html") {
        return Some((
            "takeout-gemini-sidecar",
            "Google Takeout Gemini Gems / scheduled-actions listing; not conversation history",
        ));
    }
    if name.starts_with("conversation_") && name.contains("_turn_") && name.contains("_images_") {
        return Some((
            "gemini-takeout-image",
            "image referenced by a Gemini Takeout conversation turn; the conversation keeps the reference, the bytes are not imported",
        ));
    }
    if name == "myactivity.html" && lower.contains("gemini") {
        return Some((
            "gemini-myactivity-html",
            "Gemini My Activity HTML export; request the JSON format in Takeout (only JSON is parsed)",
        ));
    }
    if let Some(r) = super::perplexity::known_unsupported(p) {
        return Some(r);
    }
    if is_conversations_name(&name) {
        return Some((
            "web-export-unrecognized-schema",
            "conversations.json matches neither the ChatGPT (mapping) nor the Claude (chat_messages) export schema; export version unknown",
        ));
    }
    let rel = p.rel_path.replace('\\', "/");
    let head_has = |k: &str| p.head.contains(k);
    if name.starts_with("manifest-") && name.ends_with(".json") && head_has("\"data_files\"") {
        return Some((
            "claude-export-manifest",
            "Claude export manifest listing the split data parts; not conversation history",
        ));
    }
    if rel.starts_with("memories/") && head_has("\"memory_files\"") {
        return Some((
            "claude-export-memories",
            "Claude memory files; not conversation history",
        ));
    }
    if rel.starts_with("projects/") && head_has("\"prompt_template\"") {
        return Some((
            "claude-export-projects",
            "Claude project definitions and knowledge documents; not conversation history",
        ));
    }
    if rel.starts_with("reflections/") && head_has("\"reflections\"") {
        return Some((
            "claude-export-feedback",
            "Claude feedback/reflection records; not conversation history",
        ));
    }
    if rel.starts_with("artifacts/") && (name == "artifact.json" || p.ext() == "html") {
        return Some((
            "claude-export-frames",
            "Claude design artifact/frame versions (generated documents); not conversation history",
        ));
    }
    if name == "login_history.json" && head_has("\"login_events\"") {
        return Some((
            "claude-export-metadata",
            "Claude login history; account metadata, deliberately not imported",
        ));
    }
    if !in_export(p) {
        return None;
    }
    match name.as_str() {
        "chat.html" => Some((
            "chatgpt-export-sidecar",
            "ChatGPT chat.html is a rendered copy of conversations.json",
        )),
        "message_feedback.json" | "shared_conversations.json" | "model_comparisons.json" => Some((
            "chatgpt-export-sidecar",
            "ChatGPT export side file (feedback/sharing); not conversation history",
        )),
        "user.json" => Some((
            "chatgpt-export-sidecar",
            "ChatGPT account file; deliberately not imported (account PII)",
        )),
        "projects.json" | "users.json" | "memories.json" => Some((
            "claude-export-sidecar",
            "Claude export project/account file; not conversation history (account PII)",
        )),
        n if (n.starts_with("file-") || n.starts_with("file_"))
            && matches!(
                p.ext().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "pdf" | "txt" | "wav" | "mp3"
            ) =>
        {
            Some((
                "chatgpt-export-attachment",
                "attachment bytes shipped in a ChatGPT export; conversations keep the file reference, the bytes are not imported",
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_names() {
        assert!(is_conversations_name("conversations.json"));
        assert!(is_conversations_name("conversations-012.json"));
        assert!(!is_conversations_name("conversations-x.json"));
        assert!(!is_conversations_name("conversations-.json"));
    }

    #[test]
    fn stamps_never_invent() {
        assert!(iso_stamp(Some(&Value::from("not a time"))).utc.is_none());
        assert!(
            iso_stamp(Some(&Value::from("2026-08-09T17:29:41.209311+00:00")))
                .utc
                .is_some()
        );
        assert!(epoch_stamp(Some(&Value::Null)).utc.is_none());
        assert_eq!(unique_id(Some("msg_1")), None);
        assert!(unique_id(Some("9a1f4c2e-3b6d-4e8a-9c1f-2d5b7a8e0f31")).is_some());
    }
}
