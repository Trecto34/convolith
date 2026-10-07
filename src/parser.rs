//! Parser interface.
//!
//! Adding a parser must not require touching the importer: register it in
//! [`crate::parsers::registry`] and the discovery/import pipeline picks it up.

use crate::model::EventDraft;
use crate::source::{Capabilities, Detection, ParseReport, Probe, Source};
use anyhow::Result;
use serde_json::{Map, Value};
use std::path::Path;

pub const PARSER_API_VERSION: &str = "1";

/// Metadata for one conversation discovered inside a source. A single source
/// (SQLite database, archive, export bundle) may hold many conversations.
#[derive(Debug, Clone, Default)]
pub struct ConversationMeta {
    /// Provider label for this conversation, when the parser knows it more
    /// precisely than its own `provider()` (e.g. a store shared by two apps).
    pub provider: Option<String>,
    /// Application label, same idea.
    pub application: Option<String>,
    /// Provider-native conversation/thread id when one exists.
    pub native_id: Option<String>,
    /// Provider-native session id when distinct from the conversation id.
    pub native_session_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub working_directory: Option<String>,
    pub branch: Option<String>,
    pub repository_root: Option<String>,
    pub git_remote: Option<String>,
    pub started_at: Option<crate::timeutil::Stamp>,
    pub ended_at: Option<crate::timeutil::Stamp>,
    /// Extra structured metadata preserved verbatim.
    pub metadata: Map<String, Value>,
    /// Identity tier the parser wants used for this conversation's events.
    /// Only `Native` changes behaviour: it asserts that the records carry
    /// provider-issued, globally unique ids, which lets the same event collapse
    /// across snapshots even when the conversation header is missing.
    pub identity_hint: IdentityHint,
}

impl ConversationMeta {
    pub fn provider_hint(&self) -> &str {
        self.provider.as_deref().unwrap_or("")
    }
    pub fn application_hint(&self) -> &str {
        self.application.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentityHint {
    /// Native, provider-issued, globally unique ids.
    Native,
    /// Stable coordinates (path + offset/index) but no provider id.
    #[allow(dead_code)]
    Coordinates,
    /// Nothing stable: use a conservative content fingerprint. This is the
    /// default because it is the tier that can never over-merge.
    #[default]
    Fingerprint,
}

/// Where a record came from inside its source; used for the provenance ledger
/// and for the coordinate identity tier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordCoords {
    pub source_id: String,
    pub record_index: Option<u64>,
    pub record_id: Option<String>,
    /// Database table / archive entry / JSONL line number, for the ledger.
    pub locator: Option<String>,
}

/// Receives parsed events. Implementations stream to disk; parsers must not
/// assume the sink buffers everything.
pub trait EventSink {
    fn begin(&mut self, meta: ConversationMeta) -> Result<()>;
    fn emit(&mut self, draft: EventDraft) -> Result<()>;
    fn end(&mut self) -> Result<()>;
}

/// Adapters and parsers use this to reach importer services (staging directory,
/// artifact store, secret policy, limits) without depending on the importer.
pub trait ParseContext {
    /// Directory parsers may write temporary files into. Owned by the importer.
    fn staging_dir(&self) -> &Path;
    fn machine_id(&self) -> Option<&str>;
    fn platform(&self) -> Option<&str>;
    fn import_run(&self) -> &str;
    /// Provider label of the parser currently running; parsers put this in
    /// source-side metadata without having to know who called them.
    fn provider(&self) -> &str;
    fn application(&self) -> &str;
    fn parser_id(&self) -> &str;
    fn parser_version(&self) -> &str;
    /// Longest text kept inline in an event; longer text goes to the artifact
    /// store and is referenced by hash.
    fn max_inline_text_bytes(&self) -> usize;
    /// Store bytes content-addressed; returns the artifact id.
    fn store_artifact(
        &mut self,
        bytes: &[u8],
        filename: Option<&str>,
        mime: Option<&str>,
        source_path: Option<&str>,
    ) -> Result<String>;
    /// Apply the configured secret policy to a text field. Returns the value to
    /// store plus the redaction record, if any.
    fn apply_secret_policy(
        &self,
        field: &str,
        text: &str,
    ) -> (String, Option<crate::secrets::RedactionHit>);
    fn max_record_bytes(&self) -> usize;
    /// Largest source a parser should read; the default is unbounded.
    fn max_file_bytes(&self) -> u64 {
        u64::MAX
    }
    fn note(&mut self, message: String);
}

pub trait SourceParser: Send + Sync {
    /// Stable parser id, e.g. `claude_code`.
    fn id(&self) -> &'static str;
    fn version(&self) -> &'static str {
        PARSER_API_VERSION
    }
    /// Human provider label, e.g. `anthropic`.
    fn provider(&self) -> &'static str;
    /// Human application label, e.g. `claude-code`.
    fn application(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// One-line description of what the parser reads, for `convolith parsers`.
    fn description(&self) -> &'static str;

    /// Cheap, side-effect-free detection. Must be conservative: a generic file
    /// name is not evidence.
    fn detect(&self, probe: &Probe) -> Detection;

    /// Parse one source. Must be streaming and must not mutate the source.
    fn parse(
        &self,
        ctx: &mut dyn ParseContext,
        source: &Source,
        sink: &mut dyn EventSink,
    ) -> Result<ParseReport>;
}

/// Registry ordering: highest-confidence detection wins; ties break on the
/// order parsers appear here (most specific first).
pub struct Registry {
    parsers: Vec<Box<dyn SourceParser>>,
}

impl Registry {
    pub fn new(parsers: Vec<Box<dyn SourceParser>>) -> Registry {
        Registry { parsers }
    }
    pub fn register(&mut self, parser: Box<dyn SourceParser>) {
        self.parsers.push(parser);
    }
    pub fn all(&self) -> &[Box<dyn SourceParser>] {
        &self.parsers
    }
    pub fn by_id(&self, id: &str) -> Option<&dyn SourceParser> {
        self.parsers
            .iter()
            .find(|p| p.id() == id)
            .map(|b| b.as_ref())
    }
    /// All detections for a probe, best first.
    pub fn detect_all(&self, probe: &Probe) -> Vec<Detection> {
        let mut hits: Vec<Detection> = self
            .parsers
            .iter()
            .map(|p| p.detect(probe))
            .filter(|d| d.is_hit())
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.confidence));
        hits
    }
    pub fn best(&self, probe: &Probe) -> Option<Detection> {
        self.detect_all(probe).into_iter().next()
    }
}
