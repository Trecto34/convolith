//! Canonical event model (schema version 1).
//!
//! `spec/` documents this model independently of the implementation; these
//! types are the Rust mirror of `spec/*.schema.json`.

use crate::timeutil::{Stamp, TimestampConfidence};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const SCHEMA_VERSION: u32 = 1;
/// Bumped whenever a parser changes the canonical output it produces; part of
/// the resume key and of every provenance observation.
pub const PARSER_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
    Developer,
    Other,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
            Role::Developer => "developer",
            Role::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Role {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" | "human" => Role::User,
            "assistant" | "ai" | "model" | "bot" => Role::Assistant,
            "system" => Role::System,
            "tool" | "tool_result" | "function" => Role::Tool,
            "developer" => Role::Developer,
            _ => Role::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Message,
    Reasoning,
    ToolCall,
    ToolResult,
    Compaction,
    SystemNote,
    Attachment,
    AgentSpawn,
    AgentResult,
    Interruption,
    Error,
    Opaque,
    Control,
}

/// Only reasoning material that is literally present in the source is ever
/// emitted. Encrypted or vendor-hidden chain-of-thought is kept as an opaque
/// reference, never decoded and never invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningVisibility {
    /// Plain text reasoning stored by the provider.
    Public,
    /// A provider-authored summary of reasoning.
    Summary,
    /// Opaque/encrypted blob; content intentionally not interpreted.
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        truncation: Option<Truncation>,
    },
    Reasoning {
        text: String,
        visibility: ReasoningVisibility,
    },
    ToolCall {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        id: Option<String>,
        name: String,
        arguments: Value,
    },
    ToolResult {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tool_call_id: Option<String>,
        output: Value,
        is_error: bool,
    },
    Image {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        artifact: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        filename: Option<String>,
        /// Provider-side reference when the bytes are not present locally.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        source_ref: Option<String>,
    },
    FileRef {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        artifact: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        filename: Option<String>,
    },
    Artifact {
        artifact: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        filename: Option<String>,
        size: u64,
    },
    Data {
        value: Value,
    },
    /// Source material we deliberately do not interpret (unknown block types,
    /// encrypted payloads, unsupported shapes) — preserved verbatim.
    Opaque {
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        note: Option<String>,
        #[serde(
            skip_serializing_if = "Option::is_none",
            default,
            deserialize_with = "present_value"
        )]
        raw: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Truncation {
    /// SHA-256 of the full text after secret redaction, matching the stored artifact.
    pub full_sha256: String,
    pub full_bytes: u64,
    pub inline_bytes: u64,
    pub artifact: String,
}

impl Part {
    pub fn text(s: impl Into<String>) -> Part {
        Part::Text {
            text: s.into(),
            truncation: None,
        }
    }
    /// Concatenated searchable text of this part (used by the FTS index and by
    /// content fingerprints). `None` for parts with no text.
    pub fn as_text(&self) -> Option<String> {
        match self {
            Part::Text { text, .. } | Part::Reasoning { text, .. } => Some(text.clone()),
            Part::ToolCall {
                name, arguments, ..
            } => Some(format!("{name} {}", compact_json(arguments))),
            Part::ToolResult { output, .. } => Some(compact_json(output)),
            Part::Opaque { kind, note, .. } => {
                Some(format!("[{kind}] {}", note.clone().unwrap_or_default()))
            }
            _ => None,
        }
    }
}

pub fn compact_json(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// A provenance observation: which source record produced or corroborated an
/// event. Every canonical event carries at least one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceRef {
    pub source_id: String,
    /// Path as discovered, before any normalization.
    pub source_path: String,
    /// Full archive ancestry, outermost first, e.g.
    /// `["server-backup.tar.gz", "home/user/.codex/session.jsonl"]`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub container_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub record_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub record_id: Option<String>,
    pub parser: String,
    pub parser_version: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_sha256: Option<String>,
    pub first_seen: String,
    pub import_run: String,
    /// Which identity tier established this observation.
    pub identity_tier: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redaction {
    pub kind: String,
    pub count: u32,
    /// Where it was applied, e.g. `content[0].text`.
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: String,
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_event_id: Option<String>,
    /// Zero-based order of this event inside its conversation as observed.
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timestamp_original: Option<String>,
    pub timestamp_confidence: TimestampConfidence,
    pub role: Role,
    pub event_type: EventType,
    pub provider: String,
    pub application: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repository_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worktree_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub content: Vec<Part>,
    /// Structured, non-content metadata preserved from the source.
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    pub metadata: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub redactions: Vec<Redaction>,
    pub provenance: Vec<ProvenanceRef>,
}

impl Event {
    pub fn text_bytes(&self) -> usize {
        self.content
            .iter()
            .filter_map(|p| p.as_text())
            .map(|t| t.len())
            .sum()
    }
    pub fn preview(&self, max: usize) -> String {
        let mut out = String::new();
        for p in &self.content {
            if let Some(t) = p.as_text() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&t);
                if out.len() >= max {
                    break;
                }
            }
        }
        let out: String = out.chars().take(max).collect();
        out.replace('\n', " ")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub schema_version: u32,
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    pub provider: String,
    pub application: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repository_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ended_at: Option<String>,
    pub event_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub source_ids: Vec<String>,
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,
    pub session_id: String,
    pub conversation_id: String,
    pub provider: String,
    pub application: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ended_at: Option<String>,
    pub event_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub machine_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub source_ids: Vec<String>,
    pub event_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repository_id: Option<String>,
    pub name: String,
    /// Every historical path spelling that maps to this project.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub machines: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub branches: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence: Vec<String>,
    pub event_count: u64,
}

/// One immutable stored artifact (content-addressed by SHA-256).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub schema_version: u32,
    pub artifact_id: String,
    pub sha256: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub event_ids: Vec<String>,
    /// Relative path inside the dataset.
    pub stored_path: String,
    /// False when the payload exceeded the artifact size cap: hash and size
    /// are recorded, the bytes are not in the store.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub copied: bool,
}

/// `Some(Value::Null)` must survive a round trip: a present `null` is data,
/// an absent key is `None`.
fn present_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

fn default_true() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStats {
    pub import_run: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub files_discovered: u64,
    pub files_skipped: u64,
    pub candidate_sources: u64,
    pub supported_sources: u64,
    pub unsupported_sources: u64,
    pub sources_skipped: u64,
    pub sources_failed: u64,
    pub conversations: u64,
    pub sessions: u64,
    pub events_total: u64,
    pub events_new: u64,
    pub events_duplicate: u64,
    pub tool_calls: u64,
    pub artifacts: u64,
    pub conflicts: u64,
    pub parse_errors: u64,
    pub bytes_hashed: u64,
    /// Record-level accounting, so `examined = imported + duplicate + skipped +
    /// failed` can be checked by `validate`.
    pub records_examined: u64,
    pub records_imported: u64,
    pub records_duplicate: u64,
    pub records_skipped: u64,
    pub records_failed: u64,
}

impl RunStats {
    pub fn empty(run: &str, started_at: &str) -> RunStats {
        RunStats {
            import_run: run.to_string(),
            started_at: started_at.to_string(),
            ..Default::default()
        }
    }

    /// The accounting identity from spec §43, evaluated.
    pub fn accounting_holds(&self) -> bool {
        self.records_examined
            == self.records_imported
                + self.records_duplicate
                + self.records_skipped
                + self.records_failed
    }
}

/// Stamped timestamp helper used by parsers.
#[derive(Debug, Clone)]
pub struct EventDraft {
    pub role: Role,
    pub event_type: EventType,
    pub timestamp: Stamp,
    pub content: Vec<Part>,
    pub metadata: Map<String, Value>,
    /// Provider-native message/event id, when present.
    pub native_id: Option<String>,
    pub parent_native_id: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub tool_call_ids: Vec<String>,
}

impl Default for EventDraft {
    fn default() -> Self {
        EventDraft {
            role: Role::Other,
            event_type: EventType::Message,
            timestamp: Stamp::unknown(),
            content: Vec::new(),
            metadata: Map::new(),
            native_id: None,
            parent_native_id: None,
            model: None,
            agent: None,
            tool_call_ids: Vec::new(),
        }
    }
}

impl EventDraft {
    pub fn with_content(role: Role, event_type: EventType, content: Vec<Part>) -> Self {
        EventDraft {
            role,
            event_type,
            content,
            ..Default::default()
        }
    }
    /// Deterministic content fingerprint used by the dedup fallback tier.
    /// Never text alone: it is combined with provider/application/conversation,
    /// sequence, role and tool-call coordinates by the caller.
    pub fn content_fingerprint(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for p in &self.content {
            parts.push(match p {
                Part::Text { text, .. } => format!("t:{text}"),
                Part::Reasoning { text, visibility } => format!("r:{:?}:{text}", visibility),
                Part::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    format!(
                        "c:{}:{name}:{}",
                        id.clone().unwrap_or_default(),
                        compact_json(arguments)
                    )
                }
                Part::ToolResult {
                    tool_call_id,
                    output,
                    is_error,
                } => format!(
                    "o:{}:{is_error}:{}",
                    tool_call_id.clone().unwrap_or_default(),
                    compact_json(output)
                ),
                Part::Image {
                    artifact,
                    source_ref,
                    ..
                } => format!(
                    "i:{}:{}",
                    artifact.clone().unwrap_or_default(),
                    source_ref.clone().unwrap_or_default()
                ),
                Part::FileRef { path, artifact, .. } => format!(
                    "f:{}:{}",
                    path.clone().unwrap_or_default(),
                    artifact.clone().unwrap_or_default()
                ),
                Part::Artifact { artifact, size, .. } => format!("a:{artifact}:{size}"),
                Part::Data { value } => format!("d:{}", compact_json(value)),
                Part::Opaque { kind, raw, note } => format!(
                    "x:{kind}:{}:{}",
                    note.clone().unwrap_or_default(),
                    raw.as_ref().map(compact_json).unwrap_or_default()
                ),
            });
        }
        let refs: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
        crate::id::hex(&crate::id::hash_parts(&refs))
    }
}
